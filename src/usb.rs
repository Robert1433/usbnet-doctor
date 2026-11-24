//! USB topology: every hub, every port, what is plugged in and at what speed.
//!
//! Uses the same hub IOCTLs as Microsoft's USBView. A USB 3 hub shows up in
//! Windows as two hubs (a SuperSpeed one and its USB 2.0 companion); this
//! module merges each SuperSpeed port with its USB 2.0 companion port so the
//! tree shows physical ports.

use std::collections::{HashMap, HashSet};
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

use crate::util::*;

const GUID_DEVINTERFACE_USB_HUB: u128 = 0xf18a0e88_c30c_11d0_8815_00a0c906bed8;

const IOCTL_NODE_INFO: u32 = 0x22_0408;
const IOCTL_DESCRIPTOR: u32 = 0x22_0410;
const IOCTL_DRIVERKEY: u32 = 0x22_0420;
const IOCTL_CONN_EX: u32 = 0x22_0448;
const IOCTL_PORT_PROPS: u32 = 0x22_0458;
const IOCTL_CONN_EX_V2: u32 = 0x22_045C;

#[derive(Debug, Clone)]
pub struct Connection {
    pub speed: u8,
    pub vid: u16,
    pub pid: u16,
    pub is_hub: bool,
    pub instance: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Port {
    pub number: u32,
    pub user: bool,
    pub type_c: bool,
    /// Companion port: (hub instance id, upper-cased; port number).
    pub companion: Option<(String, u32)>,
    pub usb2: bool,
    pub usb3: bool,
    pub op_ss: bool,
    pub op_ssp: bool,
    pub cap_ss: bool,
    pub status: u32,
    pub conn: Option<Connection>,
}

#[derive(Debug)]
pub struct Hub {
    pub instance: String,
    pub name: String,
    pub is_root: bool,
    pub bus_powered: bool,
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

struct HubHandle(HANDLE);
impl Drop for HubHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

fn ioctl(h: HANDLE, code: u32, buf: &mut [u8]) -> Option<usize> {
    let mut ret = 0u32;
    let ptr = buf.as_mut_ptr();
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            ptr as *const _,
            buf.len() as u32,
            ptr as *mut _,
            buf.len() as u32,
            &mut ret,
            null_mut(),
        )
    };
    (ok != 0).then_some(ret as usize)
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn interface_paths(guid: &GUID) -> Vec<String> {
    let mut len = 0u32;
    unsafe {
        if CM_Get_Device_Interface_List_SizeW(&mut len, guid, null(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
            != CR_SUCCESS
            || len == 0
        {
            return vec![];
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_Interface_ListW(guid, null(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
            != CR_SUCCESS
        {
            return vec![];
        }
        buf.split(|&c| c == 0)
            .filter(|s| !s.is_empty())
            .map(String::from_utf16_lossy)
            .collect()
    }
}

/// "\\?\USB#VID_2109&PID_0817#6&13c452c7&0&6#{guid}" -> "USB\VID_2109&PID_0817\6&13C452C7&0&6"
pub fn instance_from_link(link: &str) -> String {
    let s = link.trim_start_matches(r"\\?\").trim_start_matches(r"\\.\");
    let parts: Vec<&str> = s.split('#').collect();
    if parts.len() >= 3 {
        format!(r"{}\{}\{}", parts[0], parts[1], parts[2]).to_uppercase()
    } else {
        s.to_uppercase()
    }
}

/// Driver key ("{36fc9e60-...}\0012") -> device instance id, for every USB device.
pub fn driver_map() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(usb) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(r"SYSTEM\CurrentControlSet\Enum\USB") else {
        return map;
    };
    for hw in usb.enum_keys().flatten() {
        let Ok(hk) = usb.open_subkey(&hw) else { continue };
        for inst in hk.enum_keys().flatten() {
            let Ok(ik) = hk.open_subkey(&inst) else { continue };
            if let Ok(drv) = ik.get_value::<String, _>("Driver") {
                map.insert(drv.to_lowercase(), format!(r"USB\{hw}\{inst}"));
            }
        }
    }
    map
}

fn string_descriptor(h: HANDLE, port: u32, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = vec![0u8; 12 + 255];
    buf[0..4].copy_from_slice(&port.to_le_bytes());
    buf[4] = 0x80; // device-to-host, standard, device
    buf[5] = 6; // GET_DESCRIPTOR
    buf[6..8].copy_from_slice(&((3u16 << 8) | index as u16).to_le_bytes());
    buf[8..10].copy_from_slice(&0x0409u16.to_le_bytes());
    buf[10..12].copy_from_slice(&255u16.to_le_bytes());
    let ret = ioctl(h, IOCTL_DESCRIPTOR, &mut buf)?;
    if ret < 14 || buf[13] != 3 {
        return None;
    }
    let end = (12 + buf[12] as usize).min(ret);
    let units: Vec<u16> = buf[14..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let s = String::from_utf16_lossy(&units).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn read_hub(path: &str, drivers: &HashMap<String, String>) -> Option<Hub> {
    let w = wide(path);
    let h = unsafe {
        CreateFileW(
            w.as_ptr(),
            0x4000_0000, // GENERIC_WRITE, as USBView does
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let h = HubHandle(h);
    let mut ni = vec![0u8; 80];
    ioctl(h.0, IOCTL_NODE_INFO, &mut ni)?;
    let count = ni[6] as u32;
    let bus_powered = ni[75] != 0;

    let instance = instance_from_link(path);
    let is_root = instance.starts_with(r"USB\ROOT_HUB");
    let mut ports = Vec::new();
    for p in 1..=count {
        let mut port = Port {
            number: p,
            user: false,
            type_c: false,
            companion: None,
            usb2: false,
            usb3: false,
            op_ss: false,
            op_ssp: false,
            cap_ss: false,
            status: 0,
            conn: None,
        };

        let mut pp = vec![0u8; 512];
        pp[0..4].copy_from_slice(&p.to_le_bytes());
        if let Some(ret) = ioctl(h.0, IOCTL_PORT_PROPS, &mut pp) {
            let flags = u32_at(&pp, 8);
            port.user = flags & 1 != 0;
            port.type_c = flags & 8 != 0;
            let cport = u16_at(&pp, 14) as u32;
            if cport != 0 && ret > 18 {
                let units: Vec<u16> = pp[16..ret].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                let link = from_wide(&units);
                if !link.is_empty() {
                    port.companion = Some((instance_from_link(&link), cport));
                }
            }
        }

        let mut v2 = vec![0u8; 16];
        v2[0..4].copy_from_slice(&p.to_le_bytes());
        v2[4..8].copy_from_slice(&16u32.to_le_bytes());
        v2[8..12].copy_from_slice(&7u32.to_le_bytes());
        if ioctl(h.0, IOCTL_CONN_EX_V2, &mut v2).is_some() {
            let proto = u32_at(&v2, 8);
            let flags = u32_at(&v2, 12);
            port.usb2 = proto & 2 != 0;
            port.usb3 = proto & 4 != 0;
            port.op_ss = flags & 1 != 0;
            port.cap_ss = flags & 2 != 0;
            port.op_ssp = flags & 4 != 0;
        }

        let mut ce = vec![0u8; 512];
        ce[0..4].copy_from_slice(&p.to_le_bytes());
        if ioctl(h.0, IOCTL_CONN_EX, &mut ce).is_some() {
            port.status = u32_at(&ce, 31);
            if port.status == 1 {
                let vid = u16_at(&ce, 12);
                let pid = u16_at(&ce, 14);
                let mut dk = vec![0u8; 512];
                dk[0..4].copy_from_slice(&p.to_le_bytes());
                let dev_instance = ioctl(h.0, IOCTL_DRIVERKEY, &mut dk).and_then(|ret| {
                    let units: Vec<u16> = dk[8..ret].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                    drivers.get(&from_wide(&units).to_lowercase()).cloned()
                });
                let reg_name = dev_instance.as_deref().and_then(device_name);
                let product = string_descriptor(h.0, p, ce[19]);
                let name = match (product, reg_name) {
                    (Some(prod), Some(reg)) if !reg.eq_ignore_ascii_case(&prod) && !reg.starts_with("USB Composite") && !reg.starts_with("Generic") => {
                        format!("{prod} ({reg})")
                    }
                    (Some(prod), _) => prod,
                    (None, Some(reg)) => reg,
                    (None, None) => "Unknown device".into(),
                };
                let name = match vendor(vid) {
                    Some(v) if name.starts_with("Generic") || name.starts_with("USB Composite") || name == "Unknown device" => {
                        format!("{name} ({v})")
                    }
                    _ => name,
                };
                port.conn = Some(Connection {
                    speed: ce[23],
                    vid,
                    pid,
                    is_hub: ce[24] != 0,
                    instance: dev_instance.map(|s| s.to_uppercase()),
                    name,
                });
            }
        }
        ports.push(port);
    }

    let controller = if is_root {
        parent_instance(&instance).and_then(|c| device_name(&c))
    } else {
        None
    };
    let name = if is_root {
        "Root hub".to_string()
    } else {
        device_name(&instance).unwrap_or_else(|| "USB hub".into())
    };
    Some(Hub { instance, name, is_root, bus_powered, controller, ports })
}

/// Vendors of common hub, dock and adapter chips, for devices Windows calls "Generic".
fn vendor(vid: u16) -> Option<&'static str> {
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
        _ => return None,
    })
}

fn status_text(s: u32) -> &'static str {
    match s {
        0 => "empty",
        1 => "connected",
        2 => "device failed enumeration",
        3 => "device general failure",
        4 => "device caused overcurrent",
        5 => "not enough power for device",
        6 => "not enough bandwidth for device",
        7 => "hubs nested too deeply",
        8 => "device in legacy hub",
        9 => "device enumerating",
        10 => "device reset",
        _ => "unknown status",
    }
}

fn speed_text(c: &Connection, on_ss_port: &Port) -> (String, bool) {
    if on_ss_port.usb3 && on_ss_port.op_ssp {
        return ("SuperSpeed+ 10 Gbps".into(), true);
    }
    if on_ss_port.usb3 && on_ss_port.op_ss {
        return ("SuperSpeed 5 Gbps".into(), true);
    }
    match c.speed {
        0 => ("Low-Speed 1.5 Mbps".into(), false),
        1 => ("Full-Speed 12 Mbps".into(), false),
        2 => ("High-Speed 480 Mbps".into(), false),
        3 => ("SuperSpeed 5 Gbps".into(), true),
        _ => ("unknown speed".into(), false),
    }
}

fn short_name(name: &str) -> String {
    let n = name.split(" (").next().unwrap_or(name);
    if n.len() > 40 {
        format!("{}…", &n[..40])
    } else {
        n.to_string()
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
                let sec = if p.usb3 {
                    p.companion.as_ref().and_then(|(h, n)| self.port(h, *n))
                } else {
                    None
                };
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
            let kind = if prim.type_c {
                "USB-C"
            } else if !prim.user {
                "internal"
            } else if hub.is_root {
                "USB-A"
            } else {
                ""
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
            let mut child: Option<(&Connection, &Port)> = None;
            if let Some(c) = ss_conn {
                let (sp, ss) = speed_text(c, prim);
                desc = format!("{} {} {}", green("●"), c.name, dim(format!("[{:04X}:{:04X}]", c.vid, c.pid)));
                desc.push_str(&format!("  {}", if ss { green(&sp) } else { yellow(&sp) }));
                if c.is_hub {
                    desc.push_str(&dim(" hub"));
                    if let Some(h2) = hs_conn.filter(|h| h.is_hub) {
                        desc.push_str(&dim(format!(" + USB 2.0 companion [{:04X}:{:04X}]", h2.vid, h2.pid)));
                    }
                    child = Some((c, prim));
                }
                self.record(c, prim, &sp, ss, chain, &label, &hub.name, ext_hubs, bus_pw);
            } else if let Some(c) = hs_conn {
                let port_for_speed = hs_port.unwrap_or(prim);
                let (sp, _) = speed_text(c, port_for_speed);
                let ss_capable = prim.usb3 && hs_port.map(|p| p.cap_ss).unwrap_or(false);
                desc = format!("{} {} {}", green("●"), c.name, dim(format!("[{:04X}:{:04X}]", c.vid, c.pid)));
                desc.push_str(&format!("  {}", if c.speed == 2 { sp.clone() } else { dim(&sp) }));
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
                    child = Some((c, port_for_speed));
                }
                self.record(c, port_for_speed, &sp, false, chain, &label, &hub.name, ext_hubs, bus_pw);
            } else {
                desc = dim("empty");
            }

            for p in std::iter::once(prim).chain(sec) {
                if p.status > 1 {
                    desc.push_str(&red(format!("  ✖ {}", status_text(p.status))));
                    self.issues.push(UsbIssue {
                        problem: true,
                        text: format!("{} {label}: {}", hub.name, status_text(p.status)),
                    });
                }
            }

            let label_col = format!("{label:<9} {kind:<8} {cap:<7}");
            self.lines.push(format!("{prefix}{branch}{} {desc}", dim(label_col)));

            if let Some((c, _)) = child {
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
        _port: &Port,
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
    let drivers = driver_map();
    let guid = GUID::from_u128(GUID_DEVINTERFACE_USB_HUB);
    let mut hubs: Vec<Hub> = interface_paths(&guid).iter().filter_map(|p| read_hub(p, &drivers)).collect();
    hubs.sort_by(|a, b| a.controller.cmp(&b.controller).then(a.instance.cmp(&b.instance)));

    let mut w = Walker {
        hubs: &hubs,
        by_instance: hubs.iter().enumerate().map(|(i, h)| (h.instance.as_str(), i)).collect(),
        lines: vec![],
        locations: HashMap::new(),
        issues: vec![],
        visited: HashSet::new(),
    };

    for (i, hub) in hubs.iter().enumerate().filter(|(_, h)| h.is_root) {
        let ctrl = hub.controller.clone().unwrap_or_else(|| "USB host controller".into());
        w.lines.push(bold(&ctrl));
        w.lines.push(format!("└─ {}", dim(format!("Root hub, {} ports", hub.ports.len()))));
        w.walk(i, "   ", &[], 0, false);
    }

    // Hubs that were not reached from a root hub (should be rare).
    let orphans: Vec<usize> = (0..hubs.len())
        .filter(|i| !w.visited.contains(i))
        .filter(|&i| hubs[i].ports.iter().any(|p| !w.is_secondary(p)))
        .collect();
    for i in orphans {
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
            cur = parent_instance(&cur)?.to_uppercase();
        }
        None
    }

    pub fn print(&self) {
        heading("USB topology");
        for l in &self.lines {
            println!("{l}");
        }
        println!(
            "{}",
            dim("Port n/m = a USB 3 port (n) and its USB 2.0 companion (m) on the same connector.")
        );
    }
}
