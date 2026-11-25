//! Linux USB backend: /sys/bus/usb/devices.
//!
//! An xHCI controller shows up as two root hubs (a USB 2.0 bus and a USB 3
//! bus), and a USB 3 hub as two hubs, one on each bus. Each port of a USB 3
//! bus links to its USB 2.0 twin through the `peer` symlink, which is what
//! the shared tree code needs to merge them into physical connectors.

use std::fs;
use std::path::{Path, PathBuf};

use super::{attr, attr_hex, attr_num, id_name, sysfs};
use crate::usb::*;
use crate::util::Stamp;

fn devices_dir() -> PathBuf {
    sysfs().join("bus/usb/devices")
}

/// "1.5" / "12" / "480" / "5000" / "10000" / "20000" (Mbps) as a speed code.
fn speed_code(mbps: f64) -> u8 {
    match mbps {
        m if m >= 20_000.0 => SPEED_SUPER_PLUS_2X2,
        m if m >= 10_000.0 => SPEED_SUPER_PLUS,
        m if m >= 5_000.0 => SPEED_SUPER,
        m if m >= 480.0 => SPEED_HIGH,
        m if m >= 12.0 => SPEED_FULL,
        m if m > 0.0 => SPEED_LOW,
        _ => 255,
    }
}

/// A readable name for a USB device: its product string, or the usb.ids name.
pub fn device_name(dir: &Path) -> String {
    let vid = attr_hex(dir, "idVendor").unwrap_or(0);
    let pid = attr_hex(dir, "idProduct").unwrap_or(0);
    let product = attr(dir, "product").or_else(|| id_name("usb.ids", vid, Some(pid)));
    let name = product.unwrap_or_else(|| "Unknown device".into());
    // The manufacturer string, unless it is a placeholder or already in the name.
    let maker = attr(dir, "manufacturer").filter(|m| {
        !m.starts_with("Linux") && !m.starts_with("Generic") && !name.to_lowercase().contains(&m.to_lowercase())
    });
    let generic = name.starts_with("Generic") || name == "Unknown device";
    match (maker, vendor(vid as u16)) {
        (Some(m), _) => format!("{name} ({m})"),
        (None, Some(v)) if generic => format!("{name} ({v})"),
        _ => name,
    }
}

/// Name of the host controller a root hub belongs to, with its bus address.
fn controller_name(root: &Path) -> Option<String> {
    let ctrl = fs::canonicalize(root).ok()?.parent()?.to_path_buf();
    let addr = ctrl.file_name()?.to_string_lossy().into_owned();
    let pci = match (attr_hex(&ctrl, "vendor"), attr_hex(&ctrl, "device")) {
        (Some(v), Some(d)) => id_name("pci.ids", v, Some(d)).or_else(|| Some(format!("PCI {v:04x}:{d:04x}"))),
        _ => None,
    };
    let name = pci.or_else(|| attr(root, "product")).unwrap_or_else(|| "USB host controller".into());
    Some(format!("{name} [{addr}]"))
}

/// "usb1-port3" or "1-2-port3" -> ("usb1" / "1-2", 3).
fn split_port(name: &str) -> Option<(String, u32)> {
    let (hub, n) = name.rsplit_once("-port")?;
    Some((hub.to_string(), n.parse().ok()?))
}

/// The port directories of a hub, from its interface directory ("1-2:1.0/1-2-port1").
fn port_dirs(hub_dir: &Path, hub: &str) -> Vec<(u32, PathBuf)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(hub_dir) else { return out };
    for e in entries.flatten() {
        let iname = e.file_name().to_string_lossy().into_owned();
        if !iname.contains(':') {
            continue;
        }
        let Ok(ports) = fs::read_dir(e.path()) else { continue };
        for p in ports.flatten() {
            let pname = p.file_name().to_string_lossy().into_owned();
            if let Some((h, n)) = split_port(&pname) {
                if h == hub {
                    out.push((n, p.path()));
                }
            }
        }
    }
    out.sort_by_key(|(n, _)| *n);
    out
}

fn read_port(hub: &str, hub_dir: &Path, n: u32, port_dir: &Path, hub_mbps: f64, root: bool) -> Option<Port> {
    let mut port = Port::new(n);
    port.connector = match attr(port_dir, "connect_type").as_deref() {
        Some("not used") => return None,
        Some("hotplug") => Connector::External,
        Some("hardwired") => Connector::Internal,
        _ => Connector::Unknown,
    };
    if port_dir.join("connector").exists() {
        port.connector = Connector::TypeC;
    }
    if let Ok(target) = fs::canonicalize(port_dir.join("peer")) {
        if let Some((h, pn)) = target.file_name().and_then(|f| split_port(&f.to_string_lossy())) {
            port.companion = Some((h.to_uppercase(), pn));
        }
    }
    port.usb3 = hub_mbps >= 5000.0;
    port.usb2 = !port.usb3 && hub_mbps >= 480.0;
    if let Some(oc) = attr_num::<u32>(port_dir, "over_current_count").filter(|&c| c > 0) {
        port.problem = Some(format!("{oc} overcurrent event(s): the device drew too much power"));
    }

    let child = if root {
        format!("{}-{n}", attr(hub_dir, "busnum").unwrap_or_else(|| hub.trim_start_matches("usb").to_string()))
    } else {
        format!("{hub}.{n}")
    };
    let dev = devices_dir().join(&child);
    if dev.join("idVendor").exists() {
        let mbps: f64 = attr_num(&dev, "speed").unwrap_or(0.0);
        let version: f64 = attr_num(&dev, "version").unwrap_or(0.0);
        port.op_ss = port.usb3 && mbps >= 5000.0;
        port.op_ssp = port.usb3 && mbps >= 10_000.0;
        port.cap_ss = !port.usb3 && version >= 3.0;
        port.conn = Some(Connection {
            speed: speed_code(mbps),
            vid: attr_hex(&dev, "idVendor").unwrap_or(0) as u16,
            pid: attr_hex(&dev, "idProduct").unwrap_or(0) as u16,
            is_hub: attr(&dev, "bDeviceClass").as_deref() == Some("09"),
            instance: Some(child.to_uppercase()),
            name: device_name(&dev),
        });
    }
    Some(port)
}

fn read_hub(name: &str, dir: &Path) -> Hub {
    let root = name.starts_with("usb");
    let mbps: f64 = attr_num(dir, "speed").unwrap_or(0.0);
    let ports = port_dirs(dir, name)
        .into_iter()
        .filter_map(|(n, p)| read_port(name, dir, n, &p, mbps, root))
        .collect();
    let bus = if mbps >= 5000.0 {
        "USB 3.x"
    } else if mbps >= 480.0 {
        "USB 2.0"
    } else {
        "USB 1.1"
    };
    Hub {
        instance: name.to_uppercase(),
        name: if root { format!("Root hub ({bus} bus)") } else { device_name(dir) },
        is_root: root,
        bus_powered: attr_hex(dir, "bmAttributes").map(|a| a & 0x40 == 0).unwrap_or(false),
        controller: if root { controller_name(dir) } else { None },
        ports,
    }
}

/// Every USB hub in the system with its ports.
pub fn hubs() -> Vec<Hub> {
    let dir = devices_dir();
    let Ok(entries) = fs::read_dir(&dir) else { return vec![] };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.contains(':'))
        .collect();
    names.sort();
    names
        .iter()
        .filter(|n| attr(&dir.join(n), "bDeviceClass").as_deref() == Some("09"))
        .map(|n| read_hub(n, &dir.join(n)))
        .collect()
}

/// "2-1.3:1.0" -> "2-1.3" -> "2-1" -> "USB2".
pub fn parent(instance: &str) -> Option<String> {
    let s = instance.to_lowercase();
    if let Some((dev, _)) = s.split_once(':') {
        return Some(dev.to_uppercase());
    }
    if let Some((up, _)) = s.rsplit_once('.') {
        return Some(up.to_uppercase());
    }
    let (bus, _) = s.split_once('-')?;
    bus.chars().all(|c| c.is_ascii_digit()).then(|| format!("USB{bus}"))
}

/// When the device was plugged in: now minus how long it has been connected.
pub fn last_arrival(instance: &str) -> Option<Stamp> {
    let dir = devices_dir().join(instance.to_lowercase());
    let ms: u64 = attr_num(&dir.join("power"), "connected_duration")?;
    Some(Stamp(Stamp::now().0.saturating_sub(ms * 10_000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents() {
        assert_eq!(parent("2-1.3:1.0").as_deref(), Some("2-1.3"));
        assert_eq!(parent("2-1.3").as_deref(), Some("2-1"));
        assert_eq!(parent("2-1").as_deref(), Some("USB2"));
        assert_eq!(parent("USB2"), None);
    }

    #[test]
    fn ports() {
        assert_eq!(split_port("usb1-port3"), Some(("usb1".into(), 3)));
        assert_eq!(split_port("1-2.4-port12"), Some(("1-2.4".into(), 12)));
    }
}
