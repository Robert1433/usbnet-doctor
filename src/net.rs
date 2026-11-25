//! Physical network adapters: link state and speed, traffic and error
//! counters, IP and gateway, driver, and the driver settings that matter for
//! flaky USB adapters (Energy-Efficient Ethernet, USB power saving).

use std::net::Ipv4Addr;

use crate::util::*;

#[derive(Debug, Clone)]
pub struct Setting {
    /// Windows driver keyword ("*EEE"); the Linux backend uses the same keys.
    pub key: String,
    pub name: String,
    pub value: String,
    pub raw: String,
}

#[derive(Debug, Clone)]
pub struct Adapter {
    pub alias: String,
    pub description: String,
    pub mac: String,
    pub wireless: bool,
    pub connected: bool,
    pub link_bps: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub in_errors: u64,
    pub out_errors: u64,
    pub in_discards: u64,
    pub out_discards: u64,
    pub ipv4: Vec<Ipv4Addr>,
    pub gateways: Vec<Ipv4Addr>,
    pub driver_version: Option<String>,
    /// Driver date (Windows) or firmware version (Linux).
    pub driver_note: Option<String>,
    /// Device id that `usb::UsbTree::locate` understands.
    pub instance: Option<String>,
    /// The adapter is a USB device.
    pub usb: bool,
    /// The hardware reports gigabit (or faster) link modes.
    pub gigabit_hint: bool,
    /// Driver name, used to pick its messages out of the system log.
    pub service: Option<String>,
    pub settings: Vec<Setting>,
}

impl Adapter {
    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key.eq_ignore_ascii_case(key))
    }
    pub fn is_usb(&self) -> bool {
        self.usb
    }
    pub fn gigabit_capable(&self) -> bool {
        let d = self.description.to_lowercase();
        d.contains("gigabit") || d.contains("gbe") || d.contains("2.5g") || d.contains("1000") || self.link_bps >= 1_000_000_000 || self.gigabit_hint
    }
    pub fn fast_ethernet_only(&self) -> bool {
        self.description.to_lowercase().contains("fast ethernet")
    }
}

pub fn scan() -> Vec<Adapter> {
    let mut out = crate::sys::net::adapters();
    out.sort_by_key(|a| (!a.connected, a.wireless, a.alias.clone()));
    out
}

/// Settings worth showing in the summary; the rest only with --verbose.
const KEY_SETTINGS: &[&str] = &[
    "*SpeedDuplex",
    "*EEE",
    "EEE",
    "GreenEthernet",
    "*SelectiveSuspend",
    "*SSIdleTimeout",
    "*FlowControl",
    "*JumboPacket",
];

pub fn print(adapters: &[Adapter], usb: Option<&crate::usb::UsbTree>, verbose: bool) {
    heading("Network adapters");
    if adapters.is_empty() {
        println!("{}", dim("No physical network adapters found."));
    }
    for a in adapters {
        let state = if a.connected && a.link_bps > 0 {
            green(format!("connected, link {}", fmt_bps(a.link_bps as f64)))
        } else if a.connected {
            green("connected")
        } else {
            dim("disconnected")
        };
        println!();
        println!("{} {}  {}", bold(&a.alias), dim(format!("({})", a.description)), state);
        println!("  MAC {}{}", a.mac, if a.wireless { "  · Wi-Fi" } else { "" });
        if !a.ipv4.is_empty() {
            let ips: Vec<String> = a.ipv4.iter().map(|i| i.to_string()).collect();
            let gws: Vec<String> = a.gateways.iter().map(|i| i.to_string()).collect();
            println!(
                "  IPv4 {}{}",
                ips.join(", "),
                if gws.is_empty() { String::new() } else { format!("  gateway {}", gws.join(", ")) }
            );
        }
        if let (Some(v), d) = (&a.driver_version, &a.driver_note) {
            println!("  Driver {v}{}", d.as_ref().map(|d| format!(" ({d})")).unwrap_or_default());
        }
        let errs = a.in_errors + a.out_errors + a.in_discards + a.out_discards;
        let counters = format!(
            "  Traffic ↓ {} ↑ {}   errors in/out {}/{}   discards in/out {}/{}",
            fmt_bytes(a.in_bytes),
            fmt_bytes(a.out_bytes),
            a.in_errors,
            a.out_errors,
            a.in_discards,
            a.out_discards
        );
        println!("{}", if errs > 0 { yellow(counters) } else { counters });

        if let (Some(inst), Some(usb)) = (&a.instance, usb) {
            if a.is_usb() {
                match usb.locate(inst) {
                    Some(l) => {
                        let speed = if l.superspeed { green(&l.speed) } else { yellow(&l.speed) };
                        println!("  USB  {}  via {}", speed, l.chain.join(" → "));
                        if let Some(t) = last_arrival(inst) {
                            println!("  {}", dim(format!("Plugged in at {}", t.local_string())));
                        }
                    }
                    None => println!("  USB  {}", dim("location not found")),
                }
            }
        }

        let shown: Vec<&Setting> = if verbose {
            a.settings.iter().collect()
        } else {
            KEY_SETTINGS.iter().filter_map(|k| a.setting(k)).collect()
        };
        if !shown.is_empty() {
            let parts: Vec<String> = shown.iter().map(|s| format!("{}: {}", s.name, s.value)).collect();
            if verbose {
                println!("  Advanced settings:");
                for p in parts {
                    println!("    {p}");
                }
            } else {
                println!("  {}", dim(parts.join(" · ")));
            }
        }
    }
}

/// True when a toggle-style driver setting is on ("1", or a display value like "Enabled").
pub fn is_on(s: &Setting) -> bool {
    s.raw == "1" || s.value.eq_ignore_ascii_case("enabled") || s.value.eq_ignore_ascii_case("on")
}
