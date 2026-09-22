//! tcp-cli — userland TCP client (v1.3 demo).
//!
//! Usage baked in: connects to the slirp host 10.0.2.2 port 7300, sends
//! one message, reads the echo back, verifies byte-for-byte, closes.
//! Exit 0 means a full TCP handshake + data round trip through the real
//! network stack (ARP -> IP -> TCP -> slirp -> host).

#![no_std]
#![no_main]

use glm_user::{fmt_u64, tcp_close, tcp_connect, tcp_recv, tcp_send, write};

const HOST_IP: u32 = 0x0A00_0202; // 10.0.2.2 (slirp gateway = the host)
const HOST_PORT: u16 = 7300;
const MSG: &[u8] = b"hello from glm-os tcp - ring 3 speaking";

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("[tcp-cli ] panic\n");
    glm_user::exit(101)
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    let mut buf = [0u8; 1400];

    let id = tcp_connect(HOST_IP, HOST_PORT);
    if id < 0 {
        write("[tcp-cli ] connect 10.0.2.2:7300 failed - exit 1\n");
        glm_user::exit(1);
    }
    write("[tcp-cli ] established, id ");
    write(num(id as u64, &mut b));
    write("\n");

    // send the message (loop: send takes at most one MSS)
    let mut sent = 0usize;
    while sent < MSG.len() {
        let r = tcp_send(id, &MSG[sent..]);
        if r < 0 {
            write("[tcp-cli ] send failed - exit 2\n");
            glm_user::exit(2);
        }
        sent += r as usize;
    }
    write("[tcp-cli ] sent ");
    write(num(MSG.len() as u64, &mut b));
    write(" bytes\n");

    // read the echo back
    let mut got: usize = 0;
    while got < MSG.len() {
        let n = tcp_recv(id, &mut buf);
        if n < 0 {
            write("[tcp-cli ] recv failed - exit 3\n");
            glm_user::exit(3);
        }
        if n == 0 {
            write("[tcp-cli ] EOF early - exit 4\n");
            glm_user::exit(4);
        }
        for i in 0..n as usize {
            if buf[i] != MSG[got + i] {
                write("[tcp-cli ] echo mismatch - exit 5\n");
                glm_user::exit(5);
            }
        }
        got += n as usize;
    }
    write("[tcp-cli ] echo verified ");
    write(num(got as u64, &mut b));
    write(" bytes\n");

    let _ = tcp_close(id);
    write("[tcp-cli ] closed, exit 0\n");
    glm_user::exit(0)
}
