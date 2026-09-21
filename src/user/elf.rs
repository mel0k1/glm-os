//! ELF64 loader: parse program headers, map PT_LOAD segments into a user
//! address space, hand back the entry point. Supports static ET_EXEC files
//! with 4 KiB-aligned segments (what rust-lld produces for our userland).

use alloc::vec::Vec;

use crate::mem::vmm::{self, AddressSpace, PRESENT, USER, WRITABLE, NO_EXECUTE, PAGE};

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const PT_LOAD: u64 = 1;
const PF_X: u64 = 1;
const PF_W: u64 = 2;

pub struct LoadedImage {
    pub entry: u64,
    pub seg_count: usize,
    pub mapped_bytes: u64,
}

fn u16_at(b: &[u8], off: usize) -> u64 {
    u16::from_le_bytes([b[off], b[off + 1]]) as u64
}
fn u32_at(b: &[u8], off: usize) -> u64 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]) as u64
}
fn u64_at(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// Parse and load an ELF64 executable into `space`.
pub fn load(space: &AddressSpace, image: &[u8]) -> Result<LoadedImage, &'static str> {
    if image.len() < 64 || image[0..4] != ELF_MAGIC {
        return Err("not an ELF file (bad magic)");
    }
    if image[4] != 2 {
        return Err("not a 64-bit ELF (class != ELF64)");
    }
    if image[5] != 1 {
        return Err("not little-endian");
    }
    let e_type = u16_at(image, 16);
    if e_type != 2 {
        return Err("not a static executable (e_type != ET_EXEC)");
    }
    let e_machine = u16_at(image, 18);
    if e_machine != 0x3E {
        return Err("not x86_64 (e_machine != EM_X86_64)");
    }
    let e_entry = u64_at(image, 24);
    let e_phoff = u64_at(image, 32);
    let e_phentsize = u16_at(image, 54) as usize;
    let e_phnum = u16_at(image, 56) as usize;
    if e_phentsize < 56 || e_phnum == 0 {
        return Err("no program headers");
    }
    if (e_phoff as usize) + e_phnum * e_phentsize > image.len() {
        return Err("program header table out of bounds");
    }

    let mut loaded = LoadedImage {
        entry: e_entry,
        seg_count: 0,
        mapped_bytes: 0,
    };

    for i in 0..e_phnum {
        let ph = &image[e_phoff as usize + i * e_phentsize..];
        if u32_at(ph, 0) != PT_LOAD {
            continue;
        }
        let p_offset = u64_at(ph, 8) as usize;
        let p_vaddr = u64_at(ph, 16);
        let p_filesz = u64_at(ph, 32);
        let p_memsz = u64_at(ph, 40);
        let p_flags = u32_at(ph, 4);

        if p_memsz == 0 {
            continue;
        }
        if p_vaddr.checked_add(p_memsz).is_none() || p_vaddr >= vmm::KERNEL_HALF_BASE {
            return Err("segment outside the user half");
        }
        if p_offset.checked_add(p_filesz as usize).map(|e| e > image.len()).unwrap_or(true) {
            return Err("segment data out of bounds");
        }

        // protection flags: R (implicit) | W | X
        let mut flags = PRESENT | USER;
        if p_flags & PF_W != 0 {
            flags |= WRITABLE;
        }
        if p_flags & PF_X == 0 {
            flags |= NO_EXECUTE;
        }

        // map every page of the segment [vaddr, vaddr+memsz), zero-filled
        let va_start = vmm::align_down(p_vaddr);
        let va_end = vmm::align_up(p_vaddr + p_memsz);
        let mut page = va_start;
        while page < va_end {
            if space.translate(page).is_some() {
                return Err("segment overlaps already-mapped memory");
            }
            let frame = crate::mem::frames::alloc().ok_or("out of frames loading ELF")?;
            space
                .map(page, frame, flags)
                .map_err(|e| -> &'static str { e })?;
            // zero-fill via the HHDM view of the frame
            unsafe {
                core::ptr::write_bytes(
                    crate::mem::paging::phys_to_virt(frame) as *mut u8,
                    0,
                    PAGE as usize,
                )
            };
            page += PAGE;
        }

        // copy file bytes to their virtual positions (via translate + HHDM)
        let copy_len = (p_filesz as usize).min(p_memsz as usize);
        for k in 0..copy_len {
            let va = p_vaddr + k as u64;
            let phys = space.translate(va).ok_or("translate failed while copying")?;
            unsafe {
                core::ptr::write_volatile(
                    crate::mem::paging::phys_to_virt(phys) as *mut u8,
                    image[p_offset + k],
                )
            };
        }

        loaded.seg_count += 1;
        loaded.mapped_bytes += va_end - va_start;
    }

    if loaded.seg_count == 0 {
        return Err("no PT_LOAD segments");
    }
    if loaded.entry < vmm::USER_IMG_BASE || loaded.entry >= vmm::KERNEL_HALF_BASE {
        return Err("entry point outside the user half");
    }
    Ok(loaded)
}

/// Helper: read a whole file from the ramdisk into a fresh buffer.
pub fn read_from_ramdisk(path: &str) -> Result<Vec<u8>, &'static str> {
    let fat = crate::fs::fat32::FAT.lock();
    match fat.as_ref() {
        None => Err("ramdisk not mounted"),
        Some(fs) => fs.cat(path),
    }
}
