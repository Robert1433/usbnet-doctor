//! System-log scan: network-driver and USB-stack messages, plus network
//! connects and disconnects. The backend reads the Windows event log or the
//! Linux kernel log; this module folds and judges the messages.

use std::collections::HashMap;

use crate::sys;
use crate::util::*;

pub const PROBLEM_WORDS: &[&str] = &[
    "fail", "error", "reset", "timeout", "timed out", "stall", "not enough power", "overcurrent", "over-current",
    "surprise", "disconnect", "unresponsive", "hang", "unable to", "cannot", "watchdog", "status -71", "-110",
];

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
        if sys::events::NOT_PROBLEMS.iter().any(|w| t.contains(w)) {
            return false;
        }
        (1..=2).contains(&self.level) || PROBLEM_WORDS.iter().any(|w| t.contains(w))
    }

    pub fn is_network_driver(&self) -> bool {
        let p = self.provider.to_lowercase();
        !sys::events::USB_STACK.iter().any(|k| sys::events::provider_matches(&p, k))
    }
}

/// Repeats of the same message from the same provider, folded into one line.
#[derive(Debug, Clone)]
pub struct Group {
    pub provider: String,
    pub id: u32,
    pub text: String,
    /// `fold_key` of the text: every event in the group has it.
    pub key: String,
    pub count: usize,
    pub first: Stamp,
    pub last: Stamp,
    pub problem: bool,
    pub network_driver: bool,
}

fn short_provider(p: &str) -> &str {
    p.strip_prefix("Microsoft-Windows-").unwrap_or(p)
}

/// Repeated messages that differ only in numbers ("device number 7") fold together.
pub fn fold_key(s: &str) -> String {
    clip(s, 100).chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect()
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
    pub notes: Vec<String>,
}

/// What a backend found in the system log.
#[derive(Default)]
pub struct Collected {
    pub driver_events: Vec<Event>,
    pub net_changes: Vec<NetChange>,
    /// Why the log may be incomplete (missing tool, no permission…).
    pub notes: Vec<String>,
}

pub fn scan(days: u32, extra_providers: &[String]) -> EventReport {
    let keywords: Vec<String> = sys::events::PROVIDER_KEYWORDS
        .iter()
        .map(|s| s.to_string())
        .chain(extra_providers.iter().map(|s| s.to_lowercase()))
        .collect();
    let mut c = sys::events::collect(days, &keywords);
    c.driver_events.sort_by_key(|e| e.time);
    c.net_changes.sort_by_key(|e| e.time);
    EventReport { days, driver_events: c.driver_events, net_changes: c.net_changes, notes: c.notes }
}

impl EventReport {
    pub fn disconnects(&self) -> usize {
        self.net_changes.iter().filter(|c| !c.connected).count()
    }

    /// Events folded by (provider, message), most recent first.
    pub fn groups(&self) -> Vec<Group> {
        let mut map: HashMap<(String, String), Group> = HashMap::new();
        for e in &self.driver_events {
            let key = (e.provider.clone(), fold_key(&e.text));
            let g = map.entry(key.clone()).or_insert_with(|| Group {
                key: key.1,
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
        v.sort_by_key(|g| std::cmp::Reverse(g.last));
        v
    }

    pub fn print(&self, limit: usize) {
        heading(&format!("{} (last {} days)", sys::events::LOG_NAME, self.days));
        for n in &self.notes {
            println!("{}", yellow(n));
        }
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
            let provider = if g.id == 0 {
                short_provider(&g.provider).to_string()
            } else {
                format!("{} {}", short_provider(&g.provider), dim(format!("#{}", g.id)))
            };
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
