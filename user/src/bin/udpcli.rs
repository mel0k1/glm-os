//! udp-cli — userland UDP client (v0.9 demo).
//!
//! Sends three datagrams to the echo server on port 7777 via the kernel
//! loopback path (the destination is this machine's own IPv4) and verifies
//! every echo byte for byte. Exit 0 means: ring-3 programs can talk to
//! each other over the network stack.

#![no_std]
#![no_main]

use glm_user::{exit, fmt_u64, getpid, net_bind, net_close, net_info, net_recvfrom, net_sendto, sleep_ms, SrcAddr, write};

const SERVER_PORT: u16 = 7777;

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
    let our = net_info(0) as u32; // packed BE IPv4 of this machine
    let id = net_bind(7778);
    if id < 0 {
        write("[udp-cli ] bind 7778 failed - exit 1\n");
        exit(1);
    }
    write("[udp-cli ] pid ");
    write(num(getpid(), &mut b));
    write(" socket ");
    write(num(id as u64, &mut b));
    write(" bound port 7778, target ");
    write_ip(our, &mut b);
    write(":");
    write(num(SERVER_PORT as u64, &mut b));
    write(" (loopback)\n");

    // let a freshly spawned server reach its bind() first
    sleep_ms(800);

    let mut rbuf = [0u8; 256];
    for i in 1..=3u64 {
        // payload: "GLM-UDP-<i> ring3->kernel->loopback->kernel->ring3"
        let mut msg = [0u8; 64];
        let head = b"GLM-UDP-";
        let digits = num(i, &mut b);
        msg[..head.len()].copy_from_slice(head);
        msg[head.len()..head.len() + digits.len()].copy_from_slice(digits.as_bytes());
        let tail = b" ring3->loopback->ring3";
        msg[head.len() + digits.len()..head.len() + digits.len() + tail.len()]
            .copy_from_slice(tail);
        let total = head.len() + digits.len() + tail.len();

        let n = net_sendto(id, our, SERVER_PORT, &msg[..total]);
        if n < 0 {
            write("[udp-cli ] sendto failed - exit 3\n");
            exit(3);
        }
        write("[udp-cli ] sent ");
        write(num(n as u64, &mut b));
        write(" bytes: '");
        write(core::str::from_utf8(&msg[..total]).unwrap_or("?"));
        write("'\n");

        let mut src = SrcAddr::new();
        let got = net_recvfrom(id, &mut rbuf, &mut src);
        if got < 0 {
            write("[udp-cli ] recv failed - exit 4\n");
            exit(4);
        }
        let same = got as usize == total && &rbuf[..total] == &msg[..total];
        write("[udp-cli ] reply from ");
        write_ip(src.ip(), &mut b);
        write(":");
        write(num(src.port() as u64, &mut b));
        write(" (");
        write(num(got as u64, &mut b));
        write(" bytes): ");
        if same {
            write("MATCH\n");
        } else {
            write("MISMATCH - exit 5\n");
            exit(5);
        }
    }
    write("[udp-cli ] 3/3 round trips verified\n");
    let _ = net_close(id);
    write("[udp-cli ] socket closed - exit 0\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
