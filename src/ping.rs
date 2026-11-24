//! ICMP ping through the Windows ICMP API (no admin rights needed).

use std::net::Ipv4Addr;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho};

use crate::util::*;

#[derive(Debug, Clone)]
pub struct PingStats {
    pub label: String,
    pub target: Ipv4Addr,
    pub sent: u32,
    pub rtts: Vec<u32>,
}

impl PingStats {
    pub fn received(&self) -> u32 {
        self.rtts.len() as u32
    }
    pub fn loss_pct(&self) -> f64 {
        if self.sent == 0 {
            0.0
        } else {
            100.0 * (self.sent - self.received()) as f64 / self.sent as f64
        }
    }
    pub fn avg(&self) -> f64 {
        if self.rtts.is_empty() {
            0.0
        } else {
            self.rtts.iter().map(|&r| r as f64).sum::<f64>() / self.rtts.len() as f64
        }
    }
    pub fn min(&self) -> u32 {
        self.rtts.iter().copied().min().unwrap_or(0)
    }
    pub fn max(&self) -> u32 {
        self.rtts.iter().copied().max().unwrap_or(0)
    }
    /// Mean absolute difference between consecutive replies.
    pub fn jitter(&self) -> f64 {
        if self.rtts.len() < 2 {
            return 0.0;
        }
        let sum: f64 = self.rtts.windows(2).map(|w| (w[1] as f64 - w[0] as f64).abs()).sum();
        sum / (self.rtts.len() - 1) as f64
    }

    pub fn line(&self) -> String {
        let loss = self.loss_pct();
        let loss_s = format!("loss {loss:>3.0}%");
        let loss_s = if loss == 0.0 { green(loss_s) } else if loss < 3.0 { yellow(loss_s) } else { red(loss_s) };
        if self.rtts.is_empty() {
            return format!("{:<22} {:<15} {}  no replies", self.label, self.target, loss_s);
        }
        format!(
            "{:<22} {:<15} {}  min {:>3}  avg {:>6.1}  max {:>4}  jitter {:>5.1} ms",
            self.label,
            self.target,
            loss_s,
            self.min(),
            self.avg(),
            self.max(),
            self.jitter()
        )
    }
}

fn echo(handle: isize, target: Ipv4Addr, timeout_ms: u32) -> Option<u32> {
    let data = [0x61u8; 32];
    let mut reply = [0u8; 128];
    let dest = u32::from_ne_bytes(target.octets());
    let n = unsafe {
        IcmpSendEcho(
            handle as _,
            dest,
            data.as_ptr() as *const _,
            data.len() as u16,
            null(),
            reply.as_mut_ptr() as *mut _,
            reply.len() as u32,
            timeout_ms,
        )
    };
    if n == 0 {
        return None;
    }
    // ICMP_ECHO_REPLY: Address (u32), Status (u32), RoundTripTime (u32), ...
    let status = u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]);
    let rtt = u32::from_le_bytes([reply[8], reply[9], reply[10], reply[11]]);
    (status == 0).then_some(rtt)
}

/// Pings `count` times, one every `interval`.
pub fn ping(label: &str, target: Ipv4Addr, count: u32, interval: Duration) -> PingStats {
    let mut stats = PingStats { label: label.to_string(), target, sent: 0, rtts: vec![] };
    let h = unsafe { IcmpCreateFile() };
    if h == INVALID_HANDLE_VALUE {
        return stats;
    }
    for _ in 0..count {
        let start = Instant::now();
        stats.sent += 1;
        if let Some(rtt) = echo(h as isize, target, 1000) {
            stats.rtts.push(rtt);
        }
        if let Some(rest) = interval.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    unsafe { IcmpCloseHandle(h) };
    stats
}

/// Pings until `stop` is set; used to measure latency while a speed test runs.
pub fn ping_until(label: &str, target: Ipv4Addr, interval: Duration, stop: Arc<AtomicBool>) -> PingStats {
    let mut stats = PingStats { label: label.to_string(), target, sent: 0, rtts: vec![] };
    let h = unsafe { IcmpCreateFile() };
    if h == INVALID_HANDLE_VALUE {
        return stats;
    }
    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        stats.sent += 1;
        if let Some(rtt) = echo(h as isize, target, 2000) {
            stats.rtts.push(rtt);
        }
        if let Some(rest) = interval.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    unsafe { IcmpCloseHandle(h) };
    stats
}

/// Pings several targets at the same time.
pub fn ping_all(targets: &[(String, Ipv4Addr)], count: u32) -> Vec<PingStats> {
    let handles: Vec<_> = targets
        .iter()
        .cloned()
        .map(|(label, ip)| std::thread::spawn(move || ping(&label, ip, count, Duration::from_millis(250))))
        .collect();
    handles.into_iter().filter_map(|h| h.join().ok()).collect()
}

pub fn print(results: &[PingStats]) {
    heading("Ping");
    for r in results {
        println!("{}", r.line());
    }
}
