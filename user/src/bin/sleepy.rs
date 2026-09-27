//! sleepy — v1.8: a DELAYED stdout writer for broken-pipe tests.
//!
//! Sleeps ~0.8 s, then writes three short lines to stdout. As a pipeline
//! stage whose reader exits immediately, the write hits a kernel pipe
//! with zero readers -> broken pipe -> write() reports -1 -> exit 1:
//!
//!   run SLEEPY.ELF | run ARGS.ELF   (ARGS exits at once; SLEEPY dies on
//!                                    the broken pipe with exit 1)

#![no_std]
#![no_main]

use glm_user::{exit, sleep_ms, write, write_bytes};

#[no_mangle]
pub extern "C" fn _start(_argc: i64, _argv: *const *const u8) -> ! {
    for i in 1..=3u64 {
        sleep_ms(800);
        if write("SLEEPY line ") < 0 {
            exit(1); // broken pipe: our stdout has no readers
        }
        let mut b = [0u8; 20];
        if write(core::str::from_utf8(glm_user::fmt_u64(i, &mut b)).unwrap_or("?")) < 0 {
            exit(1);
        }
        if write_bytes(b"\n") < 0 {
            exit(1);
        }
    }
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
