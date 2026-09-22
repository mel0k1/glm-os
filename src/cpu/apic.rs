//! Local APIC: detection, enable, EOI, and the LAPIC timer.
//!
//! v0.3 moves the scheduler tick off the legacy 8259 chain and onto the
//! per-CPU LAPIC timer — the mandatory first step towards SMP (every CPU
//! needs its own timer; the PIT cannot do that).
//!
//! Coexistence model ("virtual wire plus"):
//!   - 8259 PIC keeps driving PIT (uptime) and PS/2 keyboard (IRQ0/IRQ1),
//!     delivered to the CPU through LINT0 configured as ExtINT;
//!   - every external interrupt is EOIed BOTH in the PIC and in the LAPIC
//!     (once the LAPIC is enabled it swallows the EOI otherwise);
//!   - the LAPIC timer fires its own vector (0x60) for the scheduler.
//!
//! The LAPIC MMIO page (default 0xFEE00000) is reached through the HHDM
//! mapping — no extra page tables needed.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::klog;
use crate::mem::vmm::{self, LAPIC_VIRT, NO_CACHE, PRESENT, WRITABLE};

/// LAPIC timer vector (above the PIC range 0x20..0x2f, below the 0x80 gate).
pub const TIMER_VECTOR: u8 = 0x60;
/// Spurious interrupt vector (must have an IDT entry; never EOIed).
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Scheduler tick rate for the LAPIC timer.
pub const SCHED_HZ: u64 = 250;

// MMIO register offsets from the LAPIC base.
const REG_ID: u64 = 0x020;
const REG_EOI: u64 = 0x0B0;
const REG_SPURIOUS: u64 = 0x0F0;
const REG_LVT_TIMER: u64 = 0x320;
const REG_LVT_LINT0: u64 = 0x350;
const REG_LVT_LINT1: u64 = 0x360;
const REG_TIMER_ICR: u64 = 0x380; // initial count
const REG_TIMER_CCR: u64 = 0x390; // current count
const REG_TIMER_DCR: u64 = 0x3E0; // divide configuration

// LVT timer bits.
const LVT_MASK: u32 = 1 << 16;
const LVT_PERIODIC: u32 = 1 << 17;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static ONLINE: AtomicBool = AtomicBool::new(false);

#[inline]
fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack)
        );
    }
    ((hi as u64) << 32) | lo as u64
}

#[inline]
fn wrmsr(msr: u32, value: u64) {
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack)
        );
    }
}

/// CPUID leaf 1 check removed: bit 11 of IA32_APIC_BASE is hardwired to
/// "on-chip APIC present", so the MSR test in init() is authoritative
/// (and inline asm cannot name ebx on this target — LLVM reserves it).

#[inline]
fn read_reg(off: u64) -> u32 {
    unsafe { core::ptr::read_volatile((LAPIC_VIRT + off) as *const u32) }
}

#[inline]
fn write_reg(off: u64, value: u32) {
    unsafe { core::ptr::write_volatile((LAPIC_VIRT + off) as *mut u32, value) }
}

/// Is the LAPIC online (enabled by us)?
pub fn online() -> bool {
    ONLINE.load(Ordering::Relaxed)
}

/// Signal end-of-interrupt for any vector delivered through the LAPIC.
/// No-op until apic::init() succeeds, so early boot handlers stay safe.
pub fn eoi() {
    if online() {
        write_reg(REG_EOI, 0);
    }
}

/// Detect, enable and calibrate the LAPIC + its timer.
/// Must be called after the VMM is up (needs phys_to_virt).
pub fn init() {
    // IA32_APIC_BASE (MSR 0x1B): bit 11 = APIC global enable,
    // bits 12..36 = 4 KiB-aligned MMIO base (usually 0xFEE00000).
    const MSR_APIC_BASE: u32 = 0x1B;
    let msr = rdmsr(MSR_APIC_BASE);
    if msr & (1 << 11) == 0 {
        klog!("apic: LAPIC not present in MSR 0x1b, staying on legacy PIC");
        return;
    }
    let mut phys_base = msr & 0x000F_FFFF_FFF0_0000;
    if phys_base == 0 {
        phys_base = 0xFEE0_0000; // architecturally default base
    }
    LAPIC_BASE.store(phys_base, Ordering::Relaxed);

    // MMIO page into the kernel half (shared by every address space):
    // the HHDM only covers RAM, and the LAPIC lives in MMIO space.
    if let Err(e) = vmm::map_kernel_page(LAPIC_VIRT, phys_base, PRESENT | WRITABLE | NO_CACHE) {
        klog!("apic: cannot map the MMIO page ({e}), staying on legacy PIC");
        return;
    }

    let id = read_reg(REG_ID) >> 24;
    klog!(
        "apic: LAPIC found at {:#x} (msr={:#x}), id={}",
        phys_base,
        msr,
        id
    );

    // Enable the LAPIC + set the spurious vector.
    let spr = read_reg(REG_SPURIOUS);
    write_reg(REG_SPURIOUS, (spr & !0xFF) | SPURIOUS_VECTOR as u32 | (1 << 8));

    // Route the legacy 8259 output to the CPU: LINT0 = ExtINT (delivery 111),
    // unmasked. LINT1 (NMI line) stays masked.
    write_reg(REG_LVT_LINT0, 0x7_00);
    write_reg(REG_LVT_LINT1, LVT_MASK);

    // --- timer calibration against the PIT (100 Hz ticks) --------------------
    const DIVIDE_BY_16: u32 = 0x3;
    write_reg(REG_TIMER_DCR, DIVIDE_BY_16);
    // masked one-shot, loaded with a huge count; measure how much drains in
    // 10 PIT ticks = 100 ms
    write_reg(REG_LVT_TIMER, TIMER_VECTOR as u32 | LVT_MASK);
    write_reg(REG_TIMER_ICR, 0xFFFF_FFFF);
    let t0 = crate::cpu::pit::ticks();
    while crate::cpu::pit::ticks().saturating_sub(t0) < 10 {
        core::hint::spin_loop();
    }
    let counts_100ms = 0xFFFF_FFFFu32.wrapping_sub(read_reg(REG_TIMER_CCR));
    write_reg(REG_TIMER_ICR, 0); // stop the one-shot

    // counts per second = counts_100ms * 10; counts per scheduler tick:
    let per_tick = (counts_100ms as u64 * 10 / SCHED_HZ).max(16) as u32;

    // periodic, unmasked, vector 0x60 — the heartbeat of the scheduler
    write_reg(REG_LVT_TIMER, TIMER_VECTOR as u32 | LVT_PERIODIC);
    write_reg(REG_TIMER_ICR, per_tick);

    ONLINE.store(true, Ordering::Relaxed);
    klog!(
        "apic: LAPIC enabled, spurious v{}, LINT0=ExtINT (8259 via LAPIC)",
        SPURIOUS_VECTOR
    );
    klog!(
        "apic: timer calibrated: {} counts/100ms -> {} counts/tick @ {} Hz (vector {:#x})",
        counts_100ms,
        per_tick,
        SCHED_HZ,
        TIMER_VECTOR
    );
}
