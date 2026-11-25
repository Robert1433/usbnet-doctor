//! Windows network backend: IP Helper tables and the adapter's registry class key.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, GetIpForwardTable2, GetUnicastIpAddressTable, MIB_IF_TABLE2,
    MIB_IPFORWARD_TABLE2, MIB_UNICASTIPADDRESS_TABLE,
};
use windows_sys::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

use super::{device_service, from_wide, strip_inf};
use crate::net::{Adapter, Setting};

const NET_CLASS: &str = "{4d36e972-e325-11ce-bfc1-08002be10318}";

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

pub fn adapters() -> Vec<Adapter> {
    let drivers = &super::usb::driver_map();
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
            let (driver_version, driver_note, instance, settings) = match class_key(&guid) {
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
                driver_note,
                usb: instance.as_deref().map(|i| i.to_uppercase().starts_with("USB")).unwrap_or(false),
                gigabit_hint: false,
                service: instance.as_deref().and_then(device_service),
                instance: instance.map(|s| s.to_uppercase()),
                settings,
            });
        }
        FreeMibTable(t as *const _);
    }
    out
}

