//! Windows USB backend: the hub IOCTLs that Microsoft's USBView uses.

use std::collections::HashMap;
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

use super::{device_name, from_wide, parent_instance, wide};
use crate::usb::{vendor, Connection, Connector, Hub, Port};
use crate::util::Stamp;

const GUID_DEVINTERFACE_USB_HUB: u128 = 0xf18a0e88_c30c_11d0_8815_00a0c906bed8;

const IOCTL_NODE_INFO: u32 = 0x22_0408;
const IOCTL_DESCRIPTOR: u32 = 0x22_0410;
const IOCTL_DRIVERKEY: u32 = 0x22_0420;
const IOCTL_CONN_EX: u32 = 0x22_0448;
const IOCTL_PORT_PROPS: u32 = 0x22_0458;
const IOCTL_CONN_EX_V2: u32 = 0x22_045C;

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
        let mut port = Port::new(p);

        let mut pp = vec![0u8; 512];
        pp[0..4].copy_from_slice(&p.to_le_bytes());
        if let Some(ret) = ioctl(h.0, IOCTL_PORT_PROPS, &mut pp) {
            let flags = u32_at(&pp, 8);
            port.connector = if flags & 8 != 0 {
                Connector::TypeC
            } else if flags & 1 != 0 {
                Connector::External
            } else {
                Connector::Internal
            };
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
            let status = u32_at(&ce, 31);
            if status > 1 {
                port.problem = Some(status_text(status).to_string());
            }
            if status == 1 {
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

/// Every USB hub in the system with its ports.
pub fn hubs() -> Vec<Hub> {
    let drivers = driver_map();
    let guid = GUID::from_u128(GUID_DEVINTERFACE_USB_HUB);
    interface_paths(&guid).iter().filter_map(|p| read_hub(p, &drivers)).collect()
}

pub fn parent(instance: &str) -> Option<String> {
    parent_instance(instance)
}

pub fn last_arrival(instance: &str) -> Option<Stamp> {
    super::last_arrival(instance)
}
