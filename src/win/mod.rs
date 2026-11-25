//! Windows backend: SetupAPI/CfgMgr, hub IOCTLs, IP Helper, the event log,
//! the ICMP API and WinHTTP. Also the small Win32 helpers they share.

pub mod events;
pub mod net;
pub mod ping;
pub mod speed;
pub mod usb;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetConsoleProcessList, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    STD_OUTPUT_HANDLE,
};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

use crate::util::Stamp;

pub const OS_NAME: &str = "Windows";

pub fn eee_hint(name: &str) -> String {
    format!("{name}: Energy-Efficient Ethernet is on; turning it off can fix short drops and lag spikes")
}

pub fn suspend_hint(name: &str) -> String {
    format!("{name}: USB selective suspend is on; if the adapter drops out, turn it off")
}

/// Turns on ANSI escape handling in the console.
pub fn enable_ansi() -> bool {
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        GetConsoleMode(h, &mut mode) != 0 && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
    }
}

/// True when this process is alone in its console, i.e. Windows opened the
/// console just for it (double-click in Explorer). The window would then
/// close as soon as we exit, so we keep it open instead.
pub fn own_console() -> bool {
    let mut ids = [0u32; 4];
    let n = unsafe { GetConsoleProcessList(ids.as_mut_ptr(), ids.len() as u32) };
    n == 1
}

/// Local time as [year, month, day, hour, minute, second].
pub fn local_time(t: Stamp) -> Option<[u32; 6]> {
    let ft = FILETIME { dwLowDateTime: t.0 as u32, dwHighDateTime: (t.0 >> 32) as u32 };
    let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
    let mut l: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut l) == 0 {
            return None;
        }
    }
    Some([l.wYear, l.wMonth, l.wDay, l.wHour, l.wMinute, l.wSecond].map(u32::from))
}

// ---------- wide strings and the device tree ----------

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Registry device descriptions look like "@usb.inf,%desc%;USB Composite Device".
pub fn strip_inf(s: &str) -> String {
    match s.rfind(';') {
        Some(i) => s[i + 1..].to_string(),
        None => s.to_string(),
    }
}

pub fn enum_key(instance: &str) -> Option<RegKey> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"SYSTEM\CurrentControlSet\Enum\{instance}"))
        .ok()
}

const DEVPKEY_DEVICE: u128 = 0xa45c254e_df1c_4efd_8020_67d146a850e0;
const PID_FRIENDLY_NAME: u32 = 14;
const PID_DEVICE_DESC: u32 = 2;

fn devnode_string(dev: u32, pid: u32) -> Option<String> {
    let key = DEVPROPKEY { fmtid: windows_sys::core::GUID::from_u128(DEVPKEY_DEVICE), pid };
    let mut ty = 0u32;
    let mut buf = vec![0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe { CM_Get_DevNode_PropertyW(dev, &key, &mut ty, buf.as_mut_ptr() as *mut u8, &mut size, 0) };
    if r != CR_SUCCESS {
        return None;
    }
    let s = from_wide(&buf);
    (!s.is_empty()).then_some(s)
}

/// The resolved name Device Manager shows (handles "%1 ... %2" style descriptions).
fn devnode_name(instance: &str) -> Option<String> {
    let dev = locate(instance)?;
    devnode_string(dev, PID_FRIENDLY_NAME).or_else(|| devnode_string(dev, PID_DEVICE_DESC))
}

/// Friendly name of a device instance.
pub fn device_name(instance: &str) -> Option<String> {
    if let Some(n) = devnode_name(instance) {
        return Some(n);
    }
    let k = enum_key(instance)?;
    let name: String = k.get_value("FriendlyName").or_else(|_| k.get_value("DeviceDesc")).ok()?;
    Some(strip_inf(&name))
}

pub fn device_service(instance: &str) -> Option<String> {
    enum_key(instance)?.get_value("Service").ok()
}

fn locate(instance: &str) -> Option<u32> {
    let mut dev = 0u32;
    let w = wide(instance);
    let r = unsafe { CM_Locate_DevNodeW(&mut dev, w.as_ptr(), 0) };
    (r == CR_SUCCESS).then_some(dev)
}

fn devnode_id(dev: u32) -> Option<String> {
    let mut buf = vec![0u16; 512];
    let r = unsafe { CM_Get_Device_IDW(dev, buf.as_mut_ptr(), buf.len() as u32, 0) };
    (r == CR_SUCCESS).then(|| from_wide(&buf))
}

pub fn parent_instance(instance: &str) -> Option<String> {
    let dev = locate(instance)?;
    let mut parent = 0u32;
    let r = unsafe { CM_Get_Parent(&mut parent, dev, 0) };
    if r != CR_SUCCESS {
        return None;
    }
    devnode_id(parent)
}

const DEVPKEY_DEVICE_LAST_ARRIVAL: (u128, u32) = (0x83da6326_97a6_4088_9453_a1923f573b29, 102);

/// When the device was last plugged in, if Windows recorded it.
pub fn last_arrival(instance: &str) -> Option<Stamp> {
    let dev = locate(instance)?;
    let key = DEVPROPKEY {
        fmtid: windows_sys::core::GUID::from_u128(DEVPKEY_DEVICE_LAST_ARRIVAL.0),
        pid: DEVPKEY_DEVICE_LAST_ARRIVAL.1,
    };
    let mut ty = 0u32;
    let mut ft = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut size = std::mem::size_of::<FILETIME>() as u32;
    let r = unsafe { CM_Get_DevNode_PropertyW(dev, &key, &mut ty, &mut ft as *mut _ as *mut u8, &mut size, 0) };
    if r != CR_SUCCESS {
        return None;
    }
    Some(Stamp(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64))
}
