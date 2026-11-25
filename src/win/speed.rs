//! WinHTTP requests for the speed test (uses the system proxy and TLS stack).

use std::ffi::c_void;
use std::ptr::{null, null_mut};
use std::time::Instant;

use windows_sys::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
};

use super::wide;
use crate::speed::{HOST, USER_AGENT};

const ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const FLAG_SECURE: u32 = 0x0080_0000;
const QUERY_STATUS_CODE_NUMBER: u32 = 19 | 0x2000_0000;

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
pub fn request(verb: &str, path: &str, body: &[u8]) -> Result<(u32, u64, f64), String> {
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

