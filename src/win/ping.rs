//! Windows ICMP API (no admin rights needed).

use std::net::Ipv4Addr;
use std::ptr::null;

use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::NetworkManagement::IpHelper::{IcmpCloseHandle, IcmpCreateFile, IcmpSendEcho};

pub struct Pinger(HANDLE);

impl Pinger {
    pub fn new() -> Result<Pinger, String> {
        let h = unsafe { IcmpCreateFile() };
        if h == INVALID_HANDLE_VALUE {
            return Err(format!("cannot ping: {}", std::io::Error::last_os_error()));
        }
        Ok(Pinger(h))
    }

    /// One echo request; the round-trip time in ms, or None on timeout.
    pub fn echo(&mut self, target: Ipv4Addr, timeout_ms: u32) -> Option<f64> {
        let data = [0x61u8; 32];
        let mut reply = [0u8; 128];
        let dest = u32::from_ne_bytes(target.octets());
        let n = unsafe {
            IcmpSendEcho(
                self.0,
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
        (status == 0).then_some(rtt as f64)
    }
}

impl Drop for Pinger {
    fn drop(&mut self) {
        unsafe { IcmpCloseHandle(self.0) };
    }
}
