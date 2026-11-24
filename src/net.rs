//! Physical network adapters: link state and speed, traffic and error
//! counters, IP and gateway, driver, and the advanced driver settings that
//! matter for flaky USB adapters (Energy-Efficient Ethernet, selective suspend).

use std::collections::HashMap;
use std::net::Ipv4Addr;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetIpForwardTable2, GetUnicastIpAddressTable, MIB_IF_TABLE2,
    MIB_IPFORWARD_TABLE2, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

use crate::util::*;

const NET_CLASS: &str = "{4d36e972-e325-11ce-bfc1-08002be10318}";

#[derive(Debug, Clone)]
pub struct Setting {
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
    pub driver_date: Option<String>,
    pub instance: Option<String>,
    pub service: Option<String>,
    pub settings: Vec<Setting>,
}

impl Adapter {
    pub fn setting(&self, key: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.key.eq_ignore_ascii_case(key))
    }
    pub fn is_usb(&self) -> bool {
        self.instance.as_deref().map(|i| i.starts_with("USB")).unwrap_or(false)
    }
    pub fn gigabit_capable(&self) -> bool {
        let d = self.description.to_lowercase();
        d.contains("gigabit") || d.contains("gbe") || d.contains("2.5g") || d.contains("1000") || self.link_bps >= 1_000_000_000
    }
    pub fn fast_ethernet_only(&self) -> bool {
        self.description.to_lowercase().contains("fast ethernet")
    }
}

fn guid_string(g: &windows_sys::core::GUID) -> String {
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        g.data1, g.data2, g.data3, g.data4[0], g.data4[1], g.data4[2], g.data4[3], g.data4[4], g.data4[5], g.data4[6],
        g.data4[7]
    )
}

unsafe fn ipv4_of(sa: &SOCKADDR_INET) -> Option<Ipv4Addr> {
    if sa.si_family != AF_INET {
        return None;
    }
    let raw = sa.Ipv4.sin_addr.S_un.S_addr;
    Some(Ipv4Addr::from(raw.to_ne_bytes()))
}

fn ipv4_addresses() -> HashMap<u32, Vec<Ipv4Addr>> {
    let mut map: HashMap<u32, Vec<Ipv4Addr>> = HashMap::new();
    unsafe {
        let mut t: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
        if GetUnicastIpAddressTable(AF_INET, &mut t) != 0 || t.is_null() {
            return map;
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for r in rows {
            if let Some(ip) = ipv4_of(&r.Address) {
                map.entry(r.InterfaceIndex).or_default().push(ip);
            }
        }
        FreeMibTable(t as *const _);
    }
    map
}

fn default_gateways() -> HashMap<u32, Vec<Ipv4Addr>> {
    let mut map: HashMap<u32, Vec<Ipv4Addr>> = HashMap::new();
    unsafe {
        let mut t: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
        if GetIpForwardTable2(AF_INET, &mut t) != 0 || t.is_null() {
            return map;
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for r in rows {
            if r.DestinationPrefix.PrefixLength != 0 {
                continue;
            }
            if let Some(gw) = ipv4_of(&r.NextHop) {
                if !gw.is_unspecified() {
                    map.entry(r.InterfaceIndex).or_default().push(gw);
                }
            }
        }
        FreeMibTable(t as *const _);
    }
    map
}

/// The adapter's class key under Control\Class\{4d36e972-...}, found by its interface GUID.
fn class_key(guid: &str) -> Option<(RegKey, String)> {
    let class = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Control\Class\{NET_CLASS}"))
        .ok()?;
    for sub in class.enum_keys().flatten() {
        if sub.len() != 4 {
            continue;
        }
        let Ok(k) = class.open_subkey(&sub) else { continue };
        let id: String = k.get_value("NetCfgInstanceId").unwrap_or_default();
        if id.eq_ignore_ascii_case(guid) {
            return Some((k, format!(r"{NET_CLASS}\{sub}")));
        }
    }
    None
}

/// Reads every advanced property the driver exposes (Ndi\Params), with display names.
fn advanced_settings(k: &RegKey) -> Vec<Setting> {
    let mut out = Vec::new();
    let Ok(params) = k.open_subkey(r"Ndi\Params") else { return out };
    for key in params.enum_keys().flatten() {
        let Ok(pk) = params.open_subkey(&key) else { continue };
        let name: String = pk.get_value("ParamDesc").map(|s: String| strip_inf(&s)).unwrap_or_else(|_| key.clone());
        let raw: String = k
            .get_value(&key)
            .or_else(|_| pk.get_value("default"))
            .unwrap_or_default();
        let value = pk
            .open_subkey("enum")
            .ok()
            .and_then(|e| e.get_value::<String, _>(&raw).ok())
            .map(|s| strip_inf(&s))
            .unwrap_or_else(|| raw.clone());
        out.push(Setting { key, name, value, raw });
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    out
}

pub fn scan(drivers: &HashMap<String, String>) -> Vec<Adapter> {
    let ips = ipv4_addresses();
    let gws = default_gateways();
    let reverse: HashMap<&str, &str> = drivers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let mut out = Vec::new();

    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut t) != 0 || t.is_null() {
            return out;
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        for r in rows {
            // 6 = Ethernet, 71 = 802.11; bit 0 = hardware interface; bit 1 = filter (skip LWF duplicates).
            let bits = r.InterfaceAndOperStatusFlags._bitfield;
            if !(r.Type == 6 || r.Type == 71) || bits & 1 == 0 || bits & 2 != 0 {
                continue;
            }
            let guid = guid_string(&r.InterfaceGuid);
            let (driver_version, driver_date, instance, settings) = match class_key(&guid) {
                Some((k, drv_key)) => {
                    let inst: Option<String> = k
                        .get_value::<String, _>("DeviceInstanceID")
                        .ok()
                        .or_else(|| reverse.get(drv_key.to_lowercase().as_str()).map(|s| s.to_string()));
                    (k.get_value("DriverVersion").ok(), k.get_value("DriverDate").ok(), inst, advanced_settings(&k))
                }
                None => (None, None, None, vec![]),
            };
            let mac = r.PhysicalAddress[..r.PhysicalAddressLength.min(32) as usize]
                .iter()
                .map(|b| format!("{b:02X}"))
                .collect::<Vec<_>>()
                .join("-");
            out.push(Adapter {
                alias: from_wide(&r.Alias),
                description: from_wide(&r.Description),
                mac,
                wireless: r.Type == 71,
                connected: r.MediaConnectState == 1,
                link_bps: r.ReceiveLinkSpeed.max(r.TransmitLinkSpeed),
                in_bytes: r.InOctets,
                out_bytes: r.OutOctets,
                in_errors: r.InErrors,
                out_errors: r.OutErrors,
                in_discards: r.InDiscards,
                out_discards: r.OutDiscards,
                ipv4: ips.get(&r.InterfaceIndex).cloned().unwrap_or_default(),
                gateways: gws.get(&r.InterfaceIndex).cloned().unwrap_or_default(),
                driver_version,
                driver_date,
                service: instance.as_deref().and_then(device_service),
                instance: instance.map(|s| s.to_uppercase()),
                settings,
            });
        }
        FreeMibTable(t as *const _);
    }
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

pub fn print(adapters: &[Adapter], usb: &crate::usb::UsbTree, verbose: bool) {
    heading("Network adapters");
    if adapters.is_empty() {
        println!("{}", dim("No physical network adapters found."));
    }
    for a in adapters {
        let state = if a.connected {
            green(format!("connected, link {}", fmt_bps(a.link_bps as f64)))
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
        if let (Some(v), d) = (&a.driver_version, &a.driver_date) {
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

        if let Some(inst) = &a.instance {
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
