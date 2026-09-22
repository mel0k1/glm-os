//! Global Descriptor Table — one instance per CPU (v0.4 SMP).
//!
//! Layout (same selectors on every CPU):
//!   0x00 null            0x08 kernel code (DPL0)
//!   0x10 kernel data     0x18 user code   (DPL3, 64-bit)
//!   0x20 user data       0x28 TSS         (16-byte system segment)
//!
//! Each CPU needs its own TSS because RSP0 differs per CPU (it is pointed
//! at the running task's kernel stack on every switch) and each CPU gets
//! its own IST1 double-fault stack. The IDT stays shared: IST indexes are
//! resolved through the *current* CPU's TSS at interrupt time.
//!
//! The CPU sets the Accessed bit in descriptors on segment loads, so the
//! GDT must live in a WRITABLE page (not .rodata!) — a true classic.

use core::arch::asm;

use crate::klog;

pub const MAX_CPUS: usize = 8;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_CODE: u16 = 0x18;
pub const USER_DATA: u16 = 0x20;
pub const TSS_SEL: u16 = 0x28;

/// RPL-qualified selectors for iretq into ring 3.
pub const USER_CODE_RPL3: u16 = USER_CODE | 3; // 0x1b
pub const USER_DATA_RPL3: u16 = USER_DATA | 3; // 0x23

/// Size of the per-CPU ring-3 -> ring-0 fallback stack (TSS.RSP0 before the
/// scheduler assigns task stacks; never used once scheduling is online).
const RSP0_STACK_SIZE: usize = 16 * 1024;
/// Size of the per-CPU double-fault stack (TSS.IST1).
const DF_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stack {
    bytes: [u8; RSP0_STACK_SIZE],
}
#[repr(C, align(16))]
struct DfStack {
    bytes: [u8; DF_STACK_SIZE],
}

static mut RSP0_STACKS: [Stack; MAX_CPUS] =
    [const { Stack { bytes: [0; RSP0_STACK_SIZE] } }; MAX_CPUS];
static mut DF_STACKS: [DfStack; MAX_CPUS] =
    [const { DfStack { bytes: [0; DF_STACK_SIZE] } }; MAX_CPUS];

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

static mut TSS_TABLE: [Tss; MAX_CPUS] = [const {
    Tss {
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
    }
}; MAX_CPUS];

#[repr(C)]
struct Gdt {
    null: u64,
    code: u64,  // 0x08 kernel code
    data: u64,  // 0x10 kernel data
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

static mut GDT_TABLE: [Gdt; MAX_CPUS] = [const { Gdt {
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
} }; MAX_CPUS];

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

/// Point this CPU's RSP0 at a task's kernel stack: every ring3 -> ring0
/// entry (interrupt or int 0x80) pushes the trap frame THERE. Called by
/// the scheduler on every task switch, with the CPU's own index.
pub fn set_rsp0(cpu: usize, top: u64) {
    unsafe { TSS_TABLE[cpu].rsp0 = top }
}

/// Install this CPU's GDT + TSS and switch to them. Works identically on
/// the BSP (index 0, from kmain) and on APs (from ap_entry).
pub fn init_cpu(cpu: usize) {
    let tss_base = unsafe { &raw const TSS_TABLE[cpu] as *const Tss as u64 };
    let tss_limit = core::mem::size_of::<Tss>() as u64 - 1;
    let (tss_lo, tss_hi) = build_tss_descriptor(tss_base, tss_limit);

    let rsp0_top =
        unsafe { &raw const RSP0_STACKS[cpu] as *const Stack as u64 } + RSP0_STACK_SIZE as u64;
    let ist1_top = unsafe { &raw const DF_STACKS[cpu] as *const DfStack as u64 } + DF_STACK_SIZE as u64;
    unsafe {
        TSS_TABLE[cpu].rsp0 = rsp0_top;
        TSS_TABLE[cpu].ist1 = ist1_top;
        TSS_TABLE[cpu].iopb = 104;
    }

    unsafe {
        let gdt = &raw mut GDT_TABLE[cpu];
        (*gdt).tss_lo = tss_lo;
        (*gdt).tss_hi = tss_hi;
        let gdtr = Gdtr {
            limit: core::mem::size_of::<Gdt>() as u16 - 1,
            base: gdt as u64,
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
        "gdt: cpu{} tss @ {:#x} rsp0={:#x} ist1={:#x}",
        cpu,
        tss_base,
        rsp0_top,
        ist1_top
    );
}

/// BSP bootstrap: install CPU 0's descriptors.
pub fn init() {
    init_cpu(0);
}
