//! tcp-serv — userland TCP echo server (v1.3 demo).
//!
//! Listens on port 7301, accepts one connection at a time, echoes every
//! byte back until the peer closes (recv -> 0), then waits for the next
//! one. Exit 0 after two served connections (so the test harness sees a
//! clean end even if the host client keeps the port).

#![no_std]
#![no_main]

use glm_user::{fmt_u64, tcp_accept, tcp_close, tcp_listen, tcp_recv, tcp_send, write};

const PORT: u16 = 7301;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("[tcp-serv ] panic\n");
    glm_user::exit(101)
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    let mut buf = [0u8; 1400];

    let lid = tcp_listen(PORT);
    if lid < 0 {
        write("[tcp-serv ] listen 7301 failed - exit 1\n");
        glm_user::exit(1);
    }
    write("[tcp-serv ] listening on 7301 (listener id ");
    write(num(lid as u64, &mut b));
    write(")\n");

    for round in 0..2u64 {
        let id = tcp_accept(lid);
        if id < 0 {
            write("[tcp-serv ] accept failed - exit 2\n");
            glm_user::exit(2);
        }
        write("[tcp-serv ] round ");
        write(num(round, &mut b));
        write(" connection id ");
        write(num(id as u64, &mut b));
        write("\n");

        let mut total: u64 = 0;
        loop {
            let n = tcp_recv(id, &mut buf);
            if n < 0 {
                write("[tcp-serv ] recv error\n");
                break;
            }
            if n == 0 {
                write("[tcp-serv ] peer closed (EOF), total echoed ");
                write(num(total, &mut b));
                write(" bytes\n");
                break;
            }
            total += n as u64;
            let mut sent = 0usize;
            while sent < n as usize {
                let r = tcp_send(id, &buf[sent..n as usize]);
                if r < 0 {
                    write("[tcp-serv ] send error\n");
                    break;
                }
                sent += r as usize;
            }
        }
        let _ = tcp_close(id);
    }
    let _ = tcp_close(lid);
    write("[tcp-serv ] done, exit 0\n");
    glm_user::exit(0)
}
