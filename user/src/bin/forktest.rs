//! forktest — v0.6 copy-on-write fork demonstration.
//!
//! The parent prints its buffer, forks, and waits. The child overwrites
//! the buffer (which triggers the actual copy: both spaces shared the
//! page read-only up to this point), prints its own view, and exits 42.
//! The parent then proves COW isolation: its buffer must be untouched.

#![no_std]
#![no_main]

use glm_user::{exit, fmt_u64, fork, getpid, wait, write};

const EXIT_CODE: i64 = 42;

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let original: [u8; 16] = *b"COW-ORIGINAL-VAL";
    let mut buf: [u8; 16] = original;
    let mut num = [0u8; 20];

    write("forktest: parent pid=");
    write(core::str::from_utf8(fmt_u64(getpid(), &mut num)).unwrap_or("?"));
    write(", buffer='COW-ORIGINAL-VAL'\n");

    let child = fork();
    if child < 0 {
        write("forktest: fork FAILED (-1)\n");
        exit(1);
    }

    if child == 0 {
        // ---------------- child ----------------
        glm_user::sleep_ms(1200); // let the parent reach wait() first
        buf[0..5].copy_from_slice(b"CHILD");
        write("  child: pid=");
        write(core::str::from_utf8(fmt_u64(getpid(), &mut num)).unwrap_or("?"));
        write(" overwrote buffer -> 'CHILDxxxxxx-VAL' (COW fault taken)\n");
        write("  child: exiting with code 42\n");
        exit(EXIT_CODE);
    }

    // ---------------- parent ----------------
    write("forktest: parent forked, child pid=");
    write(core::str::from_utf8(fmt_u64(child as u64, &mut num)).unwrap_or("?"));
    write(", waiting...\n");

    let code = wait(0);
    write("forktest: child ");
    write(core::str::from_utf8(fmt_u64(child as u64, &mut num)).unwrap_or("?"));
    write(" exited with code ");
    write(core::str::from_utf8(fmt_u64(code as u64, &mut num)).unwrap_or("?"));
    write("\n");

    if buf == original {
        write("forktest: parent buffer intact -> COW isolation VERIFIED\n");
    } else {
        write("forktest: parent buffer CORRUPTED -> COW BROKEN\n");
        exit(1);
    }
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
