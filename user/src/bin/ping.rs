//! ping — IPC channel demo. Two named channels (rendezvous keys):
//!   key 7 = downstream (ping -> pong), key 8 = upstream (pong -> ping).
//! Either task may start first: open() joins the same kernel channel and
//! send/recv block until the peer arrives.

#![no_std]
#![no_main]

use glm_user::{chan_open, chan_recv, chan_send, exit, fmt_u64, getpid, write};

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    let down = chan_open(7); // ping -> pong
    let up = chan_open(8);   // pong -> ping
    write("[ping  ] pid ");
    write(num(getpid(), &mut b));
    write(" joined channels ");
    write(num(down as u64, &mut b));
    write("(key 7) + ");
    write(num(up as u64, &mut b));
    write("(key 8)\n");

    for i in 1..=3u64 {
        let mut msg = [0u8; 40];
        let digits = num(i, &mut b);
        let head = b"ping-";
        msg[..head.len()].copy_from_slice(head);
        msg[head.len()..head.len() + digits.len()].copy_from_slice(digits.as_bytes());
        let tail = b" from ring 3";
        msg[head.len() + digits.len()..head.len() + digits.len() + tail.len()]
            .copy_from_slice(tail);
        let total = head.len() + digits.len() + tail.len();

        let n = chan_send(down as u64, &msg[..total]);
        write("[ping  ] sent ");
        write(num(n as u64, &mut b));
        write(" bytes: '");
        write(core::str::from_utf8(&msg[..total]).unwrap_or("?"));
        write("'\n");

        let mut reply = [0u8; 64];
        let got = chan_recv(up as u64, &mut reply);
        let got = got.max(0) as usize;
        write("[ping  ] got reply (");
        write(num(got as u64, &mut b));
        write(" bytes): '");
        let clean = core::str::from_utf8(&reply[..got]).unwrap_or("??");
        write(clean);
        write("'\n");
    }
    write("[ping  ] done - exit 0\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
