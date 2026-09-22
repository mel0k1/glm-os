//! threadtest — v0.7 threads demonstration.
//!
//! The main thread spawns TWO worker threads via clone(): they live in the
//! SAME address space (one PML4), each on its own user stack. Both workers
//! bump one SHARED counter under a userland spinlock — on SMP this happens
//! in genuine parallel, so without the lock the counter would race. Each
//! worker sleeps between batches (long enough to catch `ps` showing the
//! thread states: main JOIN / workers SLEEP, same TGID, same PML4).
//!
//! The workers finish with thread_exit(100+id); the main thread joins both,
//! verifies the exact exit codes and the final counter, and exits 0/1.
//! Ground truth lands in the kernel log ("thread N exited with code M").

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use glm_user::{clone, exit, fmt_u64, getpid, join, sleep_ms, thread_exit, write};

const ITERS: u64 = 4000; // counter bumps per worker
const BATCH: u64 = 250; // ...between sleeps
const SLEEP_MS: u64 = 200;

/// userland spinlock (atomic exchange — cmpxchg under the hood)
static LOCK: AtomicBool = AtomicBool::new(false);
/// the shared piece of memory both threads write: same pages, one PML4
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 64 KiB stacks for the workers, in .bss of the shared image
#[repr(align(16))]
struct Align16([u8; 64 * 1024]);
static mut STACK1: Align16 = Align16([0; 64 * 1024]);
static mut STACK2: Align16 = Align16([0; 64 * 1024]);

fn put_u64(v: u64) {
    let mut num = [0u8; 20];
    write(core::str::from_utf8(fmt_u64(v, &mut num)).unwrap_or("?"));
}

/// TLS demo: each worker installs its OWN FS base onto a per-thread block,
/// then reads its id back FS-relative (mov rax, fs:[0]).
/// repr(C) is MANDATORY: Rust may otherwise reorder fields, moving self_id
/// away from offset 0 (this exact bug shipped in the first v0.7 build —
/// the compiler placed the 32-byte pad first, so fs:[0] read zeros).
#[repr(C, align(16))]
struct Tls {
    self_id: u64, // MUST stay at offset 0 (fs:[0])
    pad: [u8; 32],
}
static mut TLS1: Tls = Tls { self_id: 1, pad: [0; 32] };
static mut TLS2: Tls = Tls { self_id: 2, pad: [0; 32] };

extern "C" fn worker(arg: u64) -> ! {
    let id = arg;
    let tls_ptr: u64 = if id == 1 {
        unsafe { &raw mut TLS1 as u64 }
    } else {
        unsafe { &raw mut TLS2 as u64 }
    };
    if glm_user::set_fs(tls_ptr) != 0 {
        write("worker: set_fs FAILED\n");
        thread_exit(200);
    }
    // read the id back through the FS segment: proves the kernel really
    // switched FS.BASE for this thread (fs:[0] -> self_id)
    let seen: u64;
    unsafe {
        core::arch::asm!(
            "mov {0}, fs:[0]",
            out(reg) seen,
            options(nostack, nomem)
        );
    }

    write("[worker ");
    put_u64(id);
    write("] started on shared image, TLS self_id=");
    put_u64(seen);
    write("\n");
    if seen != id {
        write("[worker] TLS MISMATCH -> exiting 3\n");
        thread_exit(3);
    }

    let mut last = 0u64;
    for batch in 0..(ITERS / BATCH) {
        for _ in 0..BATCH {
            while LOCK.swap(true, Ordering::Acquire) {
                core::hint::spin_loop();
            }
            let v = COUNTER.load(Ordering::Relaxed);
            COUNTER.store(v + 1, Ordering::Relaxed);
            LOCK.store(false, Ordering::Release);
        }
        last = (batch + 1) * BATCH;
        sleep_ms(SLEEP_MS); // widen the ps window; also yields the CPU
    }

    write("[worker ");
    put_u64(id);
    write("] done: counter at ");
    put_u64(COUNTER.load(Ordering::Relaxed));
    write(" (batch ");
    put_u64(last);
    write("), thread_exit(");
    put_u64(100 + id);
    write(")\n");
    thread_exit(100 + id as i64);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut num = [0u8; 20];
    write("threadtest: main pid=");
    write(core::str::from_utf8(fmt_u64(getpid(), &mut num)).unwrap_or("?"));
    write(", spawning 2 threads on the SAME address space\n");

    let (s1, s2) = unsafe {
        (
            &raw const STACK1 as u64 + 64 * 1024 - 8,
            &raw const STACK2 as u64 + 64 * 1024 - 8,
        )
    };
    let t1 = clone(worker as u64, s1, 1);
    let t2 = clone(worker as u64, s2, 2);
    if t1 < 0 || t2 < 0 {
        write("threadtest: clone FAILED -> exit 2\n");
        exit(2);
    }
    write("threadtest: tids ");
    put_u64(t1 as u64);
    write(" and ");
    put_u64(t2 as u64);
    write(" running (ps: same TGID, same PML4, different stacks)\n");

    // catch-able window: workers bump + sleep for a few seconds; ps here
    // shows main JOIN / workers SLEEP, all with identical PML4
    let c1 = join(t1 as u64);
    let c2 = join(t2 as u64);
    let total = COUNTER.load(Ordering::SeqCst);

    write("threadtest: join(t1) -> ");
    put_u64(c1 as u64);
    write(", join(t2) -> ");
    put_u64(c2 as u64);
    write(", counter = ");
    put_u64(total);
    write(" (expect ");
    put_u64(2 * ITERS);
    write(")\n");

    if c1 == 101 && c2 == 102 && total == 2 * ITERS {
        write("threadtest: THREADS VERIFIED (shared memory + spinlock + join + TLS)\n");
        exit(0);
    }
    write("threadtest: VERIFICATION FAILED\n");
    exit(1);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
