//! ntp — SNTP client (RFC 4330) entirely in ring 3 (v2.4).
//!
//!   NTP.ELF [server_ip] [udp_port]        (defaults: 10.0.2.2 123)
//!
//! The OS learned what time it is at boot (CMOS RTC, v2.4), but a wall
//! clock that has never talked to the network is just a first guess. This
//! program asks an NTP server over the v0.9 UDP socket syscalls and adjusts
//! the kernel clock with the new v2.4 `clock_set` syscall:
//!
//!   1. build a 48-byte SNTP request (LI=0, VN=4, Mode=3 -> first byte
//!      0x1B, everything else zero),
//!   2. send it from an ephemeral-ish local port, wait up to 2 s for the
//!      reply (v2.0 recvfrom-with-deadline; 3 attempts total),
//!   3. the Transmit Timestamp (seconds, big-endian, offset 40) is the
//!      server's time in the NTP era (since 1900); subtract the classic
//!      2208988800 offset to get the Unix epoch,
//!   4. clock_set() it, print before/after/offset.
//!
//! The kernel logs every accepted adjustment (`rtc: clock set from ring 3`)
//! so the test harness can verify the path without OCR.
//!
//! Exit codes: 0 synced | 1 usage | 2 bad ip | 3 bind failed | 4 no reply.

#![no_std]
#![no_main]

use glm_user::{
    clock_set, clock_time, exit, fmt_u64, net_bind, net_close, net_recvfrom_timeout, net_sendto,
    SrcAddr, write,
};

const NTP_UNIX_DELTA: u64 = 2_208_988_800; // 1900 -> 1970
const LOCAL_PORT: u16 = 5133; // any free UDP port works for SNTP
const TIMEOUT_MS: u64 = 2000;
const TRIES: usize = 3;

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

/// Parse dotted-quad "a.b.c.d" into a packed big-endian IPv4 (host order
/// as the kernel means it: (a<<24)|(b<<16)|(c<<8)|d).
fn parse_ip(s: &str) -> Option<u32> {
    let mut out: u32 = 0;
    let mut octets = 0;
    for part in s.split('.') {
        if octets >= 4 || part.is_empty() || part.len() > 3 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u32 = part.bytes().fold(0, |a, d| a * 10 + (d - b'0') as u32);
        if v > 255 {
            return None;
        }
        out = (out << 8) | v;
        octets += 1;
    }
    if octets != 4 {
        return None;
    }
    Some(out)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("[ntp      ] panic\n");
    exit(101)
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    // ---- argv ------------------------------------------------------------
    let mut ab = [0u8; 64];
    let server: &str = if argc > 1 {
        let p = unsafe { *argv.add(1) } as *const u8;
        glm_user::arg_str(glm_user::cstr_into(p, &mut ab))
    } else {
        "10.0.2.2"
    };
    let mut pb = [0u8; 64];
    let port: u16 = if argc > 2 {
        let p = unsafe { *argv.add(2) } as *const u8;
        let s = glm_user::arg_str(glm_user::cstr_into(p, &mut pb));
        match s.parse_u16() {
            Some(v) => v,
            None => {
                write("[ntp      ] bad port '");
                write(s);
                write("' - exit 1\n");
                exit(1);
            }
        }
    } else {
        123
    };

    let Some(ip) = parse_ip(server) else {
        write("[ntp      ] bad ip '");
        write(server);
        write("' - exit 2\n");
        exit(2);
    };

    // ---- the exchange ------------------------------------------------------
    let before = clock_time();
    if before < 0 {
        write("[ntp      ] kernel has no wall clock - exit 2\n");
        exit(2);
    }

    let id = net_bind(LOCAL_PORT);
    if id < 0 {
        write("[ntp      ] bind local port failed - exit 3\n");
        exit(3);
    }

    // SNTP request packet: LI(2)=0 VN(3)=4 Mode(3)=3 client -> 0b00_100_011
    let mut req = [0u8; 48];
    req[0] = 0x23;
    let mut rx = [0u8; 512];
    let mut got: Option<u64> = None;

    for attempt in 1..=TRIES {
        let n = net_sendto(id, ip, port, &req);
        if n < 0 {
            write("[ntp      ] sendto failed - exit 4\n");
            exit(4);
        }
        let mut src = SrcAddr::new();
        let r = net_recvfrom_timeout(id, &mut rx, &mut src, TIMEOUT_MS);
        if r >= 48 {
            // sanity: mode must be server (4) or broadcast (5); the LI/VN
            // nibbles are echoed by every honest server
            let mode = rx[0] & 0x07;
            if mode == 4 || mode == 5 {
                let secs = u32::from_be_bytes([rx[40], rx[41], rx[42], rx[43]]) as u64;
                if secs > NTP_UNIX_DELTA {
                    got = Some(secs - NTP_UNIX_DELTA);
                    break;
                }
            }
            write("[ntp      ] reply ignored (bad mode/era), retrying\n");
        } else {
            write("[ntp      ] attempt ");
            let mut b = [0u8; 20];
            write(num(attempt as u64, &mut b));
            write(": no reply in time\n");
        }
    }
    net_close(id);

    let Some(server_epoch) = got else {
        write("[ntp      ] no usable reply after 3 tries - exit 4\n");
        exit(4);
    };

    // ---- apply + report ----------------------------------------------------
    let set = clock_set(server_epoch);
    if set < 0 {
        write("[ntp      ] clock_set rejected - exit 4\n");
        exit(4);
    }
    let after = clock_time();

    let mut b = [0u8; 20];
    let (before, server_epoch, after) = (before, server_epoch as i64, after);
    let offset = server_epoch - before; // how far we were behind (+) / ahead (-)
    let skew = after - server_epoch;

    write("[ntp      ] server ");
    write(server);
    write(":");
    write(num(port as u64, &mut b));
    write(" -> epoch ");
    write(num(server_epoch as u64, &mut b));
    write("\n[ntp      ] local clock was ");
    write(num(before as u64, &mut b));
    write(" (offset ");
    write_num_signed(offset, &mut b);
    write(" s), now ");
    write(num(after as u64, &mut b));
    write(" (skew ");
    write_num_signed(skew, &mut b);
    write(" s)\n[ntp      ] wall clock synced over the network - exit 0\n");
    exit(0)
}

/// Signed decimal for offsets (tiny helper; fmt_u64 is unsigned only).
fn write_num_signed(v: i64, b: &mut [u8; 20]) {
    if v < 0 {
        write("-");
        write(num((-v as u64), b));
    } else {
        write("+");
        write(num(v as u64, b));
    }
}

/// u16 parse without core's Parse trait (no_std str::parse exists, but we
/// keep the dependency surface tiny and miri-simple).
trait ParseU16 {
    fn parse_u16(&self) -> Option<u16>;
}
impl ParseU16 for str {
    fn parse_u16(&self) -> Option<u16> {
        if self.is_empty() || self.len() > 5 || !self.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut v: u32 = 0;
        for d in self.bytes() {
            v = v * 10 + (d - b'0') as u32;
        }
        if v <= u16::MAX as u32 {
            Some(v as u16)
        } else {
            None
        }
    }
}
