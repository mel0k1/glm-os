//! udp-serv — userland UDP echo server (v0.9 demo).
//!
//! Binds port 7777 and echoes every datagram back to its sender using the
//! source address reported by recvfrom. Runs forever until SIGTERM: the
//! handler closes the socket, the blocked recvfrom retry loop then sees
//! -1 and the server exits gracefully — sockets and signals cooperate.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use glm_user::{exit, fmt_u64, getpid, net_bind, net_close, net_recvfrom, net_sendto, sigaction, SrcAddr, SIGTERM, write};

static CLOSED: AtomicBool = AtomicBool::new(false);
static SERV_ID: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_term(_sig: u64) {
    // close from inside the handler: the recvfrom retry loop then sees -1
    let _ = net_close(SERV_ID.load(Ordering::Relaxed) as i64);
    CLOSED.store(true, Ordering::Relaxed);
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

/// Write "a.b.c.d" for a packed BE address straight to the console.
fn write_ip(ip: u32, b: &mut [u8; 20]) {
    let octets = [
        (ip >> 24 & 0xFF) as u64,
        (ip >> 16 & 0xFF) as u64,
        (ip >> 8 & 0xFF) as u64,
        (ip & 0xFF) as u64,
    ];
    for (i, o) in octets.iter().enumerate() {
        if i > 0 {
            write(".");
        }
        write(num(*o, b));
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    let id = net_bind(7777);
    if id < 0 {
        write("[udp-serv] bind 7777 failed (port taken?) - exit 1\n");
        exit(1);
    }
    SERV_ID.store(id as u64, Ordering::Relaxed);
    sigaction(SIGTERM, on_term);

    write("[udp-serv] pid ");
    write(num(getpid(), &mut b));
    write(" socket ");
    write(num(id as u64, &mut b));
    write(" bound on port 7777 - echoing datagrams\n");

    let mut buf = [0u8; 256];
    loop {
        let mut src = SrcAddr::new();
        let n = net_recvfrom(id, &mut buf, &mut src);
        if n < 0 {
            if CLOSED.load(Ordering::Relaxed) {
                write("[udp-serv] socket closed by signal - goodbye\n");
                exit(0);
            }
            write("[udp-serv] recv error - exit 2\n");
            exit(2);
        }
        write("[udp-serv] got ");
        write(num(n as u64, &mut b));
        write(" bytes from ");
        write_ip(src.ip(), &mut b);
        write(":");
        write(num(src.port() as u64, &mut b));
        write(": '");
        write(core::str::from_utf8(&buf[..n as usize]).unwrap_or("??"));
        write("'\n");

        let r = net_sendto(id, src.ip(), src.port(), &buf[..n as usize]);
        write("[udp-serv] echoed ");
        write(num(r.max(0) as u64, &mut b));
        write(" bytes back\n");
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
