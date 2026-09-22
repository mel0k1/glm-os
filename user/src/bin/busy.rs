//! busy — a CPU-bound task for the SMP demo.
//!
//! `spawn /BIN/BUSY.ELF` (a few times!) from glmsh: each instance burns
//! CPU in a tight loop and prints a heartbeat. With several instances
//! running, `ps` shows them in RUN state on different CPUs at the same
//! time — real parallelism, not just interleaving.

#![no_std]
#![no_main]

use glm_user::{exit, fmt_u64, getpid, write};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let pid = getpid();
    let mut b = [0u8; 20];
    write("[busy] burning cpu, pid ");
    write(core::str::from_utf8(fmt_u64(pid, &mut b)).unwrap_or("?"));
    write("\n");

    let mut beats: u64 = 0;
    loop {
        // ~0.5-1 s of pure user-mode CPU burn under QEMU TCG.
        // spin_loop() compiles to `pause`; LLVM never removes it.
        for _ in 0..20_000_000u64 {
            core::hint::spin_loop();
        }
        beats += 1;
        let mut b1 = [0u8; 20];
        let mut b2 = [0u8; 20];
        write("[busy] pid ");
        write(core::str::from_utf8(fmt_u64(pid, &mut b1)).unwrap_or("?"));
        write(" heartbeat ");
        write(core::str::from_utf8(fmt_u64(beats, &mut b2)).unwrap_or("?"));
        write("\n");
        if beats >= 8 {
            write("[busy] done, exiting 0\n");
            exit(0);
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
