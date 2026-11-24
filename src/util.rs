//! Small helpers shared by every module: wide strings, registry lookups,
//! device-tree queries and terminal colors.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_Get_Parent, CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows_sys::Win32::Devices::Properties::DEVPROPKEY;
use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    STD_OUTPUT_HANDLE,
};
use winreg::enums::HKEY_LOCAL_MACHINE;
use winreg::RegKey;

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
    let name: String = k
        .get_value("FriendlyName")
        .or_else(|_| k.get_value("DeviceDesc"))
        .ok()?;
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
    let r = unsafe {
        CM_Get_DevNode_PropertyW(dev, &key, &mut ty, &mut ft as *mut _ as *mut u8, &mut size, 0)
    };
    if r != CR_SUCCESS {
        return None;
    }
    Some(Stamp(((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64))
}

// ---------- time ----------

const TICKS_PER_SEC: u64 = 10_000_000;
const DAYS_1601_TO_1970: i64 = 134_774;

/// A UTC point in time as a Windows FILETIME (100 ns ticks since 1601-01-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Stamp(pub u64);

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Stamp {
    pub fn secs_since(&self, earlier: Stamp) -> i64 {
        (self.0 as i64 - earlier.0 as i64) / TICKS_PER_SEC as i64
    }

    /// Parses event-log times like "2026-11-17T17:53:19.1234567Z" (UTC).
    pub fn parse_utc(s: &str) -> Option<Stamp> {
        let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
        let days = days_from_civil(n(0, 4)?, n(5, 7)?, n(8, 10)?) + DAYS_1601_TO_1970;
        let secs = days * 86_400 + n(11, 13)? * 3600 + n(14, 16)? * 60 + n(17, 19)?;
        let mut ticks = secs as u64 * TICKS_PER_SEC;
        if let Some(frac) = s.get(19..).and_then(|f| f.strip_prefix('.')) {
            let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).take(7).collect();
            if !digits.is_empty() {
                ticks += digits.parse::<u64>().ok()? * 10u64.pow(7 - digits.len() as u32);
            }
        }
        Some(Stamp(ticks))
    }

    fn local(&self) -> Option<SYSTEMTIME> {
        let ft = FILETIME { dwLowDateTime: self.0 as u32, dwHighDateTime: (self.0 >> 32) as u32 };
        let mut utc: SYSTEMTIME = unsafe { std::mem::zeroed() };
        let mut local: SYSTEMTIME = unsafe { std::mem::zeroed() };
        unsafe {
            if FileTimeToSystemTime(&ft, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
                return None;
            }
        }
        Some(local)
    }

    /// Local time, "YYYY-MM-DD HH:MM:SS".
    pub fn local_string(&self) -> String {
        match self.local() {
            Some(t) => format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
            ),
            None => "?".into(),
        }
    }

    /// Local time, "YYYY-MM-DD HH:MM".
    pub fn short(&self) -> String {
        let s = self.local_string();
        s.get(..16).unwrap_or(&s).to_string()
    }
}

pub fn fmt_bps(bps: f64) -> String {
    if bps >= 1e9 {
        format!("{:.2} Gbps", bps / 1e9)
    } else if bps >= 1e6 {
        format!("{:.0} Mbps", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} kbps", bps / 1e3)
    } else {
        format!("{bps:.0} bps")
    }
}

pub fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else if b >= 1e3 {
        format!("{:.1} kB", b / 1e3)
    } else {
        format!("{b:.0} B")
    }
}

// ---------- terminal colors ----------

static COLOR: AtomicBool = AtomicBool::new(false);

pub fn init_color(disabled: bool) {
    if disabled || std::env::var_os("NO_COLOR").is_some() || !std::io::stdout().is_terminal() {
        return;
    }
    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0u32;
        if GetConsoleMode(h, &mut mode) != 0
            && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        {
            COLOR.store(true, Ordering::Relaxed);
        }
    }
}

fn paint(code: &str, s: impl Display) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn red(s: impl Display) -> String {
    paint("31", s)
}
pub fn green(s: impl Display) -> String {
    paint("32", s)
}
pub fn yellow(s: impl Display) -> String {
    paint("33", s)
}
pub fn cyan(s: impl Display) -> String {
    paint("36", s)
}
pub fn dim(s: impl Display) -> String {
    paint("2", s)
}
pub fn bold(s: impl Display) -> String {
    paint("1", s)
}

pub fn heading(title: &str) {
    println!();
    println!("{}", bold(cyan(format!("== {title} =="))));
}
