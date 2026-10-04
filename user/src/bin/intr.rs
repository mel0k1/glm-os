//! intr — v2.7 job-control proof program.
//!
//! Installs a SIGINT handler, then idles forever. From the shell:
//!   run INTR.ELF      -> then press Ctrl+C
//!
//! The kernel's jobd turns the console Ctrl+C into SIGINT for the
//! foreground task (this one). The default action would terminate with
//! 130 (128+2); here the handler catches it — frame surgery + sigreturn
//! round trip on the v2.7 delivery path — prints, and exits CLEANLY.

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use glm_user::{exit, fmt_u64, getpid, sigaction, sleep_ms, write, SIGINT};

static CAUGHT: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_int(_sig: u64) {
    let n = CAUGHT.fetch_add(1, Ordering::Relaxed) + 1;
    let mut b = [0u8; 20];
    write("  [intr] SIGINT caught in ring 3 (#");
    write(core::str::from_utf8(fmt_u64(n, &mut b)).unwrap_or("?"));
    write(") - job control delivery + frame surgery work, exiting cleanly\n");
    exit(0);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    write("[intr] pid ");
    write(core::str::from_utf8(fmt_u64(getpid(), &mut b)).unwrap_or("?"));
    write(": installing a SIGINT handler - press Ctrl+C to interrupt me\n");
    if sigaction(SIGINT, on_int) < 0 {
        write("[intr] sigaction(SIGINT) failed - kernel is older than v2.7?\n");
        exit(9);
    }
    loop {
        sleep_ms(500);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
