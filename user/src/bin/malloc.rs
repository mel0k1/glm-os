//! malloc — v2.1 heap torture test, entirely in ring 3.
//!
//! Drives glm_user::heap through the patterns an allocator must survive
//! and reports each phase on stdout ("heap: phase N ok"). The kernel
//! prints the exit code, so the test harness machine-checks:
//!   0        all phases passed
//!   10 + N   phase N failed (N = 1..)
//!
//! Phases:
//!   1  single allocations of mixed sizes, stamped, verified
//!   2  free half, realloc the rest to double size, verify copies
//!   3  churn: 200 alloc/free rounds over a 16-slot window (coalescing)
//!   4  one 1 MiB block (forces multi-page morecore), sparse write/read
//!   5  calloc zeroing (reused dirty blocks must come back clean)
//!   6  refusal: a 200 MiB request must return null, and the allocator
//!      must keep working afterwards
//!   7  sbrk(0) reports a break strictly inside the arena

#![no_std]
#![no_main]

use glm_user::{exit, heap, write};

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}

fn ok(n: u64) {
    write("heap: phase ");
    putnum(n);
    write(" ok\n");
}

fn putnum(v: u64) {
    let mut b = [0u8; 20];
    write(core::str::from_utf8(glm_user::fmt_u64(v, &mut b)).unwrap_or("?"));
}

fn fail(phase: u64, why: &str) -> ! {
    write("heap: FAIL phase ");
    putnum(phase);
    write(": ");
    write(why);
    write("\n");
    exit(10 + phase as i64)
}

/// Stamp every 256th byte of a buffer with a pattern derived from the
/// slot id, then verify it back. `skip_zero` lets calloc phases accept
/// pristine memory.
fn stamp(buf: *mut u8, len: u64, seed: u64) {
    let mut off = 0u64;
    while off < len {
        unsafe {
            *buf.add(off as usize) = (seed ^ off) as u8 | 1;
        }
        off += 256;
    }
}

fn verify(buf: *mut u8, len: u64, seed: u64, phase: u64, what: &str) {
    let mut off = 0u64;
    while off < len {
        let want = (seed ^ off) as u8 | 1;
        let got = unsafe { *buf.add(off as usize) };
        if got != want {
            fail(phase, what);
        }
        off += 256;
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("MALLOC: ring-3 heap torture over SYS_SBRK (int 0x80 #53)\n");

    // --- phase 1: mixed single allocations ----------------------------
    let sizes: [u64; 8] = [8, 24, 100, 256, 512, 1024, 4096, 16384];
    let mut ptrs = [core::ptr::null_mut::<u8>(); 8];
    for i in 0..8 {
        ptrs[i] = heap::malloc(sizes[i]);
        if ptrs[i].is_null() {
            fail(1, "malloc returned null");
        }
        let p = ptrs[i] as u64;
        if p & 0xF != 0 {
            fail(1, "payload not 16-aligned");
        }
        stamp(ptrs[i], sizes[i], 0x5A00 + i as u64);
    }
    for i in 0..8 {
        verify(ptrs[i], sizes[i], 0x5A00 + i as u64, 1, "stamp corrupted");
    }
    ok(1);

    // --- phase 2: free half, realloc the rest --------------------------
    for i in (0..8).step_by(2) {
        unsafe { heap::free(ptrs[i]) };
        ptrs[i] = core::ptr::null_mut();
    }
    for i in (1..8).step_by(2) {
        let grown = unsafe { heap::realloc(ptrs[i], sizes[i] * 2) };
        if grown.is_null() {
            fail(2, "realloc returned null");
        }
        // realloc must have PRESERVED the old stamps
        verify(grown, sizes[i], 0x5A00 + i as u64, 2, "realloc lost data");
        stamp(grown, sizes[i] * 2, 0x5A00 + i as u64);
        ptrs[i] = grown;
    }
    ok(2);

    // --- phase 3: churn over a fixed window (coalescing under stress) --
    let mut win: [*mut u8; 16] = [core::ptr::null_mut(); 16];
    let mut round = 0u64;
    while round < 200 {
        let slot = (round % 16) as usize;
        if win[slot].is_null() {
            let n = 32 + (round * 13) % 4096;
            win[slot] = heap::malloc(n);
            if win[slot].is_null() {
                fail(3, "churn malloc null");
            }
            stamp(win[slot], n, 0x3300 ^ round);
            verify(win[slot], n, 0x3300 ^ round, 3, "churn stamp corrupted");
        } else {
            unsafe { heap::free(win[slot]) };
            win[slot] = core::ptr::null_mut();
        }
        round += 1;
    }
    for s in 0..16 {
        if !win[s].is_null() {
            unsafe { heap::free(win[s]) };
        }
    }
    ok(3);

    // --- phase 4: one 1 MiB block (multi-page morecore) -----------------
    let big_len: u64 = 1024 * 1024;
    let big = heap::malloc(big_len);
    if big.is_null() {
        fail(4, "1 MiB malloc null");
    }
    stamp(big, big_len, 0x77);
    verify(big, big_len, 0x77, 4, "big block corrupted");
    unsafe { heap::free(big) };
    ok(4);

    // --- phase 5: calloc zeroing over reused (dirty) memory -------------
    // phase 4 just freed 1 MiB, so the list hands back dirty pages
    for i in 0..32u64 {
        let p = heap::calloc(512, 8);
        if p.is_null() {
            fail(5, "calloc null");
        }
        for off in (0..4096).step_by(64) {
            let got = unsafe { *p.add(off as usize) };
            if got != 0 {
                fail(5, "calloc returned dirty memory");
            }
        }
        stamp(p, 4096, 0x99 + i);
        unsafe { heap::free(p) };
    }
    ok(5);

    // --- phase 6: refusal of an oversized request -----------------------
    let huge = heap::malloc(200 * 1024 * 1024);
    if !huge.is_null() {
        fail(6, "200 MiB request was NOT refused");
    }
    // the allocator must still work after the refusal
    let p = heap::malloc(128);
    if p.is_null() {
        fail(6, "allocator dead after refusal");
    }
    stamp(p, 128, 0x11);
    verify(p, 128, 0x11, 6, "post-refusal corruption");
    unsafe { heap::free(p) };
    ok(6);

    // --- phase 7: sbrk(0) reports a sane break ---------------------------
    match heap::sbrk(0) {
        Some(b) if b > 0x2000_0000 && b < 0x3000_0000 => {}
        _ => fail(7, "break outside the arena"),
    }
    ok(7);

    write("heap: all phases passed\n");
    exit(0)
}
