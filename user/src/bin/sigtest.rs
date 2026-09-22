//! sigtest — signal handling demo for GLM OS v0.5.
//!
//! Installs handlers for SIGUSR1 and SIGTERM, then idles. From the shell:
//!   kill -u <pid>  -> SIGUSR1: the handler prints and execution continues
//!                     (proof of frame surgery + sigreturn round trip)
//!   kill <pid>     -> SIGTERM: the handler shuts the task down gracefully
//!   kill -9 <pid>  -> SIGKILL: uncatchable, the kernel terminates us

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicU64, Ordering};

use glm_user::{exit, fmt_u64, getpid, sigaction, SIGTERM, SIGUSR1};
use glm_user::{sleep_ms, write};

static USR1_COUNT: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_usr1(_sig: u64) {
    let n = USR1_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let mut b = [0u8; 20];
    write("  [sigtest] SIGUSR1 caught in ring-3 handler (#");
    write(core::str::from_utf8(fmt_u64(n, &mut b)).unwrap_or("?"));
    write(") - frame surgery + sigreturn worked, still alive\n");
}

extern "C" fn on_term(_sig: u64) {
    write("  [sigtest] SIGTERM caught - shutting down gracefully\n");
    exit(0);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    write("[sigtest] pid ");
    write(core::str::from_utf8(fmt_u64(getpid(), &mut b)).unwrap_or("?"));
    write(": installing SIGUSR1 + SIGTERM handlers (sigaction 9)\n");
    sigaction(SIGUSR1, on_usr1);
    sigaction(SIGTERM, on_term);
    write("[sigtest] idling - try: kill -u <pid> | kill <pid> | kill -9 <pid>\n");
    loop {
        sleep_ms(1000);
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
