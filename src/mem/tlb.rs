//! v0.6: TLB shootdown — invalidate stale translations on other CPUs.
//!
//! GLM OS reloads CR3 on every context switch and never maps one address
//! space on two CPUs at once, so in principle no CPU can hold stale user
//! translations for an address space it is not running. The shootdown is
//! still wired in as a correctness backstop after bulk PTE rewrites
//! (fork_cow marks pages read-only) — and as the foundation for shared
//! address spaces (threads) in a later version.
//!
//! Protocol: `shootdown_all_others()` sends the SHOOTDOWN_VECTOR IPI to
//! every other online CPU. The handler there does a full TLB flush (CR3
//! reload — cheap enough, and also drops any non-global kernel entries)
//! and acknowledges through a counter the sender waits on, with a bounded
//! spin so a wedged AP can never deadlock the forking CPU.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::cpu::{apic, smp};
use crate::klog;

/// IPI vector for TLB shootdowns (0x70 is the v0.4 test IPI, 0x71 ours).
pub const SHOOTDOWN_VECTOR: u8 = 0x71;

static ACKS: AtomicU32 = AtomicU32::new(0);

/// Called from the interrupt dispatcher on every CPU that receives the
/// shootdown IPI: flush the whole TLB (CR3 reload), then acknowledge.
pub fn on_ipi() {
    flush_all();
    ACKS.fetch_add(1, Ordering::AcqRel);
    apic::eoi();
}

/// Full TLB flush: reading CR3 and writing it back invalidates every
/// non-global translation on this CPU.
fn flush_all() {
    let v: u64;
    unsafe {
        core::arch::asm!(
            "mov {v}, cr3",
            "mov cr3, {v}",
            v = out(reg) v,
            options(nostack)
        );
    }
}

/// Fire a shootdown at every other online CPU and wait (bounded) for the
/// acknowledgements.
pub fn shootdown_all_others() {
    if !apic::online() {
        return; // pre-LAPIC boot: only the BSP exists, nothing to shoot
    }
    let me = smp::cpu_index();
    let mask = smp::online_mask();

    let mut targets = 0u32;
    for cpu in 0..smp::MAX_CPUS {
        if cpu != me && mask & (1 << cpu) != 0 {
            targets += 1;
        }
    }
    if targets == 0 {
        return;
    }

    ACKS.store(0, Ordering::Release);
    for cpu in 0..smp::MAX_CPUS {
        if cpu != me && mask & (1 << cpu) != 0 {
            apic::send_ipi(smp::lapic_id_of(cpu), SHOOTDOWN_VECTOR);
        }
    }

    // bounded wait: kernel code runs with IF=1 almost everywhere, so acks
    // arrive within microseconds; give up rather than deadlock
    let mut spins = 0u32;
    while ACKS.load(Ordering::Acquire) < targets {
        spins += 1;
        if spins > 50_000_000 {
            klog!(
                "tlb: shootdown ack timeout ({}/{} acks)",
                ACKS.load(Ordering::Relaxed),
                targets
            );
            break;
        }
        core::hint::spin_loop();
    }
}
