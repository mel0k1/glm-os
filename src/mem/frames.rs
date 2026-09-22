//! Physical frame allocator: bitmap over the Limine memory map.
//! Bit = 1 -> reserved, bit = 0 -> free. O(1)-ish alloc via rotating hint.
//!
//! v0.4 SMP: single-frame `alloc()` is lock-free (CAS on the bitmap word),
//! but the contiguous scan in `alloc_contig()` is check-then-mark, so it
//! runs under a spinlock. `free()` is a single atomic AND — safe as is.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::Spinlock;
use limine::memory_map;

const FRAME_SIZE: usize = 4096;
// enough bitmap for 1 GiB of physical memory
const MAX_FRAMES: usize = 1 << 18;
const WORDS: usize = MAX_FRAMES / 64;

static BITMAP: [core::sync::atomic::AtomicU64; WORDS] =
    [const { core::sync::atomic::AtomicU64::new(!0u64) }; WORDS];

/// Guards the non-atomic check-then-mark scan in alloc_contig().
static CONTIG_LOCK: Spinlock<()> = Spinlock::new(());

static TOTAL: AtomicUsize = AtomicUsize::new(0);
static USED: AtomicUsize = AtomicUsize::new(0);
static REGIONS: AtomicUsize = AtomicUsize::new(0);
static HINT: AtomicUsize = AtomicUsize::new(0);

fn mark_free(idx: usize) {
    BITMAP[idx / 64].fetch_and(!(1u64 << (idx % 64)), Ordering::Relaxed);
}

fn mark_used(idx: usize) {
    BITMAP[idx / 64].fetch_or(1u64 << (idx % 64), Ordering::Relaxed);
}

fn is_free(idx: usize) -> bool {
    BITMAP[idx / 64].load(Ordering::Relaxed) & (1u64 << (idx % 64)) == 0
}

pub fn init(entries: &[&memory_map::Entry]) {
    let mut total = 0usize;
    let mut regions = 0usize;
    for e in entries {
        if e.entry_type == memory_map::EntryType::USABLE {
            regions += 1;
            let start = ((e.base + FRAME_SIZE as u64 - 1) / FRAME_SIZE as u64) as usize; // round up, skips frame 0
            let end = ((e.base + e.length) / FRAME_SIZE as u64) as usize;
            for idx in start..end.min(MAX_FRAMES) {
                mark_free(idx);
                total += 1;
            }
        }
    }
    TOTAL.store(total, Ordering::Relaxed);
    USED.store(0, Ordering::Relaxed);
    REGIONS.store(regions, Ordering::Relaxed);
}

/// Allocate one physical frame, returns physical address.
pub fn alloc() -> Option<u64> {
    let words = WORDS;
    let start = HINT.load(Ordering::Relaxed);
    for off in 0..words {
        let w = (start + off) % words;
        let mut cur = BITMAP[w].load(Ordering::Relaxed);
        loop {
            let free_bits = !cur;
            if free_bits == 0 {
                break;
            }
            let bit = free_bits.trailing_zeros() as usize;
            let mask = 1u64 << bit;
            match BITMAP[w].compare_exchange_weak(cur, cur | mask, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    HINT.store(w, Ordering::Relaxed);
                    USED.fetch_add(1, Ordering::Relaxed);
                    return Some(((w * 64 + bit) * FRAME_SIZE) as u64);
                }
                Err(actual) => cur = actual,
            }
        }
    }
    None
}

/// Allocate `n` physically contiguous frames (kernel stacks, heap).
/// SMP-safe: the whole scan-and-mark runs under CONTIG_LOCK.
pub fn alloc_contig(n: usize) -> Option<u64> {
    let _g = CONTIG_LOCK.lock();
    let mut run_start: Option<usize> = None;
    let mut run_len = 0usize;
    for idx in 0..MAX_FRAMES {
        if is_free(idx) {
            if run_start.is_none() {
                run_start = Some(idx);
            }
            run_len += 1;
            if run_len == n {
                let base = run_start.unwrap();
                for i in base..base + n {
                    mark_used(i);
                }
                USED.fetch_add(n, Ordering::Relaxed);
                return Some((base * FRAME_SIZE) as u64);
            }
        } else {
            run_start = None;
            run_len = 0;
        }
    }
    None
}

pub fn free(phys: u64) {
    let idx = (phys as usize) / FRAME_SIZE;
    if idx < MAX_FRAMES {
        mark_free(idx);
        USED.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Stats {
    pub total: usize,
    pub used: usize,
    pub regions: usize,
}

pub fn stats() -> Stats {
    Stats {
        total: TOTAL.load(Ordering::Relaxed),
        used: USED.load(Ordering::Relaxed),
        regions: REGIONS.load(Ordering::Relaxed),
    }
}

pub const FRAME: usize = FRAME_SIZE;
