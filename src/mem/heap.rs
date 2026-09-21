//! Kernel heap: address-ordered first-fit free list with coalescing.
//!
//! Hand-rolled (no linked_list_allocator crate). Layout:
//!   Free block  (at block start): { next: *mut FreeNode, size: u64 (whole block) }
//!   Allocated block: header at payload-24: { block: u64, block_size: u64, payload_size: u64 }

use core::sync::atomic::{AtomicUsize, Ordering};

use super::frames;
use crate::sync::Spinlock;

const MIN_BLOCK: usize = 48; // 24 header + 24 min free node headroom
const HEAP_MIB: usize = 64;

struct FreeNode {
    next: *mut FreeNode,
    size: usize, // total block size in bytes (including this header)
}

struct Header {
    block: *mut u8,     // start of the underlying block
    block_size: usize,  // size of the underlying block
    payload_size: usize,
}

struct HeapState {
    head: *mut FreeNode,
    base: *mut u8,
    size: usize,
    allocated: usize,
    allocs: usize,
    frees: usize,
}

unsafe impl Send for HeapState {}

static STATE: Spinlock<Option<HeapState>> = Spinlock::new(None);
static ALLOCS_FAIL: AtomicUsize = AtomicUsize::new(0);

pub fn init() -> bool {
    let n = HEAP_MIB * 1024 * 1024 / frames::FRAME;
    let Some(phys) = frames::alloc_contig(n) else {
        return false;
    };
    // frames -> virtual: Limine keeps an HHDM; we ask limine_reqs for the offset
    let hhdm = crate::limine_reqs::hhdm_offset();
    let base = (phys as u64 + hhdm) as usize as *mut u8;
    let size = n * frames::FRAME;
    unsafe {
        core::ptr::write_bytes(base, 0, 64); // touch first block
        let node = base as *mut FreeNode;
        (*node) = FreeNode {
            next: core::ptr::null_mut(),
            size,
        };
    }
    *STATE.lock() = Some(HeapState {
        head: base as *mut FreeNode,
        base,
        size,
        allocated: 0,
        allocs: 0,
        frees: 0,
    });
    true
}

fn align_up(v: usize, align: usize) -> usize {
    (v + align - 1) & !(align - 1)
}

unsafe fn alloc_inner(st: &mut HeapState, size: usize, align: usize) -> *mut u8 {
    if size == 0 || size > (1 << 30) {
        return core::ptr::null_mut();
    }
    let need = size + core::mem::size_of::<Header>() + if align > 16 { align } else { 0 };

    let mut prev: *mut FreeNode = core::ptr::null_mut();
    let mut cur = st.head;

    while !cur.is_null() {
        let node = &mut *cur;
        if node.size >= need {
            let block = cur as *mut u8;
            let payload = align_up(block as usize + core::mem::size_of::<Header>(), align);
            let payload_end = payload + size;

            let mut block_size = node.size;
            let remainder = (block as usize + node.size).saturating_sub(payload_end);
            let mut new_head = node.next;

            if remainder >= MIN_BLOCK {
                block_size = payload_end - block as usize;
                let tail = payload_end as *mut FreeNode;
                (*tail) = FreeNode {
                    next: node.next,
                    size: remainder,
                };
                new_head = tail;
            } else {
                new_head = node.next;
            }

            // unlink current block
            if prev.is_null() {
                st.head = new_head;
            } else {
                (*prev).next = new_head;
            }

            // write header just below payload
            let hdr = (payload - core::mem::size_of::<Header>()) as *mut Header;
            (*hdr) = Header {
                block,
                block_size,
                payload_size: size,
            };

            st.allocated += size;
            st.allocs += 1;
            return payload as *mut u8;
        }
        prev = cur;
        cur = node.next;
    }
    core::ptr::null_mut()
}

unsafe fn free_inner(st: &mut HeapState, payload: *mut u8) {
    if payload.is_null() {
        return;
    }
    let hdr = (payload as usize - core::mem::size_of::<Header>()) as *mut Header;
    let block = (*hdr).block as *mut FreeNode;
    let block_size = (*hdr).block_size;
    let payload_size = (*hdr).payload_size;

    // insert into address-ordered list, then coalesce both sides
    if st.head.is_null() || (st.head as usize) > block as usize {
        (*block) = FreeNode {
            next: st.head,
            size: block_size,
        };
        st.head = block;
    } else {
        let mut cur = st.head;
        while !(*cur).next.is_null() && ((*cur).next as usize) < block as usize {
            cur = (*cur).next;
        }
        (*block) = FreeNode {
            next: (*cur).next,
            size: block_size,
        };
        (*cur).next = block;
        // coalesce with predecessor
        if (cur as usize) + (*cur).size == block as usize {
            (*cur).size += block_size;
            (*cur).next = (*block).next;
        }
    }
    // coalesce with successor
    let node = block;
    if !(*node).next.is_null()
        && (node as usize + (*node).size) == (*node).next as usize
    {
        (*node).size += (*(*node).next).size;
        (*node).next = (*(*node).next).next;
    }

    st.allocated -= payload_size;
    st.frees += 1;
}

pub struct HeapStats {
    pub size: usize,
    pub allocated: usize,
    pub allocs: usize,
    pub frees: usize,
    pub fails: usize,
}

pub fn stats() -> Option<HeapStats> {
    let st = STATE.lock();
    st.as_ref().map(|s| HeapStats {
        size: s.size,
        allocated: s.allocated,
        allocs: s.allocs,
        frees: s.frees,
        fails: ALLOCS_FAIL.load(Ordering::Relaxed),
    })
}

// ---------------------------------------------------------------------------
// GlobalAlloc glue
// ---------------------------------------------------------------------------

pub struct GlmAlloc;

unsafe impl core::alloc::GlobalAlloc for GlmAlloc {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let mut st = STATE.lock();
        match st.as_mut() {
            Some(s) => alloc_inner(s, layout.size(), layout.align()),
            None => {
                ALLOCS_FAIL.fetch_add(1, Ordering::Relaxed);
                core::ptr::null_mut()
            }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        let mut st = STATE.lock();
        if let Some(s) = st.as_mut() {
            let hdr = (ptr as usize - core::mem::size_of::<Header>()) as *mut Header;
            if (*hdr).payload_size != layout.size() {
                // mismatch: trust the header, but note it
                crate::klog!("heap: size mismatch on free (hdr={}, layout={})", (*hdr).payload_size, layout.size());
            }
            free_inner(s, ptr);
        }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: GlmAlloc = GlmAlloc;
