//! Download/upload throughput against Cloudflare's speed-test endpoints,
//! with latency measured while the download runs (to spot bufferbloat).
//! The backend does the HTTPS (WinHTTP on Windows, curl on Linux), so the
//! system proxy settings apply.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::ping::{ping_until, PingStats};
use crate::sys::speed::request;
use crate::util::*;

pub const HOST: &str = "speed.cloudflare.com";
pub const USER_AGENT: &str = concat!("Mozilla/5.0 (compatible; usbnet-doctor/", env!("CARGO_PKG_VERSION"), ")");

pub struct SpeedReport {
    pub download_bps: Vec<f64>,
    pub upload_bps: Option<f64>,
    pub loaded_ping: Option<PingStats>,
    pub errors: Vec<String>,
}

impl SpeedReport {
    pub fn best_download(&self) -> Option<f64> {
        self.download_bps.iter().copied().reduce(f64::max)
    }
}

fn download(bytes: u64) -> Result<f64, String> {
    let (status, total, secs) = request("GET", &format!("/__down?bytes={bytes}"), &[])?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    if total < bytes / 2 {
        return Err(format!("download ended early ({} of {})", fmt_bytes(total), fmt_bytes(bytes)));
    }
    Ok(total as f64 * 8.0 / secs)
}

fn upload(bytes: u64) -> Result<f64, String> {
    let body = vec![0u8; bytes as usize];
    let (status, _, secs) = request("POST", "/__up", &body)?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(bytes as f64 * 8.0 / secs)
}

pub fn run(mb: u64, rounds: u32, ping_target: Option<Ipv4Addr>) -> SpeedReport {
    let mut report = SpeedReport { download_bps: vec![], upload_bps: None, loaded_ping: None, errors: vec![] };
    let bytes = mb.max(1) * 1_000_000;

    let stop = Arc::new(AtomicBool::new(false));
    let pinger = ping_target.map(|ip| {
        let stop = stop.clone();
        std::thread::spawn(move || ping_until("under load", ip, Duration::from_millis(200), stop))
    });
    for _ in 0..rounds.max(1) {
        match download(bytes) {
            Ok(bps) => report.download_bps.push(bps),
            Err(e) => report.errors.push(format!("download: {e}")),
        }
    }
    stop.store(true, Ordering::Relaxed);
    report.loaded_ping = pinger.and_then(|h| h.join().ok());

    match upload((bytes / 2).max(5_000_000)) {
        Ok(bps) => report.upload_bps = Some(bps),
        Err(e) => report.errors.push(format!("upload: {e}")),
    }
    report
}

pub fn print(r: &SpeedReport, idle: Option<&PingStats>) {
    heading("Speed test (speed.cloudflare.com)");
    if !r.download_bps.is_empty() {
        let runs: Vec<String> = r.download_bps.iter().map(|b| fmt_bps(*b)).collect();
        println!(
            "Download  {}   {}",
            bold(fmt_bps(r.best_download().unwrap_or(0.0))),
            dim(format!("runs: {}", runs.join(", ")))
        );
    }
    if let Some(u) = r.upload_bps {
        println!("Upload    {}", bold(fmt_bps(u)));
    }
    if let Some(p) = &r.loaded_ping {
        if !p.rtts.is_empty() {
            let base = idle.map(|i| format!("  (idle avg {:.1} ms)", i.avg())).unwrap_or_default();
            println!(
                "Latency while downloading: avg {:.1} ms, max {:.0} ms, loss {:.0}%{}",
                p.avg(),
                p.max(),
                p.loss_pct(),
                dim(base)
            );
        }
    }
    for e in &r.errors {
        println!("{}", red(e));
    }
}
