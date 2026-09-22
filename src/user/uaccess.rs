//! User-pointer access helpers (v0.5).
//!
//! Kernel code must never dereference a user virtual address directly:
//! the CR3 currently loaded may belong to a different task (delivery runs
//! in the context of whoever is switching in). Every access therefore goes
//! through `AddressSpace::translate` + the HHDM mapping, page by page, and
//! a missing mapping is an `Err`, never a fault.

use crate::mem::paging::phys_to_virt;
use crate::mem::vmm::AddressSpace;

/// Copy `dst.len()` bytes from user space starting at `uva`.
pub fn read_user_bytes(space: &AddressSpace, uva: u64, dst: &mut [u8]) -> Result<(), ()> {
    let mut va = uva;
    for b in dst.iter_mut() {
        let phys = space.translate(va).ok_or(())?;
        // SAFETY: phys comes from a present PTE of the target address space;
        // the HHDM view is always mapped in the kernel half.
        *b = unsafe { (phys_to_virt(phys) as *const u8).read_volatile() };
        va += 1;
    }
    Ok(())
}

/// Copy `src.len()` bytes into user space starting at `uva`.
pub fn write_user_bytes(space: &AddressSpace, uva: u64, src: &[u8]) -> Result<(), ()> {
    let mut va = uva;
    for &b in src {
        let phys = space.translate(va).ok_or(())?;
        // SAFETY: as above; the target page belongs to the task's mapping.
        unsafe { (phys_to_virt(phys) as *mut u8).write_volatile(b) };
        va += 1;
    }
    Ok(())
}
