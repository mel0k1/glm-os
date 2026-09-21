//! GLM OS v0.2 — own virtual memory manager.
//!
//! Builds and walks 4-level page tables (PML4 -> PML3 -> PML2 -> PML1)
//! through the Limine higher-half direct map. New address spaces share the
//! kernel's upper-half PML4 entries (everything >= 0xFFFF_8000_0000_0000),
//! so the kernel, the heap and the HHDM survive a CR3 switch untouched.

use core::arch::asm;

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

pub const PAGE: u64 = 4096;

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
        let freed = self.free_all();
        core::mem::forget(self); // consumed; pml4 already zeroed
        freed
    }

    fn free_all(&mut self) -> u64 {
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
                            frames::free(e1 & 0x000F_FFFF_FFFF_F000);
                            freed += 1;
                        }
                    }
                    frames::free(e2 & 0x000F_FFFF_FFFF_F000);
                }
                frames::free(e3 & 0x000F_FFFF_FFFF_F000);
            }
            frames::free(e4 & 0x000F_FFFF_FFFF_F000);
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
