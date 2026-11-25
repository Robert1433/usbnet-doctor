//! Linux system log: kernel messages from network drivers and the USB stack,
//! read from the systemd journal (or `dmesg` when there is no readable journal),
//! plus link up/down messages as network connects and disconnects.

use std::process::Command;

use crate::events::{Collected, Event, NetChange};
use crate::util::{Stamp, TICKS_PER_SEC};

/// Kernel log prefixes (driver names) of network-adapter and USB-stack messages.
pub const PROVIDER_KEYWORDS: &[&str] = &[
    "usb", "hub", "xhci", "ehci", "ohci", "uhci", "netdev", "r8152", "r8169", "r8125", "r8126", "ax88179", "ax88796",
    "asix", "cdc_ncm", "cdc_ether", "cdc_mbim", "rndis_host", "aqc111", "lan78xx", "smsc95xx", "e1000e", "igb", "igc",
    "tg3", "atlantic", "iwlwifi", "mt76", "mt79", "rtw88", "rtw89", "rtl8", "ath9k", "ath10k", "ath11k", "ath12k",
    "brcmfmac",
];

/// Keywords that must match the whole prefix ("usb", not "usbhid").
const EXACT: &[&str] = &["usb", "hub", "netdev"];

/// USB-stack providers; everything else we collect is a network-driver provider.
pub const USB_STACK: &[&str] = &["usb", "hub", "xhci", "ehci", "ohci", "uhci"];

/// Messages that contain a problem word but are routine notices.
pub const NOT_PROBLEMS: &[&str] = &["usb disconnect, device number"];

pub const LOG_NAME: &str = "Kernel log";

pub fn provider_matches(provider_lower: &str, keyword: &str) -> bool {
    provider_lower == keyword || (!EXACT.contains(&keyword) && provider_lower.starts_with(keyword))
}

/// A raw kernel log line.
struct Line {
    time: Stamp,
    /// 2 = error or worse, 3 = warning, 4 = anything else (Windows levels).
    level: u32,
    text: String,
}

fn level_of_priority(p: u32) -> u32 {
    match p {
        0..=3 => 2,
        4 => 3,
        _ => 4,
    }
}

/// The value of a string field in one line of `journalctl -o json`.
fn json_str(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len();
    let mut out = String::new();
    let mut chars = line[start..].chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => {}
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32).unwrap_or('?'));
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

enum Source {
    Lines(Vec<Line>),
    Failed(String),
}

fn journal(days: u32) -> Source {
    let out = Command::new("journalctl")
        // `_TRANSPORT=kernel` rather than `-k`, which would stop at the last boot.
        .args(["_TRANSPORT=kernel", "-q", "--no-pager", "-o", "json", "--output-fields=MESSAGE,PRIORITY"])
        .arg(format!("--since=-{days}d"))
        .output();
    let out = match out {
        Ok(o) => o,
        Err(_) => return Source::Failed("journalctl not found".into()),
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<Line> = stdout
        .lines()
        .filter_map(|l| {
            let us: u64 = json_str(l, "__REALTIME_TIMESTAMP")?.parse().ok()?;
            let prio: u32 = json_str(l, "PRIORITY").and_then(|p| p.parse().ok()).unwrap_or(6);
            Some(Line { time: Stamp::from_unix_micros(us), level: level_of_priority(prio), text: json_str(l, "MESSAGE")? })
        })
        .collect();
    if lines.is_empty() {
        let err = String::from_utf8_lossy(&out.stderr);
        let why = if err.contains("permission") || err.contains("not seeing messages") {
            "no permission to read the system journal"
        } else {
            "the journal has no kernel messages"
        };
        return Source::Failed(why.into());
    }
    Source::Lines(lines)
}

/// "2026-11-25T09:07:12,590563+01:00" -> UTC stamp.
fn parse_iso_local(ts: &str) -> Option<Stamp> {
    let base = Stamp::parse_utc(ts.get(..19)?)?;
    let rest = &ts[19..];
    let (frac, offset) = match rest.find(['+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let micros: u64 = frac.trim_start_matches([',', '.']).get(..6).and_then(|f| f.parse().ok()).unwrap_or(0);
    let mut ticks = base.0 as i64 + micros as i64 * 10;
    if offset.len() >= 6 {
        let sign = if offset.starts_with('-') { -1 } else { 1 };
        let h: i64 = offset[1..3].parse().ok()?;
        let m: i64 = offset[4..6].parse().ok()?;
        ticks -= sign * (h * 3600 + m * 60) * TICKS_PER_SEC as i64;
    }
    Some(Stamp(ticks as u64))
}

fn dmesg(days: u32) -> Source {
    let out = match Command::new("dmesg").args(["--time-format=iso", "--decode"]).output() {
        Ok(o) if o.status.success() => o,
        Ok(_) => return Source::Failed("dmesg is not allowed for this user".into()),
        Err(_) => return Source::Failed("dmesg not found".into()),
    };
    let since = Stamp(Stamp::now().0.saturating_sub(days as u64 * 86_400 * TICKS_PER_SEC));
    let lines = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let (facility, rest) = l.split_once(':')?;
            if facility.trim() != "kern" {
                return None;
            }
            let (level, rest) = rest.split_once(':')?;
            let (ts, text) = rest.trim_start().split_once(' ')?;
            let level = match level.trim() {
                "emerg" | "alert" | "crit" | "err" => 2,
                "warn" => 3,
                _ => 4,
            };
            Some(Line { time: parse_iso_local(ts)?, level, text: text.to_string() })
        })
        .filter(|l| l.time >= since)
        .collect();
    Source::Lines(lines)
}

/// "r8152 2-1.3:1.0 enx00e04c: carrier on" -> "r8152".
fn provider_of(text: &str) -> String {
    let first = text.split_whitespace().next().unwrap_or("");
    first.trim_end_matches(':').to_string()
}

/// The interface a link message is about: the last word before the first ": ".
fn interface_of(text: &str) -> String {
    let head = text.split(": ").next().unwrap_or("");
    head.split_whitespace().last().unwrap_or("").to_string()
}

/// Link up (true) or down (false), for messages that announce one.
fn link_change(text: &str) -> Option<bool> {
    let t = text.to_lowercase();
    const DOWN: &[&str] = &["link is down", "carrier off", "deauthenticating from", "deauthenticated from", "disassociated from"];
    const UP: &[&str] = &["link is up", "carrier on", ": associated"];
    if DOWN.iter().any(|w| t.contains(w)) {
        Some(false)
    } else if UP.iter().any(|w| t.contains(w)) {
        Some(true)
    } else {
        None
    }
}

pub fn collect(days: u32, keywords: &[String]) -> Collected {
    let mut c = Collected::default();
    let lines = match journal(days) {
        Source::Lines(l) => l,
        Source::Failed(why) => match dmesg(days) {
            Source::Lines(l) => {
                c.notes.push(format!("Using dmesg ({why}): only messages since the last boot."));
                l
            }
            Source::Failed(why2) => {
                c.notes.push(format!(
                    "Cannot read the kernel log ({why}; {why2}). Run with sudo, or add yourself to the systemd-journal or adm group."
                ));
                return c;
            }
        },
    };

    for l in lines {
        if let Some(up) = link_change(&l.text) {
            c.net_changes.push(NetChange { time: l.time, connected: up, network: interface_of(&l.text) });
        }
        // Serial numbers are nobody's business in a report.
        if l.text.contains("SerialNumber:") {
            continue;
        }
        let provider = provider_of(&l.text);
        let pl = provider.to_lowercase();
        if !keywords.iter().any(|k| provider_matches(&pl, k)) {
            continue;
        }
        let ev = Event { time: l.time, provider, id: 0, level: l.level, text: l.text };
        // The USB core logs every plug-in in detail; keep only what looks wrong.
        let chatty = USB_STACK.iter().any(|k| provider_matches(&pl, k));
        if chatty && !ev.is_problem() {
            continue;
        }
        c.driver_events.push(ev);
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_journal_json() {
        let l = r#"{"__CURSOR":"s=1","__REALTIME_TIMESTAMP":"1732540000123456","PRIORITY":"3","MESSAGE":"r8152 2-1:1.0 eth0: Tx status -71 \"x\""}"#;
        assert_eq!(json_str(l, "PRIORITY").as_deref(), Some("3"));
        assert_eq!(json_str(l, "MESSAGE").as_deref(), Some("r8152 2-1:1.0 eth0: Tx status -71 \"x\""));
        assert_eq!(json_str(l, "__REALTIME_TIMESTAMP").as_deref(), Some("1732540000123456"));
    }

    #[test]
    fn splits_kernel_messages() {
        let t = "e1000e 0000:00:1f.6 enp0s31f6: NIC Link is Up 1000 Mbps Full Duplex";
        assert_eq!(provider_of(t), "e1000e");
        assert_eq!(interface_of(t), "enp0s31f6");
        assert_eq!(link_change(t), Some(true));
        assert_eq!(link_change("wlp2s0: deauthenticating from aa:bb by local choice"), Some(false));
        assert_eq!(provider_of("usb 2-1: reset SuperSpeed USB device number 3 using xhci_hcd"), "usb");
    }

    #[test]
    fn matches_providers() {
        assert!(provider_matches("xhci_hcd", "xhci"));
        assert!(provider_matches("usb", "usb"));
        assert!(!provider_matches("usbhid", "usb"));
        assert!(provider_matches("ax88179_178a", "ax88179"));
    }

    #[test]
    fn parses_dmesg_time() {
        let a = parse_iso_local("1970-01-01T02:00:00,000000+02:00").unwrap();
        assert_eq!(a.unix_secs(), 0);
    }
}
