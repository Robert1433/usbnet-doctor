//! HTTPS for the speed test through `curl`, which is on nearly every Linux
//! system and honours the usual proxy variables (https_proxy, no_proxy).

use std::io::Write;
use std::process::{Command, Stdio};

use crate::speed::{HOST, USER_AGENT};

/// One HTTPS request. Returns (status, bytes received, seconds spent after the
/// response headers arrived for GET, or the whole request for POST).
pub fn request(verb: &str, path: &str, body: &[u8]) -> Result<(u32, u64, f64), String> {
    let url = format!("https://{HOST}{path}");
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-o", "/dev/null", "--connect-timeout", "10", "--max-time", "120", "-A", USER_AGENT])
        .args(["-w", "%{http_code} %{size_download} %{time_starttransfer} %{time_total}"])
        .args(["-X", verb])
        .stdin(if body.is_empty() { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if !body.is_empty() {
        cmd.args(["-H", "Content-Type: application/octet-stream", "--data-binary", "@-"]);
    }
    cmd.arg(&url);

    let mut child = cmd.spawn().map_err(|e| format!("cannot run curl ({e}): install curl for the speed test"))?;
    if !body.is_empty() {
        let mut stdin = child.stdin.take().ok_or("curl: no stdin")?;
        // A write error means curl gave up; its own message says why.
        let _ = stdin.write_all(body);
    }
    let out = child.wait_with_output().map_err(|e| format!("curl: {e}"))?;
    let w = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = w.split_whitespace().collect();
    if !out.status.success() || f.len() < 4 {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim().trim_start_matches("curl: ");
        return Err(if err.is_empty() { format!("curl failed ({})", out.status) } else { err.to_string() });
    }
    let status: u32 = f[0].parse().unwrap_or(0);
    let bytes: u64 = f[1].parse().unwrap_or(0);
    let first_byte: f64 = f[2].parse().unwrap_or(0.0);
    let total: f64 = f[3].parse().unwrap_or(0.0);
    let secs = if body.is_empty() { total - first_byte } else { total };
    Ok((status, bytes, secs.max(1e-3)))
}
