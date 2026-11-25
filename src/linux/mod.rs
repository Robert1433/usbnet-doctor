//! Linux backend: sysfs for USB and network devices, ethtool ioctls for
//! driver settings, the kernel log through journalctl, unprivileged ICMP
//! sockets and curl for the speed test. Needs no root, although the kernel
//! log may need membership of the `adm` or `systemd-journal` group.

pub mod events;
pub mod net;
pub mod ping;
pub mod speed;
pub mod usb;

use std::fs;
use std::path::{Path, PathBuf};

use crate::util::Stamp;

pub const OS_NAME: &str = "Linux";

pub fn eee_hint(name: &str) -> String {
    let dev = name.split(' ').next().unwrap_or(name);
    format!(
        "{name}: Energy-Efficient Ethernet is on; turning it off can fix short drops and lag spikes (sudo ethtool --set-eee {dev} eee off)"
    )
}

pub fn suspend_hint(name: &str) -> String {
    format!(
        "{name}: USB autosuspend is on; if the adapter drops out, turn it off (write `on` to the device's power/control in sysfs, or use usbcore.autosuspend=-1)"
    )
}

/// Terminals on Linux understand ANSI colors as they are.
pub fn enable_ansi() -> bool {
    true
}

/// A terminal window does not close when a program exits on Linux, so there
/// is nothing to keep open.
pub fn own_console() -> bool {
    false
}

/// Local time as [year, month, day, hour, minute, second].
pub fn local_time(t: Stamp) -> Option<[u32; 6]> {
    let secs = t.unix_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return None;
    }
    Some([tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec].map(|v| v as u32))
}

// ---------- sysfs ----------

/// Root of sysfs. `USBNET_DOCTOR_SYSFS` points it at a copy, for tests.
pub fn sysfs() -> PathBuf {
    std::env::var_os("USBNET_DOCTOR_SYSFS").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/sys"))
}

/// A sysfs attribute, trimmed; None when missing, unreadable or empty.
pub fn attr(dir: &Path, name: &str) -> Option<String> {
    let s = fs::read_to_string(dir.join(name)).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

pub fn attr_num<T: std::str::FromStr>(dir: &Path, name: &str) -> Option<T> {
    attr(dir, name)?.parse().ok()
}

pub fn attr_hex(dir: &Path, name: &str) -> Option<u32> {
    u32::from_str_radix(attr(dir, name)?.trim_start_matches("0x"), 16).ok()
}

/// File name of the target of a symlink such as `device/driver`.
pub fn link_name(path: &Path) -> Option<String> {
    let t = fs::read_link(path).ok()?;
    Some(t.file_name()?.to_string_lossy().into_owned())
}

/// Name of a vendor or device from the pci.ids / usb.ids database, if installed.
pub fn id_name(db: &str, vendor: u32, device: Option<u32>) -> Option<String> {
    let paths = [
        format!("/usr/share/hwdata/{db}"),
        format!("/usr/share/misc/{db}"),
        format!("/usr/share/{db}"),
    ];
    let text = paths.iter().find_map(|p| fs::read(p).ok())?;
    let text = String::from_utf8_lossy(&text);
    let vhex = format!("{vendor:04x}");
    let mut vendor_name = None;
    for line in text.lines() {
        if vendor_name.is_none() {
            if let Some(rest) = line.strip_prefix(vhex.as_str()) {
                if rest.starts_with("  ") {
                    vendor_name = Some(rest.trim().to_string());
                    if device.is_none() {
                        break;
                    }
                }
            }
            continue;
        }
        // Device lines are indented by one tab; the vendor's block ends at the next unindented line.
        if !line.starts_with('\t') && !line.starts_with('#') && !line.is_empty() {
            break;
        }
        if let (Some(d), Some(rest)) = (device, line.strip_prefix('\t')) {
            if rest.starts_with(&format!("{d:04x}  ")) {
                return Some(format!("{} {}", vendor_name.as_deref().unwrap_or(""), rest[6..].trim()).trim().to_string());
            }
        }
    }
    vendor_name
}
