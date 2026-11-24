//! Event log scan: network-driver and USB-stack events from the System log,
//! plus network connect/disconnect events from NetworkProfile.

use std::collections::HashMap;
use std::ptr::{null, null_mut};


use windows_sys::Win32::System::EventLog::{
    EvtClose, EvtFormatMessage, EvtNext, EvtOpenPublisherMetadata, EvtQuery, EvtRender,
};

use crate::util::*;

const EVT_QUERY_CHANNEL_PATH: u32 = 0x1;
const EVT_RENDER_EVENT_XML: u32 = 1;
const EVT_FORMAT_MESSAGE_EVENT: u32 = 1;

/// Provider names containing any of these are network-adapter or USB-stack sources.
const PROVIDER_KEYWORDS: &[&str] = &[
    "ax88", "asix", "rtux", "rtl", "realtek", "usbeth", "ndis", "netadaptercx", "usbhub", "usbxhci", "ucx01000",
    "usbport", "netwtw", "mtkwl", "e1d", "e1r", "e2f", "b57nd",
];

const PROBLEM_WORDS: &[&str] = &[
    "fail", "error", "reset", "timeout", "timed out", "stall", "not enough power", "overcurrent", "surprise",
    "disconnect", "unresponsive", "hang",
];

/// Messages that contain a problem word but are routine notices.
const NOT_PROBLEMS: &[&str] = &["draining system power"];

/// USB-stack providers; everything else we collect is a network-driver provider.
const USB_STACK: &[&str] = &["usbhub", "usbxhci", "ucx01000", "usbport"];

#[derive(Debug, Clone)]
pub struct Event {
    pub time: Stamp,
    pub provider: String,
    pub id: u32,
    pub level: u32,
    pub text: String,
}

impl Event {
    pub fn is_problem(&self) -> bool {
        let t = self.text.to_lowercase();
        if NOT_PROBLEMS.iter().any(|w| t.contains(w)) {
            return false;
        }
        (1..=2).contains(&self.level) || PROBLEM_WORDS.iter().any(|w| t.contains(w))
    }

    pub fn is_network_driver(&self) -> bool {
        let p = self.provider.to_lowercase();
        !USB_STACK.iter().any(|k| p.contains(k))
    }
}

/// Repeats of the same message from the same provider, folded into one line.
#[derive(Debug, Clone)]
pub struct Group {
    pub provider: String,
    pub id: u32,
    pub text: String,
    pub count: usize,
    pub first: Stamp,
    pub last: Stamp,
    pub problem: bool,
    pub network_driver: bool,
}

fn short_provider(p: &str) -> &str {
    p.strip_prefix("Microsoft-Windows-").unwrap_or(p)
}

fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct NetChange {
    pub time: Stamp,
    pub connected: bool,
    pub network: String,
}

pub struct EventReport {
    pub days: u32,
    pub driver_events: Vec<Event>,
    pub net_changes: Vec<NetChange>,
}

struct Handle(isize);
impl Drop for Handle {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { EvtClose(self.0) };
        }
    }
}

fn render_xml(ev: isize) -> Option<String> {
    let mut used = 0u32;
    let mut count = 0u32;
    unsafe {
        EvtRender(0, ev, EVT_RENDER_EVENT_XML, 0, null_mut(), &mut used, &mut count);
        if used == 0 {
            return None;
        }
        let mut buf = vec![0u16; used as usize / 2 + 1];
        if EvtRender(0, ev, EVT_RENDER_EVENT_XML, (buf.len() * 2) as u32, buf.as_mut_ptr() as *mut _, &mut used, &mut count)
            == 0
        {
            return None;
        }
        Some(from_wide(&buf))
    }
}

fn format_message(meta: isize, ev: isize) -> Option<String> {
    if meta == 0 {
        return None;
    }
    let mut used = 0u32;
    unsafe {
        EvtFormatMessage(meta, ev, 0, 0, null(), EVT_FORMAT_MESSAGE_EVENT, 0, null_mut(), &mut used);
        if used == 0 {
            return None;
        }
        let mut buf = vec![0u16; used as usize + 1];
        if EvtFormatMessage(meta, ev, 0, 0, null(), EVT_FORMAT_MESSAGE_EVENT, buf.len() as u32, buf.as_mut_ptr(), &mut used)
            == 0
        {
            return None;
        }
        let s = from_wide(&buf);
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        (!s.is_empty()).then_some(s)
    }
}

/// Runs an XPath query on a channel and calls `f` with each event handle and its XML.
fn query(channel: &str, xpath: &str, mut f: impl FnMut(isize, &str)) {
    let ch = wide(channel);
    let q = wide(xpath);
    let rs = Handle(unsafe { EvtQuery(0, ch.as_ptr(), q.as_ptr(), EVT_QUERY_CHANNEL_PATH) });
    if rs.0 == 0 {
        return;
    }
    let mut batch = [0isize; 64];
    loop {
        let mut got = 0u32;
        let ok = unsafe { EvtNext(rs.0, batch.len() as u32, batch.as_mut_ptr(), 5000, 0, &mut got) };
        if ok == 0 || got == 0 {
            break;
        }
        for &ev in &batch[..got as usize] {
            let ev = Handle(ev);
            if let Some(xml) = render_xml(ev.0) {
                f(ev.0, &xml);
            }
        }
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn attr(xml: &str, elem: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{elem} "))?;
    let rest = &xml[start..];
    let rest = &rest[..rest.find('>')?];
    for q in ['\'', '"'] {
        let pat = format!("{name}={q}");
        if let Some(i) = rest.find(&pat) {
            let v = &rest[i + pat.len()..];
            return Some(unescape(&v[..v.find(q)?]));
        }
    }
    None
}

fn elem_text(xml: &str, elem: &str) -> Option<String> {
    let start = xml.find(&format!("<{elem}"))?;
    let rest = &xml[start..];
    let gt = rest.find('>')?;
    if rest[..gt].ends_with('/') {
        return Some(String::new());
    }
    let body = &rest[gt + 1..];
    Some(unescape(&body[..body.find('<')?]))
}

/// All <Data> values, in order, as (name, value).
fn data_items(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<Data") {
        rest = &rest[i + 5..];
        let Some(gt) = rest.find('>') else { break };
        let head = &rest[..gt];
        if !(head.is_empty() || head.starts_with(' ') || head.starts_with('/')) {
            continue;
        }
        let name = attr(&format!("<Data {head}>"), "Data", "Name").unwrap_or_default();
        if head.ends_with('/') {
            out.push((name, String::new()));
            rest = &rest[gt + 1..];
            continue;
        }
        let body = &rest[gt + 1..];
        let end = body.find("</Data>").unwrap_or(body.len());
        out.push((name, unescape(&body[..end])));
        rest = &body[end..];
    }
    out
}

fn parse_time(xml: &str) -> Option<Stamp> {
    let t = attr(xml, "TimeCreated", "SystemTime")?;
    Stamp::parse_utc(&t)
}

/// A readable line from raw event data when the provider has no message file:
/// keep values with letters in them, drop runs of numeric padding.
fn data_text(xml: &str) -> String {
    let items = data_items(xml);
    let words: Vec<String> = items
        .iter()
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| v.chars().any(|c| c.is_alphabetic()))
        .collect();
    if !words.is_empty() {
        return words.join(" | ");
    }
    items.iter().map(|(_, v)| v.trim()).filter(|v| !v.is_empty()).take(6).collect::<Vec<_>>().join(" ")
}

pub fn scan(days: u32, extra_providers: &[String]) -> EventReport {
    let ms = days as u64 * 86_400_000;
    let keywords: Vec<String> = PROVIDER_KEYWORDS
        .iter()
        .map(|s| s.to_string())
        .chain(extra_providers.iter().map(|s| s.to_lowercase()))
        .collect();

    let mut meta: HashMap<String, Handle> = HashMap::new();
    let mut driver_events = Vec::new();
    query("System", &format!("*[System[TimeCreated[timediff(@SystemTime) <= {ms}]]]"), |ev, xml| {
        let Some(provider) = attr(xml, "Provider", "Name") else { return };
        let pl = provider.to_lowercase();
        if !keywords.iter().any(|k| pl.contains(k.as_str())) {
            return;
        }
        let Some(time) = parse_time(xml) else { return };
        let m = meta.entry(provider.clone()).or_insert_with(|| {
            let w = wide(&provider);
            Handle(unsafe { EvtOpenPublisherMetadata(0, w.as_ptr(), null(), 0, 0) })
        });
        let text = format_message(m.0, ev).unwrap_or_else(|| data_text(xml));
        driver_events.push(Event {
            time,
            provider,
            id: elem_text(xml, "EventID").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            level: elem_text(xml, "Level").and_then(|s| s.trim().parse().ok()).unwrap_or(0),
            text,
        });
    });
    drop(meta);
    driver_events.sort_by_key(|e| e.time);

    let mut net_changes = Vec::new();
    query(
        "Microsoft-Windows-NetworkProfile/Operational",
        &format!("*[System[(EventID=10000 or EventID=10001) and TimeCreated[timediff(@SystemTime) <= {ms}]]]"),
        |_, xml| {
            let Some(time) = parse_time(xml) else { return };
            let id: u32 = elem_text(xml, "EventID").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
            let data = data_items(xml);
            let network = data.iter().find(|(n, _)| n == "Name").map(|(_, v)| v.clone()).unwrap_or_default();
            if network == "Identifying..." {
                return;
            }
            net_changes.push(NetChange { time, connected: id == 10000, network });
        },
    );
    net_changes.sort_by_key(|e| e.time);

    EventReport { days, driver_events, net_changes }
}

impl EventReport {
    pub fn disconnects(&self) -> usize {
        self.net_changes.iter().filter(|c| !c.connected).count()
    }

    /// Events folded by (provider, message), most recent first.
    pub fn groups(&self) -> Vec<Group> {
        let mut map: HashMap<(String, String), Group> = HashMap::new();
        for e in &self.driver_events {
            let key = (e.provider.clone(), clip(&e.text, 100));
            let g = map.entry(key).or_insert_with(|| Group {
                provider: e.provider.clone(),
                id: e.id,
                text: e.text.clone(),
                count: 0,
                first: e.time,
                last: e.time,
                problem: e.is_problem(),
                network_driver: e.is_network_driver(),
            });
            g.count += 1;
            g.first = g.first.min(e.time);
            g.last = g.last.max(e.time);
        }
        let mut v: Vec<Group> = map.into_values().collect();
        v.sort_by(|a, b| b.last.cmp(&a.last));
        v
    }

    pub fn print(&self, limit: usize) {
        heading(&format!("Event log (last {} days)", self.days));
        let groups = self.groups();
        let problems: usize = groups.iter().filter(|g| g.problem).map(|g| g.count).sum();
        println!(
            "Network/USB driver events: {} total, {} look like problems",
            self.driver_events.len(),
            if problems == 0 { green(0) } else { red(problems) }
        );
        for g in groups.iter().take(limit) {
            let when = if g.count == 1 {
                g.last.local_string()
            } else {
                format!("{} … {}", g.first.short(), g.last.short())
            };
            let count = format!("{:>4}×", g.count);
            let provider = format!("{} {}", short_provider(&g.provider), dim(format!("#{}", g.id)));
            let text = clip(&g.text, 110);
            println!(
                "  {}  {}  {}",
                if g.problem { red(count) } else { dim(count) },
                if g.problem { yellow(provider) } else { provider },
                text
            );
            println!("         {}", dim(when));
        }
        if groups.len() > limit {
            println!("  {}", dim(format!("… {} more kinds (use --events-shown)", groups.len() - limit)));
        }

        println!();
        println!("Network disconnects: {}", self.disconnects());
        for c in self.net_changes.iter().rev().take(limit).collect::<Vec<_>>().into_iter().rev() {
            let what = if c.connected { green("connected   ") } else { red("disconnected") };
            println!("  {}  {}  {}", dim(c.time.local_string()), what, c.network);
        }
    }
}
