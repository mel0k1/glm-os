//! Global Descriptor Table.
//!
//! Long mode barely needs segmentation, but a sane GDT is still mandatory:
//! null + 64-bit code (0x08) + data (0x10).

use core::arch::asm;

#[repr(C)]
struct Gdt {
    null: u64,
    code: u64, // selector 0x08
    data: u64, // selector 0x10
}

// The CPU sets the Accessed bit in descriptors on segment loads, so the GDT
// must live in a WRITABLE page (not .rodata!) — a true classic.
static mut GDT: Gdt = Gdt {
    null: 0,
    // L=1 (64-bit), DPL0, present, code
    code: 0x00AF_9A00_0000_FFFF,
    // DPL0, present, data, 4G limit
    data: 0x00CF_9200_0000_FFFF,
};

#[repr(C, packed(2))]
struct Gdtr {
    limit: u16,
    base: u64,
}

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;

pub fn init() {
    let gdtr = Gdtr {
        limit: core::mem::size_of::<Gdt>() as u16 - 1,
        base: unsafe { &raw const GDT as *const Gdt as u64 },
    };
    unsafe {
        asm!("lgdt [{}]", in(reg) &gdtr as *const Gdtr, options(nostack));
        // Far-return into the new code segment, then reload data segments.
        asm!(
            "push {sel}",
            "lea rax, [rip + 2f]",
            "push rax",
            "retfq",
            "2:",
            "mov ax, {data}",
            "mov ds, ax",
            "mov es, ax",
            "mov fs, ax",
            "mov gs, ax",
            "mov ss, ax",
            sel = const KERNEL_CODE,
            data = const KERNEL_DATA,
            out("rax") _,
            options(nostack),
        );
    }
}
