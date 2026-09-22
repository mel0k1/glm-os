//! loop — a long-lived background task for the multitasking demo.
//!
//! `spawn /BIN/LOOP.ELF` from glmsh: this task keeps running (one tick
//! per second via SYS_SLEEP) while the shell stays interactive — type
//! `ps` to see both tasks, `kill <pid>` to end it.

#![no_std]
#![no_main]

use glm_user::{exit, fmt_u64, getpid, syscall1, write, SYS_SLEEP};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("[loop] background task alive, ticking once per second\n");
    let mut n: u64 = 0;
    loop {
        syscall1(SYS_SLEEP, 1000);
        let mut b1 = [0u8; 20];
        let mut b2 = [0u8; 20];
        write("[loop] tick ");
        write(core::str::from_utf8(fmt_u64(n, &mut b1)).unwrap_or("?"));
        write(" (pid ");
        write(core::str::from_utf8(fmt_u64(getpid(), &mut b2)).unwrap_or("?"));
        write(")\n");
        n += 1;
        if n >= 60 {
            // stay well-behaved: exit on our own after a minute
            write("[loop] 60 ticks served, exiting 0\n");
            exit(0);
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
