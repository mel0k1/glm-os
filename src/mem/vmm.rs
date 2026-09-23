//! GLM OS v0.2 — own virtual memory manager.
//!
//! Builds and walks 4-level page tables (PML4 -> PML3 -> PML2 -> PML1)
//! through the Limine higher-half direct map. New address spaces share the
//! kernel's upper-half PML4 entries (everything >= 0xFFFF_8000_0000_0000),
//! so the kernel, the heap and the HHDM survive a CR3 switch untouched.
//!
//! v0.6 adds copy-on-write fork: `AddressSpace::fork_cow()` clones the
//! lower half by sharing every frame — writable pages are downgraded to
//! read-only in BOTH spaces and tagged with the software PTE_COW bit, so
//! the first write takes a page fault the kernel resolves by giving the
//! writer a private copy (`cow_resolve`). Shared frames carry reference
//! counts in the frame allocator, so `destroy()` never frees a frame that
//! somebody else still maps.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

use super::frames;
use super::paging::phys_to_virt;
use crate::klog;

// --- page-table entry flags -------------------------------------------------

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const NO_CACHE: u64 = 1 << 4;
pub const HUGE: u64 = 1 << 7; // PS bit (PML3: 1 GiB, PML2: 2 MiB)
pub const NO_EXECUTE: u64 = 1 << 63;

/// v0.6: software flag (bit 9, "available to software" in long-mode PTEs):
/// the page is shared copy-on-write. A COW page is present but read-only;
/// the first write faults and the kernel hands out a private copy.
pub const PTE_COW: u64 = 1 << 9;

const PTE_FRAME_MASK: u64 = 0x000F_FFFF_FFFF_F000;

pub const PAGE: u64 = 4096;

// --- COW bookkeeping ----------------------------------------------------------

/// fork_cow() calls completed so far.
static COW_FORKS: AtomicU64 = AtomicU64::new(0);
/// Write faults resolved by cow_resolve() so far.
static COW_FAULTS: AtomicU64 = AtomicU64::new(0);
/// Pages downgraded to COW by the most recent fork_cow() (stats snapshot).
static COW_LAST_MARKED: AtomicU64 = AtomicU64::new(0);

pub fn cow_stats() -> (u64, u64, u64) {
    (
        COW_FORKS.load(Ordering::Relaxed),
        COW_LAST_MARKED.load(Ordering::Relaxed),
        COW_FAULTS.load(Ordering::Relaxed),
    )
}

// address space layout constants
pub const KERNEL_HALF_BASE: u64 = 0xFFFF_8000_0000_0000; // PML4 index 256..
pub const USER_IMG_BASE: u64 = 0x0040_0000;
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const USER_STACK_PAGES: u64 = 16;

#[inline]
pub fn align_down(v: u64) -> u64 {
    v & !(PAGE - 1)
}

#[inline]
pub fn align_up(v: u64) -> u64 {
    (v + PAGE - 1) & !(PAGE - 1)
}

// --- CR3 ---------------------------------------------------------------------

/// CR3 the bootloader gave us: the kernel address space. Saved once at boot.
static KERNEL_CR3: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub fn init() {
    KERNEL_CR3.store(cr3() & 0x000F_FFFF_FFFF_F000, core::sync::atomic::Ordering::Relaxed);
}

pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Switch to a page table (physical PML4 address, low flags masked off).
pub fn load_cr3(pml4_phys: u64) {
    unsafe { asm!("mov cr3, {}", in(reg) pml4_phys & 0x000F_FFFF_FFFF_F000, options(nostack)) };
}

#[inline]
pub fn invlpg(vaddr: u64) {
    unsafe { asm!("invlpg [{}]", in(reg) vaddr, options(nostack, nomem)) };
}

// --- raw table access ---------------------------------------------------------

#[inline]
fn table_of(entry: u64) -> u64 {
    phys_to_virt(entry & 0x000F_FFFF_FFFF_F000)
}

#[inline]
fn read_entry(table_virt: u64, idx: usize) -> u64 {
    unsafe { core::ptr::read_volatile((table_virt + (idx as u64) * 8) as *const u64) }
}

#[inline]
fn write_entry(table_virt: u64, idx: usize, val: u64) {
    unsafe { core::ptr::write_volatile((table_virt + (idx as u64) * 8) as *mut u64, val) };
}

/// Allocate a zeroed page-table frame.
fn new_table() -> Option<u64> {
    let frame = frames::alloc()?;
    let virt = phys_to_virt(frame);
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, PAGE as usize) };
    Some(frame)
}

/// Fixed higher-half window for the Local APIC MMIO page (0xFEE00000).
/// PML4[511] / PDPT[511] / PD[502] / PT[...] — supervisor-only, cache-off.
pub const LAPIC_VIRT: u64 = 0xFFFF_FFFF_FEE0_0000;

/// Map one 4 KiB page into the KERNEL half (shared by every address space,
/// because user PML4s clone the kernel's upper-half entries). Supervisor
/// flags only — this is how MMIO like the LAPIC becomes reachable.
pub fn map_kernel_page(virt: u64, phys: u64, flags: u64) -> Result<(), &'static str> {
    if virt & (PAGE - 1) != 0 || phys & (PAGE - 1) != 0 {
        return Err("map_kernel: addresses must be page-aligned");
    }
    if virt < KERNEL_HALF_BASE {
        return Err("map_kernel: refusing to touch the user half");
    }
    let pml4 = phys_to_virt(kernel_cr3());
    let i4 = ((virt >> 39) & 0x1FF) as usize;
    let i3 = ((virt >> 30) & 0x1FF) as usize;
    let i2 = ((virt >> 21) & 0x1FF) as usize;
    let i1 = ((virt >> 12) & 0x1FF) as usize;

    let dir_flags = PRESENT | WRITABLE | NO_EXECUTE;

    let mut e = read_entry(pml4, i4);
    if e & PRESENT == 0 {
        let t = new_table().ok_or("map_kernel: out of frames (pml3)")?;
        write_entry(pml4, i4, t | dir_flags);
        e = read_entry(pml4, i4);
    }
    let pml3 = table_of(e);

    let mut e = read_entry(pml3, i3);
    if e & PRESENT == 0 {
        let t = new_table().ok_or("map_kernel: out of frames (pml2)")?;
        write_entry(pml3, i3, t | dir_flags);
        e = read_entry(pml3, i3);
    }
    if e & HUGE != 0 {
        return Err("map_kernel: huge page where a table was expected");
    }
    let pml2 = table_of(e);

    let mut e = read_entry(pml2, i2);
    if e & PRESENT == 0 {
        let t = new_table().ok_or("map_kernel: out of frames (pml1)")?;
        write_entry(pml2, i2, t | dir_flags);
        e = read_entry(pml2, i2);
    }
    if e & HUGE != 0 {
        return Err("map_kernel: huge page where a table was expected");
    }
    let pml1 = table_of(e);

    write_entry(pml1, i1, phys | flags);
    invlpg(virt);
    Ok(())
}

// --- the address space ---------------------------------------------------------

/// An owned PML4. User spaces clone the kernel's upper half; the lower half
/// (everything a user program can see) is exclusively ours.
pub struct AddressSpace {
    pub pml4: u64,
    owns_lower_half: bool,
}

impl AddressSpace {
    /// Wrap an existing PML4 (e.g. whatever CR3 points at right now) without
    /// owning it: translate()/map() work, destroy() must never be called.
    pub fn from_pml4(pml4: u64) -> Self {
        Self {
            pml4: pml4 & 0x000F_FFFF_FFFF_F000,
            owns_lower_half: false,
        }
    }

    /// A fresh user address space: empty lower half, kernel upper half shared.
    pub fn new_user() -> Option<Self> {
        let pml4 = new_table()?;
        let cur = phys_to_virt(kernel_cr3());
        let dst = phys_to_virt(pml4);
        for i in 256..512usize {
            // share (not copy) kernel tables: kernel+heap+hhdm stay valid
            write_entry(dst, i, read_entry(cur, i));
        }
        Some(Self {
            pml4,
            owns_lower_half: true,
        })
    }

    /// v0.6: copy-on-write clone of this address space (fork).
    ///
    /// Every present user leaf page is shared with the child:
    ///   * writable pages are downgraded to read-only + PTE_COW in BOTH
    ///     spaces (the writer gets a private copy on the first fault);
    ///   * read-only pages (text, sigreturn trampoline) are shared as-is.
    /// Every shared frame gets +1 in the refcount table, so each space's
    /// destroy() releases its own share without double-freeing.
    ///
    /// Returns the child. The parent's modified PTEs are invalidated locally
    /// (this CPU runs the parent during a fork syscall); the caller should
    /// fire a TLB shootdown so no other CPU keeps stale writable entries.
    pub fn fork_cow(&self) -> Option<AddressSpace> {
        let pml4 = new_table()?;
        let cur = phys_to_virt(kernel_cr3());
        let dst = phys_to_virt(pml4);
        for i in 256..512usize {
            write_entry(dst, i, read_entry(cur, i));
        }

        let src = phys_to_virt(self.pml4);
        let dir_flags = PRESENT | WRITABLE | USER;
        let mut marked = 0u64;
        let mut shared = 0u64;

        for i4 in 0..256usize {
            let e4 = read_entry(src, i4);
            if e4 & PRESENT == 0 {
                continue;
            }
            let c_pml3 = new_table()?;
            write_entry(dst, i4, c_pml3 | dir_flags);
            let c_pml3_v = phys_to_virt(c_pml3);
            let pml3 = table_of(e4);
            for i3 in 0..512usize {
                let e3 = read_entry(pml3, i3);
                if e3 & PRESENT == 0 || e3 & HUGE != 0 {
                    continue;
                }
                let c_pml2 = new_table()?;
                write_entry(c_pml3_v, i3, c_pml2 | dir_flags);
                let c_pml2_v = phys_to_virt(c_pml2);
                let pml2 = table_of(e3);
                for i2 in 0..512usize {
                    let e2 = read_entry(pml2, i2);
                    if e2 & PRESENT == 0 || e2 & HUGE != 0 {
                        continue;
                    }
                    let c_pml1 = new_table()?;
                    write_entry(c_pml2_v, i2, c_pml1 | dir_flags);
                    let c_pml1_v = phys_to_virt(c_pml1);
                    let pml1 = table_of(e2);
                    for i1 in 0..512usize {
                        let e1 = read_entry(pml1, i1);
                        if e1 & PRESENT == 0 {
                            continue;
                        }
                        let phys = e1 & PTE_FRAME_MASK;
                        let flags = e1 & !PTE_FRAME_MASK;
                        let va = ((i4 as u64) << 39)
                            | ((i3 as u64) << 30)
                            | ((i2 as u64) << 21)
                            | ((i1 as u64) << 12);
                        if flags & (USER | WRITABLE) == (USER | WRITABLE) {
                            // writable -> COW: downgrade parent + child
                            let ro = (flags & !WRITABLE) | PTE_COW;
                            write_entry(pml1, i1, phys | ro);
                            write_entry(c_pml1_v, i1, phys | ro);
                            invlpg(va); // parent runs on THIS cpu
                            marked += 1;
                        } else {
                            // read-only (or already COW): share as-is
                            write_entry(c_pml1_v, i1, phys | flags);
                        }
                        frames::add_ref(phys);
                        shared += 1;
                    }
                }
            }
        }

        COW_FORKS.fetch_add(1, Ordering::Relaxed);
        COW_LAST_MARKED.store(marked, Ordering::Relaxed);
        klog!(
            "vmm: fork_cow pml4 {:#x} -> {:#x}: {} pages shared, {} downgraded to cow",
            self.pml4,
            pml4,
            shared,
            marked
        );
        Some(Self {
            pml4,
            owns_lower_half: true,
        })
    }

    /// Map one 4 KiB page. Allocates intermediate tables on demand.
    /// `virt` must be in the lower (user) half.
    pub fn map(&self, virt: u64, phys: u64, flags: u64) -> Result<(), &'static str> {
        if virt & (PAGE - 1) != 0 || phys & (PAGE - 1) != 0 {
            return Err("map: addresses must be page-aligned");
        }
        if virt >= KERNEL_HALF_BASE {
            return Err("map: refusing to touch the kernel half");
        }
        let pml4 = phys_to_virt(self.pml4);
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;

        // intermediate entries need U|W so user code can reach the leaf and
        // the kernel can patch the leaf later through its own view
        let dir_flags = PRESENT | WRITABLE | USER;

        let mut e = read_entry(pml4, i4);
        if e & PRESENT == 0 {
            let t = new_table().ok_or("map: out of frames (pml3)")?;
            write_entry(pml4, i4, t | dir_flags);
            e = read_entry(pml4, i4);
        }
        let pml3 = table_of(e);

        let mut e = read_entry(pml3, i3);
        if e & PRESENT == 0 {
            let t = new_table().ok_or("map: out of frames (pml2)")?;
            write_entry(pml3, i3, t | dir_flags);
            e = read_entry(pml3, i3);
        }
        if e & HUGE != 0 {
            return Err("map: huge page where a table was expected");
        }
        let pml2 = table_of(e);

        let mut e = read_entry(pml2, i2);
        if e & PRESENT == 0 {
            let t = new_table().ok_or("map: out of frames (pml1)")?;
            write_entry(pml2, i2, t | dir_flags);
            e = read_entry(pml2, i2);
        }
        let pml1 = table_of(e);

        let old = read_entry(pml1, i1);
        if old & PRESENT != 0 {
            return Err("map: page already mapped");
        }
        write_entry(pml1, i1, phys | flags);
        Ok(())
    }

    /// Virtual -> physical translation by walking the tables (no CR3 needed:
    /// we read the pages through the HHDM). Returns the physical address.
    pub fn translate(&self, virt: u64) -> Option<u64> {
        let pml4 = phys_to_virt(self.pml4);
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;

        let e = read_entry(pml4, i4);
        if e & PRESENT == 0 {
            return None;
        }
        let pml3 = table_of(e);
        let e = read_entry(pml3, i3);
        if e & PRESENT == 0 {
            return None;
        }
        if e & HUGE != 0 {
            // 1 GiB page
            return Some((e & 0x000F_FFFC_0000_0000) | (virt & 0x3FFF_FFFF));
        }
        let pml2 = table_of(e);
        let e = read_entry(pml2, i2);
        if e & PRESENT == 0 {
            return None;
        }
        if e & HUGE != 0 {
            // 2 MiB page
            return Some((e & 0x000F_FFFF_FE00_0000) | (virt & 0x1F_FFFF));
        }
        let pml1 = table_of(e);
        let e = read_entry(pml1, i1);
        if e & PRESENT == 0 {
            return None;
        }
        Some((e & 0x000F_FFFF_FFFF_F000) | (virt & (PAGE - 1)))
    }

    /// Unmap one 4 KiB page, returning the physical address it pointed at.
    pub fn unmap(&self, virt: u64) -> Option<u64> {
        let pml4 = phys_to_virt(self.pml4);
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;

        let e = read_entry(pml4, i4);
        if e & PRESENT == 0 {
            return None;
        }
        let e = read_entry(table_of(e), i3);
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        let e = read_entry(table_of(e), i2);
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        let pml1 = table_of(e);
        let old = read_entry(pml1, i1);
        if old & PRESENT == 0 {
            return None;
        }
        write_entry(pml1, i1, 0);
        invlpg(virt);
        Some(old & 0x000F_FFFF_FFFF_F000)
    }

    /// Free every user-half table and data frame, then the PML4 itself.
    /// Returns the number of data frames reclaimed. (Our loader only ever
    /// creates 4 KiB pages below the kernel half.)
    pub fn destroy(mut self) -> u64 {
        let freed = self.free_all(false);
        core::mem::forget(self); // consumed; pml4 already zeroed
        freed
    }

    /// v0.7: like destroy(), but LEAKS the root PML4 frame (4 KiB).
    ///
    /// Used on the self-exit path: the dying task's own CR3 may still point
    /// at this PML4 until post_dispatch loads the next task's table. Freeing
    /// the root there would let a concurrent frames::alloc() hand that frame
    /// out as new page-table memory while a live CR3 walks it. A bounded
    /// 4 KiB leak per self-exit buys a race-free teardown.
    pub fn destroy_keep_root(mut self) -> u64 {
        let freed = self.free_all(true);
        core::mem::forget(self); // consumed; pml4 deliberately leaked
        freed
    }

    fn free_all(&mut self, keep_root: bool) -> u64 {
        if !self.owns_lower_half || self.pml4 == 0 {
            return 0;
        }
        let mut freed = 0u64;
        let pml4 = phys_to_virt(self.pml4);
        for i4 in 0..256usize {
            let e4 = read_entry(pml4, i4);
            if e4 & PRESENT == 0 {
                continue;
            }
            let pml3 = table_of(e4);
            for i3 in 0..512usize {
                let e3 = read_entry(pml3, i3);
                if e3 & PRESENT == 0 || e3 & HUGE != 0 {
                    continue;
                }
                let pml2 = table_of(e3);
                for i2 in 0..512usize {
                    let e2 = read_entry(pml2, i2);
                    if e2 & PRESENT == 0 || e2 & HUGE != 0 {
                        continue;
                    }
                    let pml1 = table_of(e2);
                    for i1 in 0..512usize {
                        let e1 = read_entry(pml1, i1);
                        if e1 & PRESENT != 0 {
                            // v0.6: release() handles both private frames
                            // (ref == 0 -> free now) and COW-shared frames
                            // (free only when the last sharer leaves)
                            frames::release(e1 & PTE_FRAME_MASK);
                            freed += 1;
                        }
                    }
                    frames::free(e2 & PTE_FRAME_MASK);
                }
                frames::free(e3 & PTE_FRAME_MASK);
            }
            frames::free(e4 & PTE_FRAME_MASK);
        }
        if keep_root {
            // v0.7: the dying task may still be running on this CR3 — leak
            // the root frame instead of freeing it under its own feet.
            self.pml4 = 0;
            self.owns_lower_half = false;
            return freed;
        }
        frames::free(self.pml4);
        self.pml4 = 0;
        self.owns_lower_half = false;
        freed
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // if we still own a pml4 here, someone dropped without destroy():
        // leak the frames rather than corrupt memory by freeing blindly.
        if self.owns_lower_half && self.pml4 != 0 {
            klog!("vmm: warning: AddressSpace dropped without destroy() (frames leaked)");
        }
    }
}

impl AddressSpace {
    /// Walk to the PML1 (leaf) table of `virt`: returns (pml1_virt, i1).
    /// None = any level missing or a huge page in the path.
    fn walk_leaf(&self, virt: u64) -> Option<(u64, usize)> {
        let pml4 = phys_to_virt(self.pml4);
        let i4 = ((virt >> 39) & 0x1FF) as usize;
        let i3 = ((virt >> 30) & 0x1FF) as usize;
        let i2 = ((virt >> 21) & 0x1FF) as usize;
        let i1 = ((virt >> 12) & 0x1FF) as usize;
        let e = read_entry(pml4, i4);
        if e & PRESENT == 0 {
            return None;
        }
        let e = read_entry(table_of(e), i3);
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        let e = read_entry(table_of(e), i2);
        if e & PRESENT == 0 || e & HUGE != 0 {
            return None;
        }
        Some((table_of(e), i1))
    }
}

// --- COW fault resolution -------------------------------------------------------

/// Resolve a write-protection page fault at `va` caused by COW sharing.
///
/// Called from the interrupt dispatcher for user-mode write faults to a
/// PRESENT page. Reads the CURRENT CR3's tables (the faulting task's).
/// Returns true when the fault was a COW hit and has been handled: the
/// faulting store should simply be retried.
pub fn cow_resolve(va: u64) -> bool {
    let space = AddressSpace::from_pml4(cr3());
    let Some((pml1, i1)) = space.walk_leaf(va) else {
        return false;
    };
    let e = read_entry(pml1, i1);
    if e & PRESENT == 0 || e & PTE_COW == 0 {
        return false; // a genuine fault, not ours
    }
    let phys = e & PTE_FRAME_MASK;
    let flags = e & !PTE_FRAME_MASK;

    if frames::ref_of(phys) == 0 {
        // v1.7 fix: ref == 0 means this task is the frame's ONLY owner
        // (fork_cow leaves a parent+child pair at ref 1). Keep the frame,
        // just restore writability.
        write_entry(pml1, i1, phys | (flags & !PTE_COW) | WRITABLE);
    } else {
        // other address spaces still share this frame: copy it
        let Some(newf) = frames::alloc() else {
            klog!("vmm: cow_resolve: out of frames for {:#x}", va);
            return false;
        };
        unsafe {
            core::ptr::copy_nonoverlapping(
                phys_to_virt(phys) as *const u8,
                phys_to_virt(newf) as *mut u8,
                PAGE as usize,
            );
        }
        write_entry(pml1, i1, newf | (flags & !PTE_COW) | WRITABLE);
        frames::release(phys); // one sharer now has a private copy
    }
    invlpg(va & !(PAGE - 1));
    COW_FAULTS.fetch_add(1, Ordering::Relaxed);
    klog!(
        "vmm: cow fault at {:#x}: frame {:#x} (ref {}) -> private copy",
        va,
        phys,
        frames::ref_of(phys)
    );
    true
}

// --- boot-time self-test --------------------------------------------------------

/// Build a user address space, map a page, verify it through a real CR3
/// switch, then tear everything down. Prints a short report; returns pass/fail.
pub fn self_test() -> bool {
    const OK: &str = "      ";
    let Some(space) = AddressSpace::new_user() else {
        klog!("vmm: self-test: cannot allocate pml4");
        return false;
    };
    let va = USER_IMG_BASE;
    let Some(frame) = frames::alloc() else {
        klog!("vmm: self-test: no frame");
        return false;
    };
    let mapped = space.map(va, frame, PRESENT | WRITABLE | USER).is_ok();
    crate::console::print_args(format_args!(
        "{}map   {:#x} -> frame {:#x} (P|W|U)          {}\n",
        OK,
        va,
        frame,
        if mapped { "ok" } else { "FAIL" }
    ));
    if !mapped {
        return false;
    }

    // write a magic via the HHDM (kernel view of the same physical frame)
    let magic: u64 = 0x47_4C_4D_5F_4F_53_5F_32; // "GLM_OS_2"
    unsafe { core::ptr::write_volatile(phys_to_virt(frame) as *mut u64, magic) };
    crate::console::print_args(format_args!(
        "{}write magic via hhdm                     ok\n",
        OK
    ));

    // translate must agree
    let t_ok = matches!(space.translate(va + 0x123), Some(p) if p & 0xFFF == 0x123 && p & !0xFFF == frame);
    crate::console::print_args(format_args!(
        "{}translate {:#x} -> {:#x}       {}\n",
        OK,
        va + 0x123,
        space.translate(va + 0x123).unwrap_or(0),
        if t_ok { "ok" } else { "FAIL" }
    ));
    if !t_ok {
        klog!("vmm: self-test: translate mismatch (frame {:#x})", frame);
        return false;
    }

    // the real thing: switch CR3, read the user mapping from the CPU itself
    let saved = kernel_cr3();
    load_cr3(space.pml4);
    let seen = unsafe { core::ptr::read_volatile(va as *const u64) };
    load_cr3(saved);
    let cpu_ok = seen == magic;
    crate::console::print_args(format_args!(
        "{}cr3 {:#x} -> {:#x}, cpu read   {}\n",
        OK,
        saved,
        space.pml4,
        if cpu_ok { "ok" } else { "FAIL" }
    ));

    // cleanup: free the test page, then reclaim every table + the pml4
    let test_pml4 = space.pml4;
    let unmapped = space.unmap(va).is_some();
    frames::free(frame);
    crate::console::print_args(format_args!(
        "{}unmap + frame reclaim                    {}\n",
        OK,
        if unmapped { "ok" } else { "FAIL" }
    ));
    let _reclaimed = space.destroy();
    klog!(
        "vmm: self-test {} (cr3 {:#x}->{:#x}, magic {:#x})",
        if cpu_ok && unmapped { "PASS" } else { "FAIL" },
        saved,
        test_pml4,
        magic
    );
    cpu_ok && unmapped
}
