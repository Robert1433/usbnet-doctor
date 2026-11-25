//! Small helpers shared by every module: timestamps, number formatting and
//! terminal colors.

use std::fmt::Display;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::sys::usb::last_arrival;

// ---------- time ----------

pub const TICKS_PER_SEC: u64 = 10_000_000;
const DAYS_1601_TO_1970: i64 = 134_774;
/// 100 ns ticks between 1601-01-01 and 1970-01-01.
#[cfg_attr(windows, allow(dead_code))]
pub const UNIX_EPOCH_TICKS: u64 = DAYS_1601_TO_1970 as u64 * 86_400 * TICKS_PER_SEC;

/// A UTC point in time as 100 ns ticks since 1601-01-01 (a Windows FILETIME).
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

#[cfg_attr(windows, allow(dead_code))]
impl Stamp {
    pub fn now() -> Stamp {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Stamp::from_unix_micros(d.as_micros() as u64)
    }

    pub fn from_unix_micros(us: u64) -> Stamp {
        Stamp(UNIX_EPOCH_TICKS + us * 10)
    }

    /// Seconds since 1970-01-01 (UTC).
    pub fn unix_secs(&self) -> i64 {
        (self.0 as i64 - UNIX_EPOCH_TICKS as i64).div_euclid(TICKS_PER_SEC as i64)
    }

    pub fn secs_since(&self, earlier: Stamp) -> i64 {
        (self.0 as i64 - earlier.0 as i64) / TICKS_PER_SEC as i64
    }

    /// Parses event-log times like "2026-11-17T17:53:19.1234567Z" (UTC).
    #[cfg_attr(not(windows), allow(dead_code))]
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

    /// Local time, "YYYY-MM-DD HH:MM:SS".
    pub fn local_string(&self) -> String {
        match crate::sys::local_time(*self) {
            Some([y, mo, d, h, mi, s]) => format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}"),
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
    if crate::sys::enable_ansi() {
        COLOR.store(true, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_roundtrip() {
        let s = Stamp::parse_utc("1970-01-02T00:00:00Z").unwrap();
        assert_eq!(s.unix_secs(), 86_400);
        assert_eq!(Stamp::from_unix_micros(86_400_000_000), s);
    }
}
