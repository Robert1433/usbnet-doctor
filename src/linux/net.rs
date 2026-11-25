//! Linux network backend: /sys/class/net for link state and counters,
//! getifaddrs and /proc/net/route for addresses and gateways, and ethtool
//! ioctls (read-only, no root needed) for the driver and its settings.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::fs;
use std::net::Ipv4Addr;
use std::path::Path;

use super::{attr, attr_hex, attr_num, id_name, link_name, sysfs};
use crate::net::{Adapter, Setting};

const SIOCETHTOOL: u64 = 0x8946;
const ETHTOOL_GSET: u32 = 0x01;
const ETHTOOL_GDRVINFO: u32 = 0x03;
const ETHTOOL_GPAUSEPARAM: u32 = 0x12;
const ETHTOOL_GEEE: u32 = 0x44;

/// `struct ifreq` with the `ifr_data` member of the union.
#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    data: *mut c_void,
    _pad: [u8; 16],
}

#[repr(C)]
#[derive(Default)]
struct EthtoolCmd {
    cmd: u32,
    supported: u32,
    advertising: u32,
    speed: u16,
    duplex: u8,
    port: u8,
    phy_address: u8,
    transceiver: u8,
    autoneg: u8,
    mdio_support: u8,
    maxtxpkt: u32,
    maxrxpkt: u32,
    speed_hi: u16,
    eth_tp_mdix: u8,
    eth_tp_mdix_ctrl: u8,
    lp_advertising: u32,
    reserved: [u32; 2],
}

#[repr(C)]
struct DrvInfo {
    cmd: u32,
    driver: [u8; 32],
    version: [u8; 32],
    fw_version: [u8; 32],
    bus_info: [u8; 32],
    erom_version: [u8; 32],
    reserved2: [u8; 12],
    n_priv_flags: u32,
    n_stats: u32,
    testinfo_len: u32,
    eedump_len: u32,
    regdump_len: u32,
}

#[repr(C)]
#[derive(Default)]
struct PauseParam {
    cmd: u32,
    autoneg: u32,
    rx_pause: u32,
    tx_pause: u32,
}

#[repr(C)]
#[derive(Default)]
struct Eee {
    cmd: u32,
    supported: u32,
    advertised: u32,
    lp_advertised: u32,
    eee_active: u32,
    eee_enabled: u32,
    tx_lpi_enabled: u32,
    tx_lpi_timer: u32,
    reserved: [u32; 2],
}

struct Socket(libc::c_int);
impl Drop for Socket {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// Runs an ethtool "get" command; `data` must start with its `cmd` field.
fn ethtool<T>(sock: &Socket, ifname: &str, data: &mut T) -> bool {
    let mut req = IfReq { name: [0; libc::IFNAMSIZ], data: data as *mut T as *mut c_void, _pad: [0; 16] };
    for (d, s) in req.name.iter_mut().zip(ifname.bytes().take(libc::IFNAMSIZ - 1)) {
        *d = s as libc::c_char;
    }
    unsafe { libc::ioctl(sock.0, SIOCETHTOOL as _, &mut req) == 0 }
}

fn c_str(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn ipv4_addresses() -> HashMap<String, Vec<Ipv4Addr>> {
    let mut map: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return map;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                let name = CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned();
                map.entry(name).or_default().push(ip);
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    map
}

/// Default gateways per interface, from /proc/net/route.
fn default_gateways() -> HashMap<String, Vec<Ipv4Addr>> {
    let mut map: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
    let Ok(text) = fs::read_to_string("/proc/net/route") else { return map };
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[1] != "00000000" || f[7] != "00000000" {
            continue;
        }
        if let Ok(gw) = u32::from_str_radix(f[2], 16) {
            // The kernel prints the address in host byte order (little-endian).
            let ip = Ipv4Addr::from(gw.to_le_bytes());
            if !ip.is_unspecified() {
                map.entry(f[0].to_string()).or_default().push(ip);
            }
        }
    }
    map
}

/// The USB device ("2-1.3") an interface sits on, if any.
fn usb_device(device_path: &Path) -> Option<String> {
    device_path.ancestors().filter_map(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned()).find(|n| {
        let mut parts = n.splitn(2, '-');
        let bus = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        !bus.is_empty()
            && bus.chars().all(|c| c.is_ascii_digit())
            && !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit() || c == '.')
    })
}

fn describe(ifdir: &Path, usb: Option<&str>, driver: Option<&str>) -> String {
    if let Some(dev) = usb {
        return super::usb::device_name(&sysfs().join("bus/usb/devices").join(dev));
    }
    let dev = ifdir.join("device");
    if let (Some(v), Some(d)) = (attr_hex(&dev, "vendor"), attr_hex(&dev, "device")) {
        if let Some(n) = id_name("pci.ids", v, Some(d)) {
            return n;
        }
        return format!("PCI {v:04x}:{d:04x}");
    }
    driver.unwrap_or("network adapter").to_string()
}

fn setting(key: &str, name: &str, value: String, raw: &str) -> Setting {
    Setting { key: key.into(), name: name.into(), value, raw: raw.into() }
}

fn on_off(v: u32) -> &'static str {
    if v != 0 {
        "on"
    } else {
        "off"
    }
}

pub fn adapters() -> Vec<Adapter> {
    let ips = ipv4_addresses();
    let gws = default_gateways();
    let sock = Socket(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) });
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(sysfs().join("class/net")) else { return out };

    for e in entries.flatten() {
        let ifname = e.file_name().to_string_lossy().into_owned();
        let dir = e.path();
        // Physical adapters only: they have a `device` link; type 1 = Ethernet (Wi-Fi included).
        if !dir.join("device").exists() || attr(&dir, "type").as_deref() != Some("1") {
            continue;
        }
        let device_path = fs::canonicalize(dir.join("device")).unwrap_or_default();
        let usb = usb_device(&device_path);
        let driver = link_name(&dir.join("device/driver"));
        let wireless = dir.join("wireless").exists() || dir.join("phy80211").exists();
        let stat = |n: &str| attr_num::<u64>(&dir.join("statistics"), n).unwrap_or(0);
        let connected = attr(&dir, "carrier").as_deref() == Some("1")
            && attr(&dir, "operstate").map(|s| s != "down").unwrap_or(true);
        let speed_mbps = attr_num::<i64>(&dir, "speed").filter(|&s| s > 0 && connected);

        let mut settings = Vec::new();
        let mut driver_version = None;
        let mut driver_note = None;
        let mut gigabit_hint = false;
        if sock.0 >= 0 {
            let mut info: DrvInfo = unsafe { std::mem::zeroed() };
            info.cmd = ETHTOOL_GDRVINFO;
            if ethtool(&sock, &ifname, &mut info) {
                let (drv, ver, fw) = (c_str(&info.driver), c_str(&info.version), c_str(&info.fw_version));
                driver_version = Some(if ver.is_empty() { drv } else { format!("{drv} {ver}") });
                if !fw.is_empty() && fw != "N/A" {
                    driver_note = Some(format!("firmware {fw}"));
                }
            }

            let mut cmd = EthtoolCmd { cmd: ETHTOOL_GSET, ..Default::default() };
            if ethtool(&sock, &ifname, &mut cmd) {
                // 1000baseT half/full, 2500baseX, 1000baseKX, 10000baseT.
                gigabit_hint = cmd.supported & (1 << 4 | 1 << 5 | 1 << 12 | 1 << 15 | 1 << 17) != 0;
                let duplex = attr(&dir, "duplex").filter(|d| d != "unknown");
                let value = match (speed_mbps, duplex) {
                    (Some(s), Some(d)) => format!("{s} Mbps {d}"),
                    (Some(s), None) => format!("{s} Mbps"),
                    _ if connected => "speed not reported".to_string(),
                    _ => "no link".to_string(),
                };
                let auto = if cmd.autoneg != 0 { "auto-negotiated" } else { "forced" };
                settings.push(setting("*SpeedDuplex", "Speed & duplex", format!("{value}, {auto}"), ""));
            }

            let mut eee = Eee { cmd: ETHTOOL_GEEE, ..Default::default() };
            if ethtool(&sock, &ifname, &mut eee) && eee.supported != 0 {
                let mut v = on_off(eee.eee_enabled).to_string();
                if eee.eee_enabled != 0 {
                    v.push_str(if eee.eee_active != 0 { " (active)" } else { " (not active on this link)" });
                }
                settings.push(setting("*EEE", "Energy-Efficient Ethernet", v, if eee.eee_enabled != 0 { "1" } else { "0" }));
            }

            let mut pause = PauseParam { cmd: ETHTOOL_GPAUSEPARAM, ..Default::default() };
            if ethtool(&sock, &ifname, &mut pause) {
                let v = format!(
                    "rx {}, tx {}{}",
                    on_off(pause.rx_pause),
                    on_off(pause.tx_pause),
                    if pause.autoneg != 0 { ", negotiated" } else { "" }
                );
                settings.push(setting("*FlowControl", "Flow control", v, ""));
            }
        }
        if let Some(mtu) = attr(&dir, "mtu") {
            settings.push(setting("*JumboPacket", "MTU", mtu.clone(), &mtu));
        }
        if let Some(dev) = &usb {
            let power = sysfs().join("bus/usb/devices").join(dev).join("power");
            if let Some(control) = attr(&power, "control") {
                let delay = attr(&power, "autosuspend_delay_ms");
                let (value, raw) = if control == "auto" {
                    (format!("on{}", delay.map(|d| format!(" (after {d} ms idle)")).unwrap_or_default()), "1")
                } else {
                    ("off".to_string(), "0")
                };
                settings.push(setting("*SelectiveSuspend", "USB autosuspend", value, raw));
            }
        }
        if let Some(bus) = device_path.file_name() {
            settings.push(setting("BusInfo", "Bus address", bus.to_string_lossy().into_owned(), ""));
        }
        settings.sort_by_key(|a| a.name.to_lowercase());

        out.push(Adapter {
            description: describe(&dir, usb.as_deref(), driver.as_deref()),
            mac: attr(&dir, "address").unwrap_or_default().to_uppercase(),
            wireless,
            connected,
            link_bps: speed_mbps.map(|s| s as u64 * 1_000_000).unwrap_or(0),
            in_bytes: stat("rx_bytes"),
            out_bytes: stat("tx_bytes"),
            in_errors: stat("rx_errors"),
            out_errors: stat("tx_errors"),
            in_discards: stat("rx_dropped"),
            out_discards: stat("tx_dropped"),
            ipv4: ips.get(&ifname).cloned().unwrap_or_default(),
            gateways: gws.get(&ifname).cloned().unwrap_or_default(),
            driver_version: driver_version.or_else(|| driver.clone()),
            driver_note,
            instance: usb.as_ref().map(|u| u.to_uppercase()),
            usb: usb.is_some(),
            gigabit_hint,
            service: driver,
            settings,
            alias: ifname,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn struct_sizes_match_the_kernel() {
        assert_eq!(std::mem::size_of::<EthtoolCmd>(), 44);
        assert_eq!(std::mem::size_of::<DrvInfo>(), 196);
        assert_eq!(std::mem::size_of::<Eee>(), 40);
        assert_eq!(std::mem::size_of::<IfReq>(), 40);
    }

    #[test]
    fn finds_usb_device_in_path() {
        let p = Path::new("/sys/devices/pci0000:00/0000:00:14.0/usb2/2-1/2-1.3/2-1.3:1.0");
        assert_eq!(usb_device(p).as_deref(), Some("2-1.3"));
        assert_eq!(usb_device(Path::new("/sys/devices/pci0000:00/0000:00:1f.6")), None);
    }
}
