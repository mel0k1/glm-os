//! Symmetric multiprocessing (v0.4) — built on the Limine MP protocol.
//!
//! Limine itself brings every CPU up to long mode and parks them spinning
//! on a per-CPU `goto_address` slot. Writing that slot releases the CPU:
//! it jumps into `ap_entry` with interrupts off, the kernel's page tables
//! active (higher half + HHDM, exactly what the BSP booted with) and its
//! own dedicated stack. No INIT/SIPI/16-bit trampoline of our own — the
//! bootloader does the ugly part, we do the interesting part.
//!
//! Per-CPU model for v0.4 (GS_BASE per-CPU areas are queued for v0.5):
//!   - every CPU is assigned an index: BSP = 0, APs = 1.. in MP response order;
//!   - the index is resolved from the CPU-local LAPIC ID register;
//!   - per-CPU state (current task, switch request, counters) lives in
//!     static arrays indexed by that number;
//!   - each AP gets a resident kidle task pinned to it, so the scheduler
//!     always has a valid "current" and the CPU can idle with hlt.
//!
//! External interrupts (PIT, keyboard through the 8259) remain BSP-only:
//! APs mask LINT0. Every CPU still preempts its own user tasks via its
//! own LAPIC timer, so parallelism is real, not cosmetic.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::klog;
use crate::sched;
use limine::mp::Cpu;
use limine::request::MpRequest;

pub use crate::cpu::gdt::MAX_CPUS;

#[used]
#[link_section = ".limine_requests"]
static MP_REQUEST: MpRequest = MpRequest::new();

// ---------------------------------------------------------------------------
// Per-CPU state (indexed by cpu index, not APIC id)
// ---------------------------------------------------------------------------

static CPU_LAPIC_IDS: [AtomicU32; MAX_CPUS] =
    [const { AtomicU32::new(0xFFFF_FFFF) }; MAX_CPUS];
static CURRENT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];
static SWITCH_REQUESTED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static CPU_SWITCHES: [AtomicU64Alias; MAX_CPUS] = [const { AtomicU64Alias::new(0) }; MAX_CPUS];
static ONLINE_BITS: AtomicUsize = AtomicUsize::new(0);
static KIDLE_SLOT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(usize::MAX) }; MAX_CPUS];
static IPI_RECV: [AtomicU64Alias; MAX_CPUS] = [const { AtomicU64Alias::new(0) }; MAX_CPUS];
static CPU_COUNT: AtomicUsize = AtomicUsize::new(0);

type AtomicU64Alias = core::sync::atomic::AtomicU64;

/// The MP response, stashed by init() for start_aps().
static mut MP_CPUS: Option<&'static [&'static Cpu]> = None;
static mut BSP_LAPIC_ID: u32 = 0xFFFF_FFFF;

pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Relaxed)
}

pub fn online_mask() -> usize {
    ONLINE_BITS.load(Ordering::Relaxed)
}

pub fn cpu_switches(cpu: usize) -> u64 {
    CPU_SWITCHES[cpu].load(Ordering::Relaxed)
}

pub fn ipi_recv(cpu: usize) -> u64 {
    IPI_RECV[cpu].load(Ordering::Relaxed)
}

pub fn lapic_id_of(cpu: usize) -> u32 {
    CPU_LAPIC_IDS[cpu].load(Ordering::Relaxed)
}

pub fn current_task_idx(cpu: usize) -> usize {
    CURRENT[cpu].load(Ordering::Relaxed)
}

/// Which CPU am I? One MMIO read of the CPU-local LAPIC ID register +
/// a tiny cached-memory scan. Called on every interrupt/syscall — the
/// GS_BASE fast path is queued for v0.5.
#[inline]
pub fn cpu_index() -> usize {
    if !crate::cpu::apic::online() {
        return 0; // pre-LAPIC boot: the only alive CPU is the BSP
    }
    let id = crate::cpu::apic::read_id();
    for i in 0..MAX_CPUS {
        if CPU_LAPIC_IDS[i].load(Ordering::Relaxed) == id {
            return i;
        }
    }
    0
}

#[inline]
pub fn request_switch(cpu: usize) {
    SWITCH_REQUESTED[cpu].store(true, Ordering::Release);
}

#[inline]
pub fn take_switch_request(cpu: usize) -> bool {
    SWITCH_REQUESTED[cpu].swap(false, Ordering::AcqRel)
}

#[inline]
pub fn set_current_task(cpu: usize, slot: usize) {
    CURRENT[cpu].store(slot, Ordering::Release);
}

#[inline]
pub fn count_switch(cpu: usize) {
    CPU_SWITCHES[cpu].fetch_add(1, Ordering::Relaxed);
}

/// IPI test vector bookkeeping (see idt.rs, vector 0x70).
pub fn on_ipi_received() {
    let cpu = cpu_index();
    IPI_RECV[cpu].fetch_add(1, Ordering::Relaxed);
}

/// Send the IPI-test vector (0x70) to CPU `to`.
pub fn send_test_ipi(to: usize) -> Result<(), &'static str> {
    if to >= MAX_CPUS || CPU_LAPIC_IDS[to].load(Ordering::Relaxed) == 0xFFFF_FFFF {
        return Err("no such cpu");
    }
    if ONLINE_BITS.load(Ordering::Relaxed) & (1 << to) == 0 {
        return Err("cpu not online");
    }
    const TEST_VECTOR: u8 = 0x70;
    crate::cpu::apic::send_ipi(lapic_id_of(to), TEST_VECTOR);
    Ok(())
}

// ---------------------------------------------------------------------------
// init (BSP side): enumerate the CPUs Limine brought up
// ---------------------------------------------------------------------------

pub fn init() {
    let Some(resp) = MP_REQUEST.get_response() else {
        CPU_COUNT.store(1, Ordering::Relaxed);
        klog!("smp: no MP response from the bootloader - single core");
        return;
    };

    unsafe {
        BSP_LAPIC_ID = resp.bsp_lapic_id();
        MP_CPUS = Some(resp.cpus());
    }
    let bsp = resp.bsp_lapic_id();
    let cpus = resp.cpus();

    // assign indices: BSP first (0), then the others in response order
    let mut next = 0usize;
    for cpu in cpus {
        let id = cpu.lapic_id;
        let idx = if id == bsp { 0 } else { next + 1 };
        CPU_LAPIC_IDS[idx].store(id, Ordering::Relaxed);
        if id != bsp {
            next += 1;
        }
    }
    let total = cpus.len().min(MAX_CPUS);
    CPU_COUNT.store(total, Ordering::Relaxed);
    ONLINE_BITS.store(1, Ordering::Relaxed); // BSP is online from the start
    klog!(
        "smp: {} cpu(s) via limine mp (bsp lapic id {})",
        total,
        bsp
    );
}

// ---------------------------------------------------------------------------
// start_aps (BSP side): pin a kidle to every AP, then release it
// ---------------------------------------------------------------------------

pub fn start_aps() {
    let (cpus, bsp) = unsafe {
        match MP_CPUS {
            Some(c) => (c, BSP_LAPIC_ID),
            None => return,
        }
    };

    for cpu in cpus {
        let id = cpu.lapic_id;
        if id == bsp {
            continue; // the BSP is already running the shell
        }
        let idx = (0..MAX_CPUS)
            .find(|i| CPU_LAPIC_IDS[*i].load(Ordering::Relaxed) == id)
            .unwrap_or(MAX_CPUS);
        if idx >= MAX_CPUS {
            continue;
        }

        // a resident idle task pinned to this CPU: the AP's initial "current"
        let Some(slot) = sched::spawn_ap_kidle(idx) else {
            klog!("smp: task table full, cpu{} stays offline", idx);
            continue;
        };
        KIDLE_SLOT[idx].store(slot, Ordering::Relaxed);

        // release the spinning CPU; Limine synchronizes the write
        cpu.goto_address.write(ap_entry);
        klog!("smp: released cpu{} (lapic id {})", idx, id);

        // wait up to ~3 s for the AP to report in
        let t0 = crate::cpu::pit::ticks();
        while crate::cpu::pit::ticks().saturating_sub(t0) < 300 {
            if ONLINE_BITS.load(Ordering::Relaxed) & (1 << idx) != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if ONLINE_BITS.load(Ordering::Relaxed) & (1 << idx) == 0 {
            klog!("smp: cpu{} did not come online in time", idx);
        }
    }
}

// ---------------------------------------------------------------------------
// AP side: first kernel code a secondary CPU runs
// ---------------------------------------------------------------------------

/// Entry of every application processor. Long mode, kernel page tables,
/// interrupts off, own stack (courtesy of the Limine MP protocol);
/// the only argument is a pointer to the MP CPU descriptor.
pub unsafe extern "C" fn ap_entry(cpu: &Cpu) -> ! {
    let id = cpu.lapic_id;
    let idx = (0..MAX_CPUS)
        .find(|i| CPU_LAPIC_IDS[*i].load(Ordering::Relaxed) == id)
        .unwrap_or(0);

    // the MP protocol starts APs with IDTR = 0 — load the shared IDT
    // before anything can raise (or deliver) an interrupt
    crate::cpu::idt::load();

    // own GDT/TSS (rsp0 + IST1 for this CPU), LAPIC + timer, then in
    gdt_boot_cpu(idx);
    crate::cpu::apic::init_ap(idx);

    let slot = KIDLE_SLOT[idx].load(Ordering::Relaxed);
    if slot == usize::MAX {
        // no kidle was staged (task table full): park forever, quietly
        loop {
            asm!("hlt");
        }
    }
    sched::ap_go(idx, slot);
    ONLINE_BITS.fetch_or(1 << idx, Ordering::Release);

    // hand the CPU to its kidle: switch to the kidle's kernel stack and
    // loop there (yield via int 0x80 -> the scheduler takes over).
    // Interrupts on: this CPU's LAPIC timer drives its own preemption.
    crate::cpu::enable_interrupts();
    let kstack = sched::task_kstack_top(slot);
    klog!("smp: cpu{} online, running kidle (slot {})", idx, slot);
    asm!(
        "mov rsp, {rsp}",
        "xor ebp, ebp",
        "call {main}",
        rsp = in(reg) kstack,
        main = sym sched::ap_idle_main,
        options(noreturn)
    );
}

fn gdt_boot_cpu(idx: usize) {
    crate::cpu::gdt::init_cpu(idx);
}
