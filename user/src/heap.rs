//! v2.1: malloc/free/realloc/calloc for ring 3 — built on SYS_SBRK.
//!
//! The kernel maps a per-process arena at 0x2000_0000 and grows it one
//! zeroed page per sbrk() call; this module turns that flat region into
//! a real allocator. Design: the classic first-fit free list with
//! address-ordered insertion and boundary coalescing (K&R §8.7, adapted
//! to 64-bit and checked arithmetic):
//!
//!   block:  [Hdr: size (u64, whole block incl. header) | next (u64)]
//!           [payload ...]
//!
//!   * every block size is a multiple of 16, so the payload of a block
//!     at an address >= USER_HEAP_BASE is itself 16-aligned — safe for
//!     any type the userland has;
//!   * the free list is kept sorted by ADDRESS: free() merges the block
//!     with its lower and upper physical neighbours, so a program that
//!     allocates and frees in any pattern eventually coalesces back
//!     into large blocks instead of fragmenting into slivers;
//!   * when no free block fits, the allocator asks the kernel for
//!     morecore: a page-rounded chunk of at least 4 KiB, whose header
//!     is carved out of the chunk itself and the rest enters the list
//!     through free() (which also merges it with the top block if they
//!     are adjacent — growth stays coalesced);
//!   * freed memory stays mapped (the break never shrinks behind
//!     malloc's back): sbrk-mapped pages are only given back on process
//!     exit. Honest and simple — a compacting collector this is not.
//!
//! Not thread-safe: the free-list head is a plain static. Threads share
//! the address space, so a multi-threaded program must confine malloc
//! to one thread (the whole userland does; documented blind spot).
//!
//! free() validates its argument (alignment, arena range, size sanity)
//! and silently ignores bogus pointers rather than corrupting the list.

use crate::syscall1;
use crate::SYS_SBRK;

pub const ARENA_BASE: u64 = 0x0000_2000_0000;
pub const ARENA_MAX: u64 = 0x0000_3000_0000;

/// Block header: 16 bytes. `size` covers the WHOLE block (header
/// included); bit 0 is reserved as 0 — sizes are multiples of 16.
/// `next` is the next free block by address (meaningful only while the
/// block is in the free list; allocated blocks leave it untouched).
#[repr(C)]
struct Hdr {
    size: u64,
    next: u64,
}

const HDR_SIZE: u64 = core::mem::size_of::<Hdr>() as u64;
const MIN_BLOCK: u64 = 32; // header + 16 bytes payload
const ALIGN: u64 = 16;
const MAX_ALLOC: u64 = 64 * 1024 * 1024; // sanity cap per allocation

/// Sentinel: a zero-size header whose `next` points at the first real
/// free block (or itself when the list is empty). Lives in BSS, one per
/// process — every process owns a private arena and a private list.
static mut BASE: Hdr = Hdr { size: 0, next: 0 };
static mut INITED: bool = false;

#[inline]
fn align_up16(v: u64) -> u64 {
    (v + ALIGN - 1) & !(ALIGN - 1)
}

/// The sbrk gate: returns the old break as u64, or None on refusal.
fn more_sbrk(inc: u64) -> Option<u64> {
    let r = syscall1(SYS_SBRK, inc) as i64;
    if r < 0 {
        None
    } else {
        Some(r as u64)
    }
}

/// Ensure `need` bytes (whole block, header included) are available in
/// the free list: fetch a fresh chunk from the kernel and insert it.
/// Returns false when the kernel refuses (out of arena).
fn morecore(need: u64) -> bool {
    // chunk: page-rounded, at least 4 KiB, at least the request
    let chunk = {
        let min = need.max(4096);
        (min + 4095) & !4095
    };
    let Some(old) = more_sbrk(chunk) else {
        return false;
    };
    // the chunk starts at the old break; carve the header from its head
    // and hand the rest (as a free block) to the list via free-list
    // insertion. Region: [old, old + chunk); block header at old.
    unsafe {
        let hp = old as *mut Hdr;
        (*hp).size = chunk;
        (*hp).next = 0;
        insert_free(old);
    }
    true
}

/// Insert a block ([hdr at `block`, size in hdr.size) into the
/// address-ordered free list, coalescing with both neighbours.
unsafe fn insert_free(block: u64) {
    let base = &raw mut BASE as u64;
    // walk until we find the first free block ABOVE ours; `prev` trails
    let mut prev = base;
    loop {
        let cur = (*(prev as *mut Hdr)).next;
        if cur == base || cur > block {
            break;
        }
        prev = cur;
    }
    let next = (*(prev as *mut Hdr)).next;

    // merge with the upper neighbour first: block + size == next
    let bsize = (*(block as *mut Hdr)).size;
    let upper = block + bsize;
    let mut size = bsize;
    if next == upper {
        // swallow the upper block: our span now reaches its span
        size += (*(next as *mut Hdr)).size;
        (*(block as *mut Hdr)).size = size;
        (*(block as *mut Hdr)).next = (*(next as *mut Hdr)).next;
    } else {
        (*(block as *mut Hdr)).next = next;
        (*(block as *mut Hdr)).size = size;
    }

    // merge with the lower neighbour: prev + prev.size == block
    if prev + (*(prev as *mut Hdr)).size == block {
        (*(prev as *mut Hdr)).size += size;
        (*(prev as *mut Hdr)).next = (*(block as *mut Hdr)).next;
    } else {
        (*(prev as *mut Hdr)).next = block;
    }
}

/// Validate a malloc'd payload pointer for free(): 16-aligned, header
/// inside the arena, size sane.
unsafe fn valid_block(p: u64) -> bool {
    if p == 0 || p & (ALIGN - 1) != 0 {
        return false;
    }
    let block = p - HDR_SIZE;
    if block < ARENA_BASE || block + HDR_SIZE > ARENA_MAX {
        return false;
    }
    let size = (*(block as *mut Hdr)).size;
    size >= MIN_BLOCK && size & (ALIGN - 1) == 0 && block + size <= ARENA_MAX
}

/// malloc(n): a pointer to at least `n` writable bytes, 16-aligned, or
/// null when the arena is exhausted / the request is bogus.
pub fn malloc(n: u64) -> *mut u8 {
    if n == 0 {
        return malloc(1);
    }
    if n > MAX_ALLOC {
        return core::ptr::null_mut();
    }
    // whole block = header + payload, rounded to 16; watch the add
    let Some(need) = n.checked_add(HDR_SIZE).map(align_up16) else {
        return core::ptr::null_mut();
    };
    if need < MIN_BLOCK {
        return core::ptr::null_mut();
    }

    unsafe {
        if !INITED {
            // the sentinel initially points at itself: empty list
            (*core::ptr::addr_of_mut!(BASE)).next = core::ptr::addr_of_mut!(BASE) as u64;
            INITED = true;
        }
        let base = &raw mut BASE as u64;

        // first fit over the address-ordered list
        let mut prev = base;
        loop {
            let cur = (*(prev as *mut Hdr)).next;
            if cur == base {
                break; // exhausted
            }
            let csize = (*(cur as *mut Hdr)).size;
            if csize >= need {
                let rest = csize - need;
                if rest >= MIN_BLOCK {
                    // split: the tail stays free, the head is handed out
                    let tail = cur + need;
                    // CRITICAL: rewrite the head's size to the REQUESTED
                    // span — the header still carries the old (bigger)
                    // free-block size, and free()/realloc() would later
                    // insert/over-read a span that overlaps the tail
                    (*(cur as *mut Hdr)).size = need;
                    (*(tail as *mut Hdr)).size = rest;
                    (*(tail as *mut Hdr)).next = (*(cur as *mut Hdr)).next;
                    (*(prev as *mut Hdr)).next = tail;
                } else {
                    // take the whole block (up to 15 bytes of slack)
                    (*(prev as *mut Hdr)).next = (*(cur as *mut Hdr)).next;
                }
                let payload = cur + HDR_SIZE;
                return payload as *mut u8;
            }
            prev = cur;
        }

        // nothing fits: morecore once, then scan again (the fresh chunk
        // may have coalesced with a trailing free block)
        if morecore(need) {
            let mut prev = base;
            loop {
                let cur = (*(prev as *mut Hdr)).next;
                if cur == base {
                    break;
                }
                let csize = (*(cur as *mut Hdr)).size;
                if csize >= need {
                    let rest = csize - need;
                    if rest >= MIN_BLOCK {
                        let tail = cur + need;
                        (*(cur as *mut Hdr)).size = need; // see the note in the first scan
                        (*(tail as *mut Hdr)).size = rest;
                        (*(tail as *mut Hdr)).next = (*(cur as *mut Hdr)).next;
                        (*(prev as *mut Hdr)).next = tail;
                    } else {
                        (*(prev as *mut Hdr)).next = (*(cur as *mut Hdr)).next;
                    }
                    let payload = cur + HDR_SIZE;
                    return payload as *mut u8;
                }
                prev = cur;
            }
        }
    }
    core::ptr::null_mut()
}

/// free(p): return a block to the list (coalescing with neighbours).
/// Bogus pointers are ignored (validated, not trusted).
///
/// # Safety
/// `p` must be a pointer obtained from malloc/realloc that has not been
/// freed already, and no thread may be inside malloc/free concurrently.
pub unsafe fn free(p: *mut u8) {
    let a = p as u64;
    if !valid_block(a) {
        return;
    }
    let block = a - HDR_SIZE;
    let size = (*(block as *mut Hdr)).size;
    if !INITED {
        return; // nothing was ever allocated
    }
    // mark the span as one free block and insert with coalescing
    (*(block as *mut Hdr)).size = size;
    insert_free(block);
}

/// calloc(n, sz): n * zeroed bytes; null on overflow/exhaustion.
pub fn calloc(n: u64, sz: u64) -> *mut u8 {
    let Some(total) = n.checked_mul(sz) else {
        return core::ptr::null_mut();
    };
    let p = malloc(total);
    if !p.is_null() {
        unsafe {
            core::ptr::write_bytes(p, 0, total as usize);
        }
    }
    p
}

/// realloc(p, n): grow/shrink by allocation + copy + free. NULL behaves
/// like malloc(n); n == 0 behaves like free(p) and returns NULL.
///
/// # Safety
/// `p` must be a live malloc'd pointer or NULL; see free().
pub unsafe fn realloc(p: *mut u8, n: u64) -> *mut u8 {
    if p.is_null() {
        return malloc(n);
    }
    if n == 0 {
        free(p);
        return core::ptr::null_mut();
    }
    if !valid_block(p as u64) {
        return core::ptr::null_mut();
    }
    let old_hdr = (p as u64 - HDR_SIZE) as *const Hdr;
    let old_size = unsafe { (*old_hdr).size - HDR_SIZE };
    let q = malloc(n);
    if q.is_null() {
        return core::ptr::null_mut();
    }
    let keep = if old_size < n { old_size } else { n } as usize;
    core::ptr::copy_nonoverlapping(p as *const u8, q, keep);
    free(p);
    q
}

/// The raw gate, exposed for diagnostics: sbrk(0) reports the break.
pub fn sbrk(inc: i64) -> Option<u64> {
    let r = syscall1(SYS_SBRK, inc as u64) as i64;
    if r < 0 {
        None
    } else {
        Some(r as u64)
    }
}
