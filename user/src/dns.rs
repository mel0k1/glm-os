//! dns — a tiny RFC 1035 stub resolver living entirely in ring 3 (v2.0).
//!
//! The userland program builds the query, ships it in one UDP datagram
//! to the resolver address (SYS_NET_INFO subop 2 — slirp's forwarder
//! 10.0.2.3 in the default QEMU configuration) and parses the first A
//! record out of the answer. The kernel only moves bytes; every DNS
//! field is userland's business.
//!
//! Failure is BOUNDED: a lost query times out, a name that does not
//! exist comes back as an error code — resolve() can never hang.

use crate::{
    getpid, net_bind, net_close, net_info, net_recvfrom_timeout, net_sendto, SrcAddr, uptime_ms,
};

const QTYPE_A: u16 = 1; // host address
const QCLASS_IN: u16 = 1; // the Internet
const RES_PORT: u16 = 53;
const ATTEMPTS: usize = 3; // retransmit budget per lookup
const TIMEOUT_MS: u64 = 1500; // per attempt

/// Error codes (all negative, all bounded):
pub const E_BIND: i64 = -1; // could not grab a source port
pub const E_TIMEOUT: i64 = -2; // no answer after ATTEMPTS x TIMEOUT_MS
pub const E_NO_A: i64 = -3; // answer parsed but carries no A record
pub const E_NAME: i64 = -4; // empty / too long / bad label
pub const E_NXDOMAIN: i64 = -5; // rcode 3: the name does not exist
pub const E_SERVFAIL: i64 = -6; // any other non-zero rcode

/// "a.b.c.d" for a packed BE address, into a caller-provided scratch.
pub fn ip_to_str(ip: u32, buf: &mut [u8; 16]) -> &str {
    let o = [
        (ip >> 24 & 0xFF) as u8,
        (ip >> 16 & 0xFF) as u8,
        (ip >> 8 & 0xFF) as u8,
        (ip & 0xFF) as u8,
    ];
    let mut i = 0usize;
    for (k, v) in o.iter().enumerate() {
        if k > 0 {
            buf[i] = b'.';
            i += 1;
        }
        if *v >= 100 {
            buf[i] = b'0' + v / 100;
            i += 1;
            buf[i] = b'0' + (v / 10 % 10);
            i += 1;
        } else if *v >= 10 {
            buf[i] = b'0' + v / 10;
            i += 1;
        }
        buf[i] = b'0' + (v % 10);
        i += 1;
    }
    core::str::from_utf8(&buf[..i]).unwrap_or("?")
}

/// Human text for the error codes above.
pub fn err_str(e: i64) -> &'static str {
    match e {
        E_BIND => "bind failed",
        E_TIMEOUT => "timeout",
        E_NO_A => "no A record",
        E_NAME => "bad name",
        E_NXDOMAIN => "nxdomain",
        E_SERVFAIL => "servfail",
        _ => "?",
    }
}

/// Resolve `name` to its first IPv4 A record.
///
/// A dotted quad ("10.0.2.2") short-circuits and returns itself — the
/// caller does not need a second parser. Real names go over the wire.
pub fn resolve(name: &str) -> Result<u32, i64> {
    if let Some(ip) = dotted(name) {
        return Ok(ip);
    }
    let name = name.strip_suffix('.').unwrap_or(name);
    if name.is_empty() || name.len() > 253 {
        return Err(E_NAME);
    }

    // ---- build the query ------------------------------------------------
    let mut q = [0u8; 300];
    let mut id: u16 = (uptime_ms() as u16) ^ (getpid() as u16);
    let mut p = 12usize; // the fixed header
    q[0..2].copy_from_slice(&id.to_be_bytes());
    q[2..4].copy_from_slice(&0x0100u16.to_be_bytes()); // RD=1: recurse please
    q[4..6].copy_from_slice(&1u16.to_be_bytes()); // QDCOUNT=1
    for label in name.split('.') {
        let l = label.len();
        if l == 0 || l > 63 {
            return Err(E_NAME);
        }
        if p + 1 + l + 5 > q.len() {
            return Err(E_NAME);
        }
        q[p] = l as u8;
        q[p + 1..p + 1 + l].copy_from_slice(label.as_bytes());
        p += 1 + l;
    }
    q[p] = 0; // end of QNAME
    p += 1;
    q[p..p + 2].copy_from_slice(&QTYPE_A.to_be_bytes());
    q[p + 2..p + 4].copy_from_slice(&QCLASS_IN.to_be_bytes());
    p += 4;
    let qlen = p;

    // ---- grab a source port (kernel has no ephemeral bind yet) ----------
    let base = 4096u16 + (getpid() % 2048) as u16;
    let mut sid: i64 = -1;
    for k in 0..8u16 {
        sid = net_bind(base + k);
        if sid >= 0 {
            break;
        }
    }
    if sid < 0 {
        return Err(E_BIND);
    }

    let dns_ip = net_info(2) as u32;
    let mut rbuf = [0u8; 512]; // the classic UDP-DNS answer budget
    let mut src = SrcAddr::new();

    let mut outcome = Err(E_TIMEOUT);
    for _ in 0..ATTEMPTS {
        // re-stamp the id every attempt so a stale late answer from the
        // previous try can never be mistaken for the current one
        id = id.rotate_left(7) ^ (uptime_ms() as u16);
        q[0..2].copy_from_slice(&id.to_be_bytes());
        if net_sendto(sid, dns_ip, RES_PORT, &q[..qlen]) < 0 {
            continue; // transient TX failure: burn the attempt, retry
        }
        let n = net_recvfrom_timeout(sid, &mut rbuf, &mut src, TIMEOUT_MS);
        if n < 12 {
            continue; // timeout (-3) or a junk short packet
        }
        if src.ip() != dns_ip || src.port() != RES_PORT {
            continue; // off-brand sender: ignore, keep the budget
        }
        let n = n as usize;
        if rbuf[0] != (id >> 8) as u8 || rbuf[1] != (id & 0xFF) as u8 {
            continue; // id mismatch
        }
        let flags = u16::from_be_bytes([rbuf[2], rbuf[3]]);
        if flags & 0x8000 == 0 {
            continue; // QR=0: this is a query, not a response
        }
        match flags & 0x000F {
            0 => {}
            3 => {
                outcome = Err(E_NXDOMAIN);
                break;
            }
            rc => {
                let _ = rc;
                outcome = Err(E_SERVFAIL);
                break;
            }
        }

        let ancount = u16::from_be_bytes([rbuf[6], rbuf[7]]) as usize;
        // ---- skip the question section ---------------------------------
        let mut q = 12usize;
        while q < n && rbuf[q] != 0 {
            q += 1 + rbuf[q] as usize;
        }
        q += 5; // the zero byte + QTYPE + QCLASS
        // ---- walk the answers ------------------------------------------
        let mut found = Err(E_NO_A);
        for _ in 0..ancount {
            if q + 2 > n {
                break;
            }
            if rbuf[q] & 0xC0 == 0xC0 {
                q += 2; // compressed name pointer
            } else {
                while q < n && rbuf[q] != 0 {
                    q += 1 + rbuf[q] as usize;
                }
                q += 1;
            }
            if q + 10 > n {
                break;
            }
            let rtype = u16::from_be_bytes([rbuf[q], rbuf[q + 1]]);
            let rdlen = u16::from_be_bytes([rbuf[q + 8], rbuf[q + 9]]) as usize;
            if rtype == QTYPE_A && rdlen == 4 && q + 10 + 4 <= n {
                let ip = u32::from_be_bytes([
                    rbuf[q + 10],
                    rbuf[q + 11],
                    rbuf[q + 12],
                    rbuf[q + 13],
                ]);
                found = Ok(ip);
                break;
            }
            q += 10 + rdlen;
        }
        outcome = found;
        break;
    }
    let _ = net_close(sid);
    outcome
}

/// Parse a strict dotted quad. Returns None on anything else.
fn dotted(s: &str) -> Option<u32> {
    let mut ip: u32 = 0;
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 || part.is_empty() || part.len() > 3 {
            return None;
        }
        let mut v: u32 = 0;
        for c in part.bytes() {
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + (c - b'0') as u32;
        }
        if v > 255 {
            return None;
        }
        ip = (ip << 8) | v;
        n += 1;
    }
    if n != 4 {
        return None;
    }
    Some(ip)
}
