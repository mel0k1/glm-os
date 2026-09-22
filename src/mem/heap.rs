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
    magic: u64,         // "GLMHEAD" — overwritten means someone wrote
                        // past a neighbouring payload (heap guard)
}

const HDR_MAGIC: u64 = 0x474C4D48454144; // "GLMHEAD"

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

unsafe fn ok_node(st: &HeapState, p: *mut FreeNode) -> bool {
    // NOTE: tails of split blocks can be UNALIGNED (payload sizes like 3
    // bytes make the next FreeNode land at base+27). Unaligned access is
    // legal on x86_64, so only canonicality + bounds are checked here.
    let a = p as usize;
    let base = st.base as usize;
    a >= base && a < base + st.size
}

unsafe fn ok_payload(st: &HeapState, p: *mut u8) -> bool {
    let a = p as usize;
    let base = st.base as usize;
    a >= base + core::mem::size_of::<Header>() && a < base + st.size
}

/// Lock-free diagnostics formatter: hex into stack arrays, straight to
/// COM1 without locks — works even when LINE_LOCK is held by whoever
/// triggered the corruption report. No allocation anywhere.
fn hex(v: u64) -> [u8; 18] {
    let mut b = [b'0'; 18];
    b[0] = b'0';
    b[1] = b'x';
    const HEXD: &[u8; 16] = b"0123456789abcdef";
    for i in 0..16 {
        b[2 + i] = HEXD[((v >> (60 - 4 * i)) & 0xF) as usize];
    }
    b
}

/// Hexdump 32 bytes of raw memory (diagnostics only).
unsafe fn diag_mem(prefix: &str, a: usize) {
    crate::io::serial::diag_str(prefix);
    for i in 0..32 {
        let b = core::ptr::read_volatile((a + i) as *const u8);
        let h = hex((b as u64) & 0xFF);
        // last two hex chars of "0x00000000000000XX"
        crate::io::serial::diag_str(core::str::from_utf8_unchecked(&h[16..18]));
        crate::io::serial::diag_str(" ");
    }
    crate::io::serial::diag_str("\n");
}

fn diag_hex(prefix: &str, v: u64) {
    unsafe {
        crate::io::serial::diag_str(prefix);
        let h = hex(v);
        crate::io::serial::diag_str(core::str::from_utf8_unchecked(&h));
        crate::io::serial::diag_str(" ");
    }
}

unsafe fn heap_corrupt(site: &str, st: &HeapState) -> ! {
    crate::io::serial::diag_str("\n[glm] heap: CORRUPT at ");
    crate::io::serial::diag_str(site);
    crate::io::serial::diag_str("\n[glm] ");
    diag_hex("head=", st.head as usize as u64);
    diag_hex("base=", st.base as usize as u64);
    diag_hex("size=", st.size as u64);
    diag_hex("allocs=", st.allocs as u64);
    diag_hex("frees=", st.frees as u64);
    crate::io::serial::diag_str("\n");
    panic!("heap corruption at {}", site);
}

unsafe fn alloc_inner(st: &mut HeapState, size: usize, align: usize) -> *mut u8 {
    if size == 0 || size > (1 << 30) {
        return core::ptr::null_mut();
    }
    // v1.4 FIX: the worst-case alignment shift must be reserved for EVERY
    // alignment, not just align > 16. payload = align_up(block+24, align)
    // can sit up to align-1 bytes past block+24, so with the old
    // "align > 16" rule a size+24 block let payload_end run up to 7 bytes
    // past the block end, corrupting the NEXT free-list node. This was
    // latent since v0.3 and finally fired when terminal strings (align 1)
    // seeded unaligned blocks.
    let need = size + core::mem::size_of::<Header>() + align - 1;

    let mut prev: *mut FreeNode = core::ptr::null_mut();
    let mut cur = st.head;

    while !cur.is_null() {
        if !ok_node(st, cur) {
            diag_hex("node=", cur as usize as u64);
            diag_hex("prev=", prev as usize as u64);
            diag_hex("need=", need as u64);
            if !prev.is_null() {
                unsafe { diag_mem("prevmem=", prev as usize) };
            }
            unsafe { diag_mem("nodemem=", cur as usize) };
            heap_corrupt("alloc-walk", st);
        }
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
                magic: HDR_MAGIC,
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

/// Full invariant walk: strictly address-ordered, every node in bounds,
/// no overlaps. Called on every free — the free list is tiny early on,
/// and the first violation pinpoints the operation after which the list
/// started rotting.
unsafe fn validate_walk(st: &HeapState) {
    let mut prev: *mut FreeNode = core::ptr::null_mut();
    let mut cur = st.head;
    let mut n = 0usize;
    while !cur.is_null() {
        if !ok_node(st, cur) {
            diag_hex("node#", n as u64);
            diag_hex("addr=", cur as usize as u64);
            diag_hex("prev=", prev as usize as u64);
            heap_corrupt("validate-node", st);
        }
        let node = &mut *cur;
        let size = node.size;
        let next = node.next;
        if size < core::mem::size_of::<FreeNode>() || (cur as usize) + size > (st.base as usize) + st.size {
            diag_hex("node#", n as u64);
            diag_hex("addr=", cur as usize as u64);
            diag_hex("size=", size as u64);
            heap_corrupt("validate-size", st);
        }
        if !prev.is_null() {
            let p = &*prev;
            if (prev as usize) + p.size > cur as usize {
                diag_hex("prev#", (n - 1) as u64);
                diag_hex("prevaddr=", prev as usize as u64);
                diag_hex("next=", cur as usize as u64);
                heap_corrupt("validate-overlap", st);
            }
        }
        prev = cur;
        cur = next;
        n += 1;
        if n > 4096 {
            heap_corrupt("validate-loop", st);
        }
    }
}

unsafe fn free_inner(st: &mut HeapState, payload: *mut u8) {
    if payload.is_null() {
        return;
    }
    if !ok_payload(st, payload) {
        diag_hex("payload=", payload as usize as u64);
        heap_corrupt("free-bounds", st);
    }
    let hdr = (payload as usize - core::mem::size_of::<Header>()) as *mut Header;
    let block = (*hdr).block as *mut FreeNode;
    let block_size = (*hdr).block_size;
    let payload_size = (*hdr).payload_size;
    let magic = (*hdr).magic;
    if !ok_node(st, block)
        || (block as usize) + block_size > (st.base as usize) + st.size
        || block_size < core::mem::size_of::<Header>()
        || magic != HDR_MAGIC
    {
        diag_hex("block=", block as usize as u64);
        diag_hex("bsize=", block_size as u64);
        diag_hex("hdr=", hdr as usize as u64);
        diag_hex("magic=", magic);
        diag_hex("psize=", payload_size as u64);
        heap_corrupt("free-header", st);
    }
    validate_walk(st);

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
    validate_walk(st);
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
