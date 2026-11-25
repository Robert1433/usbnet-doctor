//! Ping: loss, latency and jitter. The backend sends the ICMP echoes
//! (Windows ICMP API, or an unprivileged ICMP socket on Linux).

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::sys::ping::Pinger;
use crate::util::*;

#[derive(Debug, Clone)]
pub struct PingStats {
    pub label: String,
    pub target: Ipv4Addr,
    pub sent: u32,
    /// Set when pinging could not start at all.
    pub error: Option<String>,
    /// Round-trip times of the replies, in milliseconds.
    pub rtts: Vec<f64>,
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
            self.rtts.iter().sum::<f64>() / self.rtts.len() as f64
        }
    }
    pub fn min(&self) -> f64 {
        self.rtts.iter().copied().reduce(f64::min).unwrap_or(0.0)
    }
    pub fn max(&self) -> f64 {
        self.rtts.iter().copied().reduce(f64::max).unwrap_or(0.0)
    }
    /// Mean absolute difference between consecutive replies.
    pub fn jitter(&self) -> f64 {
        if self.rtts.len() < 2 {
            return 0.0;
        }
        let sum: f64 = self.rtts.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
        sum / (self.rtts.len() - 1) as f64
    }

    pub fn line(&self) -> String {
        let loss = self.loss_pct();
        let loss_s = format!("loss {loss:>3.0}%");
        let loss_s = if loss == 0.0 { green(loss_s) } else if loss < 3.0 { yellow(loss_s) } else { red(loss_s) };
        if self.rtts.is_empty() {
            let why = self.error.as_deref().unwrap_or("no replies");
            return format!("{:<22} {:<15} {}  {why}", self.label, self.target, loss_s);
        }
        format!(
            "{:<22} {:<15} {}  min {:>5.1}  avg {:>6.1}  max {:>6.1}  jitter {:>5.1} ms",
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

/// Pings `count` times, one every `interval`.
pub fn ping(label: &str, target: Ipv4Addr, count: u32, interval: Duration) -> PingStats {
    let mut stats = PingStats { label: label.to_string(), target, sent: 0, error: None, rtts: vec![] };
    let mut pinger = match Pinger::new() {
        Ok(p) => p,
        Err(e) => {
            stats.sent = count;
            stats.error = Some(e);
            return stats;
        }
    };
    for _ in 0..count {
        let start = Instant::now();
        stats.sent += 1;
        if let Some(rtt) = pinger.echo(target, 1000) {
            stats.rtts.push(rtt);
        }
        if let Some(rest) = interval.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    stats
}

/// Pings until `stop` is set; used to measure latency while a speed test runs.
pub fn ping_until(label: &str, target: Ipv4Addr, interval: Duration, stop: Arc<AtomicBool>) -> PingStats {
    let mut stats = PingStats { label: label.to_string(), target, sent: 0, error: None, rtts: vec![] };
    let mut pinger = match Pinger::new() {
        Ok(p) => p,
        Err(e) => {
            stats.error = Some(e);
            return stats;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let start = Instant::now();
        stats.sent += 1;
        if let Some(rtt) = pinger.echo(target, 2000) {
            stats.rtts.push(rtt);
        }
        if let Some(rest) = interval.checked_sub(start.elapsed()) {
            std::thread::sleep(rest);
        }
    }
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
