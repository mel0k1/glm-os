//! Global Descriptor Table.
//!
//! v0.2: full protected-flat model for ring transitions —
//!   0x00 null            0x08 kernel code (DPL0)
//!   0x10 kernel data     0x18 user code   (DPL3, 64-bit)
//!   0x20 user data       0x28 TSS         (16-byte system segment)
//!
//! The TSS supplies RSP0 (kernel stack entered on ring3 -> ring0 interrupts
//! and gates) and IST1 (dedicated stack for double faults).
//!
//! The CPU sets the Accessed bit in descriptors on segment loads, so the GDT
//! must live in a WRITABLE page (not .rodata!) — a true classic.

use core::arch::asm;

use crate::klog;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_CODE: u16 = 0x18;
pub const USER_DATA: u16 = 0x20;
pub const TSS_SEL: u16 = 0x28;

/// RPL-qualified selectors for iretq into ring 3.
pub const USER_CODE_RPL3: u16 = USER_CODE | 3; // 0x1b
pub const USER_DATA_RPL3: u16 = USER_DATA | 3; // 0x23

/// Size of the ring-3 -> ring-0 interrupt stack (TSS.RSP0).
const RSP0_STACK_SIZE: usize = 64 * 1024;
/// Size of the double-fault stack (TSS.IST1).
const DF_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stack {
    bytes: [u8; RSP0_STACK_SIZE],
}
#[repr(C, align(16))]
struct DfStack {
    bytes: [u8; DF_STACK_SIZE],
}

static mut RSP0_STACK: Stack = Stack { bytes: [0; RSP0_STACK_SIZE] };
static mut DF_STACK: DfStack = DfStack { bytes: [0; DF_STACK_SIZE] };

/// 64-bit Task State Segment (104 bytes).
#[repr(C, packed)]
struct Tss {
    rsv0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    rsv1: u64,
    ist1: u64,
    ist2: u64,
    ist3: u64,
    ist4: u64,
    ist5: u64,
    ist6: u64,
    ist7: u64,
    rsv2: u64,
    iopb: u16,
}

static mut TSS: Tss = Tss {
    rsv0: 0,
    rsp0: 0,
    rsp1: 0,
    rsp2: 0,
    rsv1: 0,
    ist1: 0,
    ist2: 0,
    ist3: 0,
    ist4: 0,
    ist5: 0,
    ist6: 0,
    ist7: 0,
    rsv2: 0,
    iopb: 104,
};

#[repr(C)]
struct Gdt {
    null: u64,
    code: u64, // 0x08 kernel code
    data: u64, // 0x10 kernel data
    ucode: u64, // 0x18 user code (DPL3)
    udata: u64, // 0x20 user data (DPL3)
    tss_lo: u64, // 0x28 TSS (16-byte descriptor, takes two slots)
    tss_hi: u64,
}

#[repr(C, packed(2))]
struct Gdtr {
    limit: u16,
    base: u64,
}

pub fn task_kstack_top() -> u64 {
    (&raw const RSP0_STACK as *const Stack as u64) + RSP0_STACK_SIZE as u64
}

fn build_tss_descriptor(base: u64, limit: u64) -> (u64, u64) {
    // 16-byte system descriptor layout:
    //   [0..2)  limit lo     [2..4)  base lo     [4]  base bits 16-23
    //   [5]     attr 0x89    [6]     limit hi    [7]  base bits 24-31  <- the classic bug
    //   [8..12) base bits 32-63
    let lo = (limit & 0xFFFF)
        | ((base & 0xFFFF) << 16)
        | (((base >> 16) & 0xFF) << 32)
        | (0x89u64 << 40) // present, 64-bit available TSS
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let hi = (base >> 32) & 0xFFFF_FFFF;
    (lo, hi)
}

pub fn init() {
    let tss_base = unsafe { &raw const TSS as *const Tss as u64 };
    let tss_limit = core::mem::size_of::<Tss>() as u64 - 1;
    let (tss_lo, tss_hi) = build_tss_descriptor(tss_base, tss_limit);

    unsafe {
        TSS.rsp0 = task_kstack_top();
        TSS.ist1 = (&raw const DF_STACK as *const DfStack as u64) + DF_STACK_SIZE as u64;
        TSS.iopb = 104;
    }

    static mut GDT_INSTANCE: Gdt = Gdt {
        null: 0,
        // L=1, DPL0, present, code
        code: 0x00AF_9A00_0000_FFFF,
        // DPL0, present, data, 4G limit
        data: 0x00CF_9200_0000_FFFF,
        // L=1, DPL3, present, code (attr 0xFA)
        ucode: 0x00AF_FA00_0000_FFFF,
        // DPL3, present, data, 4G limit (attr 0xF2)
        udata: 0x00CF_F200_0000_FFFF,
        tss_lo: 0,
        tss_hi: 0,
    };
    unsafe {
        GDT_INSTANCE.tss_lo = tss_lo;
        GDT_INSTANCE.tss_hi = tss_hi;
        let gdtr = Gdtr {
            limit: core::mem::size_of::<Gdt>() as u16 - 1,
            base: &raw const GDT_INSTANCE as *const Gdt as u64,
        };
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
        // activate the TSS (LTR takes a register/memory operand, not immediate)
        asm!(
            "mov ax, {sel}",
            "ltr ax",
            sel = const TSS_SEL,
            out("ax") _,
            options(nomem, nostack)
        );
    }
    klog!(
        "gdt: tss @ {:#x} rsp0={:#x} ist1={:#x}",
        tss_base,
        unsafe { TSS.rsp0 },
        unsafe { TSS.ist1 }
    );
}
