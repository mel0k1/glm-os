//! heapfork — v2.1: the malloc'd heap must be per-process after fork().
//!
//! The parent allocates a buffer over SYS_SBRK and stamps it, forks, and
//! the child OVERWRITES its copy of the buffer. Because fork_cow shares
//! sbrk pages like any other user page, the child's writes must fault
//! into private frames — the parent's view must stay untouched. This is
//! the CoW machinery proving it covers the new heap region.
//!
//! Exit codes: 0 = clean; 20 = child saw corruption; 30 = parent's
//! buffer changed after the child ran; 31 = fork itself failed.

#![no_std]
#![no_main]

use glm_user::{exit, fork, heap, wait, write};

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}

const LEN: u64 = 64 * 1024;

fn stamp(buf: *mut u8, byte: u8) {
    let mut off = 0u64;
    while off < LEN {
        unsafe { *buf.add(off as usize) = byte };
        off += 1024;
    }
}

fn check(buf: *mut u8, byte: u8) -> bool {
    let mut off = 0u64;
    while off < LEN {
        if unsafe { *buf.add(off as usize) } != byte {
            return false;
        }
        off += 1024;
    }
    true
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("HEAPFORK: heap isolation across fork (CoW over sbrk pages)\n");

    let buf = heap::malloc(LEN);
    if buf.is_null() {
        write("heapfork: malloc failed\n");
        exit(31);
    }
    stamp(buf, 0xA5);

    let pid = fork();
    if pid < 0 {
        write("heapfork: fork failed\n");
        exit(31);
    }
    if pid == 0 {
        // child: scribble over OUR copy of the heap buffer
        stamp(buf, 0x5C);
        if !check(buf, 0x5C) {
            exit(20);
        }
        exit(0);
    }

    // parent: wait, then make sure OUR view still reads 0xA5
    let code = wait(pid as u64);
    if code != 0 {
        write("heapfork: child reported corruption\n");
        exit(code);
    }
    if !check(buf, 0xA5) {
        write("heapfork: parent buffer aliased by the child!\n");
        exit(30);
    }
    write("heapfork: child wrote its copy, parent still sees its own\n");
    exit(0)
}
