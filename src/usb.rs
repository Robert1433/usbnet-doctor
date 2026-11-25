//! USB topology: every hub, every port, what is plugged in and at what speed.
//!
//! The platform backend (`sys::usb`) lists the hubs and their ports. A USB 3
//! hub shows up as two hubs (a SuperSpeed one and its USB 2.0 companion), on
//! Windows and on Linux alike; this module merges each SuperSpeed port with
//! its USB 2.0 companion port so the tree shows physical connectors.

use std::collections::{HashMap, HashSet};

use crate::sys;
use crate::util::*;

/// Connection speed of a device, as the backend reports it.
pub const SPEED_LOW: u8 = 0;
pub const SPEED_FULL: u8 = 1;
pub const SPEED_HIGH: u8 = 2;
pub const SPEED_SUPER: u8 = 3;
pub const SPEED_SUPER_PLUS: u8 = 4;
pub const SPEED_SUPER_PLUS_2X2: u8 = 5;

#[derive(Debug, Clone)]
pub struct Connection {
    pub speed: u8,
    pub vid: u16,
    pub pid: u16,
    pub is_hub: bool,
    /// Device id the backend uses to find this device again (upper-cased).
    pub instance: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connector {
    /// A port the user can plug into (USB-A, or unknown shape on an external hub).
    External,
    TypeC,
    /// Wired to a built-in device (camera, Bluetooth, card reader…).
    Internal,
    /// The firmware does not say.
    Unknown,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub number: u32,
    pub connector: Connector,
    /// Companion port: (hub instance id, upper-cased; port number).
    pub companion: Option<(String, u32)>,
    pub usb2: bool,
    pub usb3: bool,
    /// Operating at SuperSpeed / SuperSpeed+ right now.
    pub op_ss: bool,
    pub op_ssp: bool,
    /// The device on this (USB 2.0) port could run at SuperSpeed.
    pub cap_ss: bool,
    /// A port error, in words ("not enough power for device").
    pub problem: Option<String>,
    pub conn: Option<Connection>,
}

impl Port {
    pub fn new(number: u32) -> Port {
        Port {
            number,
            connector: Connector::Unknown,
            companion: None,
            usb2: false,
            usb3: false,
            op_ss: false,
            op_ssp: false,
            cap_ss: false,
            problem: None,
            conn: None,
        }
    }
}

#[derive(Debug)]
pub struct Hub {
    pub instance: String,
    pub name: String,
    pub is_root: bool,
    pub bus_powered: bool,
    /// Host controller name, for root hubs.
    pub controller: Option<String>,
    pub ports: Vec<Port>,
}

/// Where a device sits in the USB tree.
#[derive(Debug, Clone)]
pub struct Location {
    pub chain: Vec<String>,
    pub external_hubs: usize,
    pub bus_powered_hub: bool,
    pub speed: String,
    pub superspeed: bool,
}

#[derive(Debug, Clone)]
pub struct UsbIssue {
    pub problem: bool,
    pub text: String,
}

pub struct UsbTree {
    pub lines: Vec<String>,
    pub locations: HashMap<String, Location>,
    pub issues: Vec<UsbIssue>,
}

/// Vendors of common hub, dock and adapter chips, for devices without a useful name.
pub fn vendor(vid: u16) -> Option<&'static str> {
    Some(match vid {
        0x2109 => "VIA Labs",
        0x1A40 => "Terminus Technology",
        0x05E3 => "Genesys Logic",
        0x0BDA => "Realtek",
        0x0B95 => "ASIX",
        0x2537 => "Norelsys",
        0x174C => "ASMedia",
        0x0451 => "Texas Instruments",
        0x0424 => "Microchip (SMSC)",
        0x04B4 => "Cypress",
        0x1D5C => "Fresco Logic",
        0x17EF => "Lenovo",
        0x413C => "Dell",
        0x03F0 => "HP",
        0x0B05 => "ASUS",
        0x13D3 => "AzureWave",
        0x27C6 => "Goodix",
        0x046D => "Logitech",
        0x045E => "Microsoft",
        0x8087 => "Intel",
        0x1D6B => "Linux Foundation",
        _ => return None,
    })
}

fn speed_text(c: &Connection, on_ss_port: &Port) -> (String, bool) {
    if c.speed == SPEED_SUPER_PLUS_2X2 {
        return ("SuperSpeed+ 20 Gbps".into(), true);
    }
    if c.speed == SPEED_SUPER_PLUS || (on_ss_port.usb3 && on_ss_port.op_ssp) {
        return ("SuperSpeed+ 10 Gbps".into(), true);
    }
    if on_ss_port.usb3 && on_ss_port.op_ss {
        return ("SuperSpeed 5 Gbps".into(), true);
    }
    match c.speed {
        SPEED_LOW => ("Low-Speed 1.5 Mbps".into(), false),
        SPEED_FULL => ("Full-Speed 12 Mbps".into(), false),
        SPEED_HIGH => ("High-Speed 480 Mbps".into(), false),
        SPEED_SUPER => ("SuperSpeed 5 Gbps".into(), true),
        _ => ("unknown speed".into(), false),
    }
}

fn short_name(name: &str) -> String {
    let n = name.split(" (").next().unwrap_or(name);
    match n.char_indices().nth(40) {
        Some((i, _)) => format!("{}…", &n[..i]),
        None => n.to_string(),
    }
}

struct Walker<'a> {
    hubs: &'a [Hub],
    by_instance: HashMap<&'a str, usize>,
    lines: Vec<String>,
    locations: HashMap<String, Location>,
    issues: Vec<UsbIssue>,
    visited: HashSet<usize>,
}

impl<'a> Walker<'a> {
    fn port(&self, hub: &str, number: u32) -> Option<&'a Port> {
        let idx = *self.by_instance.get(hub)?;
        self.hubs[idx].ports.iter().find(|p| p.number == number)
    }

    /// A USB 2.0 port whose companion is a USB 3 port: it is drawn under that port.
    fn is_secondary(&self, p: &Port) -> bool {
        if p.usb3 {
            return false;
        }
        match &p.companion {
            Some((hub, n)) => self.port(hub, *n).map(|c| c.usb3).unwrap_or(false),
            None => false,
        }
    }

    fn walk(&mut self, idx: usize, prefix: &str, chain: &[String], ext_hubs: usize, bus_pw: bool) {
        if !self.visited.insert(idx) {
            return;
        }
        let hub = &self.hubs[idx];
        let phys: Vec<(&Port, Option<&Port>)> = hub
            .ports
            .iter()
            .filter(|p| !self.is_secondary(p))
            .map(|p| {
                let sec = if p.usb3 { p.companion.as_ref().and_then(|(h, n)| self.port(h, *n)) } else { None };
                (p, sec)
            })
            .collect();

        let count = phys.len();
        for (i, (prim, sec)) in phys.into_iter().enumerate() {
            let last = i + 1 == count;
            let branch = if last { "└─ " } else { "├─ " };
            let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });

            let mut label = format!("Port {}", prim.number);
            if let Some(s) = sec {
                label.push_str(&format!("/{}", s.number));
            }
            let kind = match prim.connector {
                Connector::TypeC => "USB-C",
                Connector::Internal => "internal",
                Connector::External if hub.is_root => "USB-A",
                _ => "",
            };
            let cap = if prim.usb3 {
                "USB 3.x"
            } else if prim.usb2 {
                "USB 2.0"
            } else {
                "USB 1.1"
            };

            let (ss_conn, hs_conn, hs_port) = if prim.usb3 {
                (prim.conn.as_ref(), sec.and_then(|s| s.conn.as_ref()), sec)
            } else {
                (None, prim.conn.as_ref(), Some(prim))
            };

            let mut desc;
            let mut child: Option<&Connection> = None;
            if let Some(c) = ss_conn {
                let (sp, ss) = speed_text(c, prim);
                desc = format!("{} {} {}", green("●"), c.name, dim(format!("[{:04X}:{:04X}]", c.vid, c.pid)));
                desc.push_str(&format!("  {}", if ss { green(&sp) } else { yellow(&sp) }));
                if c.is_hub {
                    desc.push_str(&dim(" hub"));
                    if let Some(h2) = hs_conn.filter(|h| h.is_hub) {
                        desc.push_str(&dim(format!(" + USB 2.0 companion [{:04X}:{:04X}]", h2.vid, h2.pid)));
                    }
                    child = Some(c);
                }
                self.record(c, &sp, ss, chain, &label, &hub.name, ext_hubs, bus_pw);
            } else if let Some(c) = hs_conn {
                let port_for_speed = hs_port.unwrap_or(prim);
                let (sp, _) = speed_text(c, port_for_speed);
                let ss_capable = prim.usb3 && hs_port.map(|p| p.cap_ss).unwrap_or(false);
                desc = format!("{} {} {}", green("●"), c.name, dim(format!("[{:04X}:{:04X}]", c.vid, c.pid)));
                desc.push_str(&format!("  {}", if c.speed == SPEED_HIGH { sp.clone() } else { dim(&sp) }));
                if ss_capable {
                    desc.push_str(&yellow("  ⚠ USB 3 device running at USB 2.0 speed"));
                    self.issues.push(UsbIssue {
                        problem: false,
                        text: format!(
                            "{} is USB 3 capable but runs at USB 2.0 speed (480 Mbps) on {} {label}: try another cable or port",
                            c.name, hub.name
                        ),
                    });
                }
                if c.is_hub {
                    desc.push_str(&dim(if prim.usb3 { " hub (USB 2.0 link)" } else { " hub (USB 2.0 only)" }));
                    child = Some(c);
                }
                self.record(c, &sp, false, chain, &label, &hub.name, ext_hubs, bus_pw);
            } else {
                desc = dim("empty");
            }

            for p in std::iter::once(prim).chain(sec) {
                if let Some(problem) = &p.problem {
                    desc.push_str(&red(format!("  ✖ {problem}")));
                    self.issues.push(UsbIssue { problem: true, text: format!("{} {label}: {problem}", hub.name) });
                }
            }

            let label_col = format!("{label:<9} {kind:<8} {cap:<7}");
            self.lines.push(format!("{prefix}{branch}{} {desc}", dim(label_col)));

            if let Some(c) = child {
                if let Some(inst) = &c.instance {
                    if let Some(&cidx) = self.by_instance.get(inst.as_str()) {
                        let mut ch = chain.to_vec();
                        ch.push(format!("{} {label}", short_name(&hub.name)));
                        let bp = bus_pw || self.hubs[cidx].bus_powered;
                        self.walk(cidx, &child_prefix, &ch, ext_hubs + 1, bp);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        c: &Connection,
        speed: &str,
        ss: bool,
        chain: &[String],
        label: &str,
        hub_name: &str,
        ext_hubs: usize,
        bus_pw: bool,
    ) {
        if let Some(inst) = &c.instance {
            let mut ch = chain.to_vec();
            ch.push(format!("{} {label}", short_name(hub_name)));
            self.locations.insert(
                inst.clone(),
                Location {
                    chain: ch,
                    external_hubs: ext_hubs,
                    bus_powered_hub: bus_pw,
                    speed: speed.to_string(),
                    superspeed: ss,
                },
            );
        }
    }
}

pub fn scan() -> UsbTree {
    build(sys::usb::hubs())
}

pub fn build(mut hubs: Vec<Hub>) -> UsbTree {
    hubs.sort_by(|a, b| a.controller.cmp(&b.controller).then(a.instance.cmp(&b.instance)));

    let mut w = Walker {
        hubs: &hubs,
        by_instance: hubs.iter().enumerate().map(|(i, h)| (h.instance.as_str(), i)).collect(),
        lines: vec![],
        locations: HashMap::new(),
        issues: vec![],
        visited: HashSet::new(),
    };

    // Root hubs, grouped by controller (Linux shows a USB 2.0 and a USB 3 root
    // hub for each xHCI controller). Root hubs with nothing left to show after
    // the merge are skipped.
    let roots: Vec<usize> = (0..hubs.len())
        .filter(|&i| hubs[i].is_root && hubs[i].ports.iter().any(|p| !w.is_secondary(p)))
        .collect();
    let mut j = 0;
    while j < roots.len() {
        let ctrl = &hubs[roots[j]].controller;
        let group: Vec<usize> = roots[j..].iter().copied().take_while(|&i| &hubs[i].controller == ctrl).collect();
        j += group.len();
        w.lines.push(bold(ctrl.clone().unwrap_or_else(|| "USB host controller".into())));
        for (k, &i) in group.iter().enumerate() {
            let last = k + 1 == group.len();
            let ports = hubs[i].ports.iter().filter(|p| !w.is_secondary(p)).count();
            w.lines.push(format!(
                "{}{}",
                if last { "└─ " } else { "├─ " },
                dim(format!("{}, {ports} ports", hubs[i].name))
            ));
            w.walk(i, if last { "   " } else { "│  " }, &[], 0, false);
        }
    }

    // Hubs that were not reached from a root hub (should be rare).
    let orphans: Vec<usize> = (0..hubs.len())
        .filter(|i| !w.visited.contains(i) && !hubs[*i].is_root)
        .filter(|&i| hubs[i].ports.iter().any(|p| !w.is_secondary(p)))
        .collect();
    for i in orphans {
        if w.visited.contains(&i) {
            continue;
        }
        w.lines.push(bold(format!("{} (not linked to a root hub)", hubs[i].name)));
        w.walk(i, "   ", &[], 1, hubs[i].bus_powered);
    }

    let (lines, locations, issues) = (w.lines, w.locations, w.issues);
    UsbTree { lines, locations, issues }
}

impl UsbTree {
    /// Finds a device, or the nearest parent device that is in the tree.
    pub fn locate(&self, instance: &str) -> Option<&Location> {
        let mut cur = instance.to_uppercase();
        for _ in 0..4 {
            if let Some(l) = self.locations.get(&cur) {
                return Some(l);
            }
            cur = sys::usb::parent(&cur)?.to_uppercase();
        }
        None
    }

    pub fn print(&self) {
        heading("USB topology");
        if self.lines.is_empty() {
            println!("{}", dim("No USB controllers found."));
        }
        for l in &self.lines {
            println!("{l}");
        }
        println!("{}", dim("Port n/m = a USB 3 port (n) and its USB 2.0 companion (m) on the same connector."));
    }
}
