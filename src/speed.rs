//! Download/upload throughput against Cloudflare's speed-test endpoints,
//! with latency measured while the download runs (to spot bufferbloat).
//! HTTP goes through WinHTTP, so it uses the system proxy and TLS stack.

use std::ffi::c_void;
use std::net::Ipv4Addr;
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};

use crate::ping::{ping_until, PingStats};
use crate::util::*;

const HOST: &str = "speed.cloudflare.com";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; usbnet-doctor/0.1)";
const ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const FLAG_SECURE: u32 = 0x0080_0000;
const QUERY_STATUS_CODE_NUMBER: u32 = 19 | 0x2000_0000;

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

struct H(*mut c_void);
impl Drop for H {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

fn last_error(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::last_os_error())
}

/// One HTTPS request. Returns (status, bytes received, seconds spent after the
/// response headers arrived for GET, or the whole request for POST).
fn request(verb: &str, path: &str, body: &[u8]) -> Result<(u32, u64, f64), String> {
    unsafe {
        let ua = wide(USER_AGENT);
        let session = H(WinHttpOpen(ua.as_ptr(), ACCESS_TYPE_AUTOMATIC_PROXY, null(), null(), 0));
        if session.0.is_null() {
            return Err(last_error("WinHttpOpen"));
        }
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 60_000, 60_000);
        let host = wide(HOST);
        let conn = H(WinHttpConnect(session.0, host.as_ptr(), 443, 0));
        if conn.0.is_null() {
            return Err(last_error("connect"));
        }
        let (v, p) = (wide(verb), wide(path));
        let req = H(WinHttpOpenRequest(conn.0, v.as_ptr(), p.as_ptr(), null(), null(), null(), FLAG_SECURE));
        if req.0.is_null() {
            return Err(last_error("open request"));
        }
        let headers = wide("Content-Type: application/octet-stream\r\n");
        let (hptr, hlen) = if body.is_empty() { (null(), 0) } else { (headers.as_ptr(), u32::MAX) };
        let start = Instant::now();
        if WinHttpSendRequest(
            req.0,
            hptr,
            hlen,
            if body.is_empty() { null() } else { body.as_ptr() as *const c_void },
            body.len() as u32,
            body.len() as u32,
            0,
        ) == 0
        {
            return Err(last_error("send"));
        }
        if WinHttpReceiveResponse(req.0, null_mut()) == 0 {
            return Err(last_error("receive"));
        }
        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(req.0, QUERY_STATUS_CODE_NUMBER, null(), &mut status as *mut u32 as *mut c_void, &mut len, null_mut());

        let body_start = Instant::now();
        let mut buf = vec![0u8; 256 * 1024];
        let mut total = 0u64;
        loop {
            let mut read = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read) == 0 {
                return Err(last_error("read"));
            }
            if read == 0 {
                break;
            }
            total += read as u64;
        }
        let secs = if body.is_empty() { body_start.elapsed() } else { start.elapsed() }.as_secs_f64();
        Ok((status, total, secs.max(1e-3)))
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
                "Latency while downloading: avg {:.1} ms, max {} ms, loss {:.0}%{}",
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
