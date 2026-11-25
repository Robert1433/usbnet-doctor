//! ICMP echo on Linux without root: an unprivileged ICMP socket
//! (net.ipv4.ping_group_range), a raw socket when running as root, or the
//! system `ping` command as a last resort.

use std::net::Ipv4Addr;
use std::process::Command;
use std::time::{Duration, Instant};

enum Mode {
    /// SOCK_DGRAM/IPPROTO_ICMP: the kernel fills in the identifier and checksum.
    Dgram(libc::c_int),
    /// SOCK_RAW: replies come with their IP header.
    Raw(libc::c_int),
    Command,
}

pub struct Pinger {
    mode: Mode,
    id: u16,
    seq: u16,
}

fn checksum(b: &[u8]) -> u16 {
    let mut sum: u32 = b.chunks(2).map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32).sum();
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr_in {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = u32::from(ip).to_be();
    sa
}

impl Pinger {
    pub fn new() -> Result<Pinger, String> {
        let id = (std::process::id() & 0xffff) as u16;
        let dgram = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::IPPROTO_ICMP) };
        if dgram >= 0 {
            return Ok(Pinger { mode: Mode::Dgram(dgram), id, seq: 0 });
        }
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::IPPROTO_ICMP) };
        if raw >= 0 {
            return Ok(Pinger { mode: Mode::Raw(raw), id, seq: 0 });
        }
        match Command::new("ping").arg("-V").output() {
            Ok(_) => Ok(Pinger { mode: Mode::Command, id, seq: 0 }),
            Err(_) => Err("cannot ping: no ICMP socket allowed and no ping command".into()),
        }
    }

    /// One echo request; the round-trip time in ms, or None on timeout.
    pub fn echo(&mut self, target: Ipv4Addr, timeout_ms: u32) -> Option<f64> {
        self.seq = self.seq.wrapping_add(1);
        match self.mode {
            Mode::Dgram(fd) => self.socket_echo(fd, false, target, timeout_ms),
            Mode::Raw(fd) => self.socket_echo(fd, true, target, timeout_ms),
            Mode::Command => command_echo(target, timeout_ms),
        }
    }

    fn socket_echo(&self, fd: libc::c_int, raw: bool, target: Ipv4Addr, timeout_ms: u32) -> Option<f64> {
        let mut pkt = [0u8; 40];
        pkt[0] = 8; // echo request
        pkt[4..6].copy_from_slice(&self.id.to_be_bytes());
        pkt[6..8].copy_from_slice(&self.seq.to_be_bytes());
        for (i, b) in pkt[8..].iter_mut().enumerate() {
            *b = 0x61 + (i % 23) as u8;
        }
        let sum = checksum(&pkt);
        pkt[2..4].copy_from_slice(&sum.to_be_bytes());

        let sa = sockaddr(target);
        let start = Instant::now();
        let sent = unsafe {
            libc::sendto(
                fd,
                pkt.as_ptr() as *const _,
                pkt.len(),
                0,
                &sa as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if sent < 0 {
            return None;
        }
        let deadline = start + Duration::from_millis(timeout_ms as u64);
        let mut buf = [0u8; 1500];
        loop {
            let left = deadline.checked_duration_since(Instant::now())?;
            let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) };
            if ready <= 0 {
                return None;
            }
            let mut from: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let n = unsafe {
                libc::recvfrom(fd, buf.as_mut_ptr() as *mut _, buf.len(), 0, &mut from as *mut _ as *mut libc::sockaddr, &mut len)
            };
            if n <= 0 {
                continue;
            }
            let rtt = start.elapsed().as_secs_f64() * 1000.0;
            let data = &buf[..n as usize];
            let icmp = if raw { data.get(((data[0] & 0x0f) as usize) * 4..)? } else { data };
            if icmp.len() < 8 || u32::from_be(from.sin_addr.s_addr) != u32::from(target) {
                continue;
            }
            let seq = u16::from_be_bytes([icmp[6], icmp[7]]);
            let id_ok = !raw || u16::from_be_bytes([icmp[4], icmp[5]]) == self.id;
            if icmp[0] == 0 && seq == self.seq && id_ok {
                return Some(rtt);
            }
        }
    }
}

/// `ping -c 1` and its "time=1.23 ms".
fn command_echo(target: Ipv4Addr, timeout_ms: u32) -> Option<f64> {
    let secs = timeout_ms.div_ceil(1000).max(1);
    let out = Command::new("ping").args(["-n", "-c", "1", "-W", &secs.to_string(), &target.to_string()]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let t = text.split("time=").nth(1)?;
    t.split(|c: char| c != '.' && !c.is_ascii_digit()).next()?.parse().ok()
}

impl Drop for Pinger {
    fn drop(&mut self) {
        if let Mode::Dgram(fd) | Mode::Raw(fd) = self.mode {
            unsafe { libc::close(fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icmp_checksum() {
        // An echo request with id 1, seq 1 and no payload.
        let pkt = [8u8, 0, 0, 0, 0, 1, 0, 1];
        assert_eq!(checksum(&pkt), 0xf7fd);
    }
}
