//! v2.1: `sbrk` — the ring-3 heap.
//!
//! Until now every ring-3 program had to pre-reserve fixed buffers in
//! BSS: WGET carries a 64 KiB static receive buffer, FILES a 2 KiB
//! listing buffer, the GUI apps fixed string arenas. That is the classic
//! embedded bargain, and it caps every future userland program (an
//! editor, a file manager with dynamic listings, HTTP bodies over one
//! static size). SYS_SBRK closes it: the kernel grows a per-process
//! heap arena one page at a time, and the malloc in glm-user builds a
//! real allocator on top.
//!
//! Layout (see mem::vmm for the constants):
//!   USER_HEAP_BASE = 0x2000_0000   — first byte the heap may use
//!   USER_HEAP_MAX  = 0x3000_0000   — hard ceiling (256 MiB of arena)
//! The image sits at 4 MiB, the stack at the top of the low half — the
//! arena can never collide with either.
//!
//! ABI (int 0x80, number 53):
//!   rdi = increment (i64, may be negative)
//!   returns the OLD break (start of the newly won region) or -1.
//!   Classic semantics: sbrk(0) reports the current break without
//!   changing it.
//!
//! Semantics kept honestly small:
//!   * grow: every page of [align_up(old), align_up(new)) is mapped
//!     fresh — zeroed (no recycled-frame junk leaks into ring 3),
//!     writable, user-accessible, and NOT executable (the userland
//!     heap never holds code);
//!   * shrink: whole pages strictly above the new break are unmapped
//!     and released; the tail of a partially used page stays mapped
//!     (invisible to the caller — the break never tells on it);
//!   * fork: the child inherits the break value, and the heap pages
//!     themselves go through the regular COW machinery (fork_cow
//!     downgrades every writable user page, sbrk pages included);
//!   * exec: a fresh image starts over at USER_HEAP_BASE;
//!   * threads: the break lives in the Task, so a clone snapshots the
//!     creator's value. Two threads calling sbrk concurrently serialize
//!     on a kernel spinlock, but the VALUES they see can be stale —
//!     malloc itself is single-threaded in this userland, documented.
//!
//! Failure atomicity: a grow that runs out of frames (or hits a mapping
//! error mid-way) rolls back every page it already added and returns -1
//! with the break untouched.

use crate::klog;
use crate::mem::frames;
use crate::mem::paging::{phys_to_virt, cr3};
use crate::mem::vmm::{
    align_up, AddressSpace, NO_EXECUTE, PAGE, PRESENT, USER, USER_HEAP_BASE, USER_HEAP_MAX,
    WRITABLE,
};
use crate::sched;
use crate::sync::Spinlock;

/// Serializes break updates across CPUs (and against fork snapshots).
/// The critical section only walks page tables and bumps a word — no
/// blocking, no other locks taken, so a preempted holder simply resumes
/// later; nobody can spin forever.
static HEAP_LOCK: Spinlock<()> = Spinlock::new(());

/// The syscall body: sbrk(increment) -> old break or -1.
pub fn sys_sbrk(inc: i64) -> i64 {
    if !sched::online() {
        return -1;
    }
    let _g = HEAP_LOCK.lock();

    let stored = sched::current_heap_break();
    // 0 = never initialized (dead/kernel slots); a user task always
    // carries a real value, but treat anything bogus as the arena base.
    let old = if stored < USER_HEAP_BASE {
        USER_HEAP_BASE
    } else {
        stored
    };

    let Some(new_break) = old.checked_add_signed(inc) else {
        klog!("sbrk: overflow increment {} (break {:#x})", inc, old);
        return -1;
    };
    if new_break < USER_HEAP_BASE || new_break > USER_HEAP_MAX {
        klog!(
            "sbrk: break {:#x} outside the arena ({:#x}..{:#x})",
            new_break,
            USER_HEAP_BASE,
            USER_HEAP_MAX
        );
        return -1;
    }

    // The interrupt came from ring 3, so CR3 is the caller's table set.
    let space = AddressSpace::from_pml4(cr3());

    if new_break > old {
        // --- grow: add fresh zeroed RW/U/NX pages ---------------------
        let end = align_up(new_break);
        let mut p = align_up(old);
        while p < end {
            let Some(frame) = frames::alloc() else {
                rollback(&space, align_up(old), p);
                klog!("sbrk: out of frames at {:#x}", p);
                return -1;
            };
            // zero BEFORE publishing the mapping: ring 3 must never see
            // what the previous owner of this frame left behind
            unsafe {
                core::ptr::write_bytes(phys_to_virt(frame) as *mut u8, 0, PAGE as usize);
            }
            let flags = PRESENT | WRITABLE | USER | NO_EXECUTE;
            if let Err(e) = space.map(p, frame, flags) {
                frames::free(frame);
                rollback(&space, align_up(old), p);
                klog!("sbrk: map {:#x} failed: {}", p, e);
                return -1;
            }
            p += PAGE;
        }
    } else if new_break < old {
        // --- shrink: free every page entirely above the new break -----
        let mut p = align_up(new_break);
        let end = align_up(old);
        while p < end {
            if let Some(phys) = space.unmap(p) {
                // release (not free): the page may be COW-shared with a
                // fork child; release handles both refcounts correctly
                frames::release(phys);
            }
            p += PAGE;
        }
    }

    sched::set_current_heap_break(new_break);
    old as i64
}

/// Undo a partially applied grow: unmap+release [from, until).
fn rollback(space: &AddressSpace, from: u64, until: u64) {
    let mut p = from;
    while p < until {
        if let Some(phys) = space.unmap(p) {
            frames::release(phys);
        }
        p += PAGE;
    }
}
