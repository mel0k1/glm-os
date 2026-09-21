//! Paging introspection: read CR3, walk Limine's page tables via the HHDM.
//! GLM OS v0.1 runs on the bootloader's page tables; own tables are v0.2.

use core::arch::asm;

static HHDM: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn init(hhdm_offset: u64) {
    HHDM.store(hhdm_offset, core::sync::atomic::Ordering::Relaxed);
}

pub fn hhdm_offset() -> u64 {
    HHDM.load(core::sync::atomic::Ordering::Relaxed)
}

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + hhdm_offset()
}

pub fn cr3() -> u64 {
    let v: u64;
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[inline]
fn read_u64(vaddr: u64) -> u64 {
    unsafe { core::ptr::read_volatile(vaddr as *const u64) }
}

/// Decode a page-table entry's flags into a short string (ASCII, no alloc).
pub fn flags_str(e: u64) -> ([u8; 5], usize) {
    let mut buf = [b'-'; 5];
    let mut n = 0;
    if e & 1 != 0 {
        buf[n] = b'P';
        n += 1;
    }
    if e & 2 != 0 {
        buf[n] = b'W';
        n += 1;
    }
    if e & 4 != 0 {
        buf[n] = b'U';
        n += 1;
    }
    if e & (1 << 63) != 0 {
        buf[n] = b'N';
        n += 1;
    }
    (buf, n)
}

/// Walk the PML4 and report the interesting mappings.
pub fn describe() -> (u64, usize) {
    let pml4_phys = cr3() & 0x000F_FFFF_FFFF_F000;
    let pml4 = phys_to_virt(pml4_phys);
    let mut mapped = 0;
    for i in 0..512 {
        let e = read_u64(pml4 + (i as u64) * 8);
        if e & 1 != 0 {
            mapped += 1;
        }
    }
    (pml4, mapped)
}

/// Print the first `limit` non-zero PML4 entries to the console.
pub fn dump(limit: usize) {
    let pml4_phys = cr3() & 0x000F_FFFF_FFFF_F000;
    let pml4 = phys_to_virt(pml4_phys);
    let mut shown = 0;
    crate::console::print("      pml4 entry  address            flags\n");
    for i in 0..512usize {
        let e = read_u64(pml4 + (i as u64) * 8);
        if e & 1 == 0 {
            continue;
        }
        let (fbuf, fn_) = flags_str(e);
        let flags = core::str::from_utf8(&fbuf[..fn_]).unwrap_or("");
        crate::console::print_args(format_args!(
            "      [{:03}]       {:#018x}  {}\n",
            i,
            e & 0x000F_FFFF_FFFF_F000,
            flags
        ));
        shown += 1;
        if shown >= limit {
            break;
        }
    }
}
