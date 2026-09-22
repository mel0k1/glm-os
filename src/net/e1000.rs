//! Intel 82540EM (QEMU "e1000") driver: MMIO register file + RX/TX rings.
//!
//! The 8254x TX/RX descriptor formats are the legacy 16-byte ones:
//!
//! TX: [addr u64][len u16][cso u8][cmd u8][status u8][css u8][special u16]
//! RX: [addr u64][len u16][csum u16][status u8][errors u8][special u16]
//!
//! RX model: the ISR and netd both call `drain_rx` (idempotent under
//! NET_LOCK — only descriptors whose DD bit is set are consumed). Parsed
//! frames go into a small in-kernel queue; netd pops and handles them.
//! TX model: task-context only, one descriptor per frame, DD-wait.

use crate::io::ports::inb;
use crate::klog;
use crate::mem::frames;
use crate::mem::paging::phys_to_virt;
use crate::mem::vmm::{map_kernel_page, NO_CACHE, NO_EXECUTE, PAGE, PRESENT, WRITABLE};
use crate::net::pci;
use crate::net::NET_LOCK;

// ---------------------------------------------------------------------------
// MMIO window
// ---------------------------------------------------------------------------

/// Fixed kernel VA for the 128 KiB BAR0 window (PML4[511]/PDPT[511],
/// below the LAPIC page at 0xFFFF_FFFF_FEE0_0000, above the kernel image).
const E1000_VIRT: u64 = 0xFFFF_FFFF_FE00_0000;
const MMIO_PAGES: usize = 32;

// Register offsets (8254x)
const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_ICR: usize = 0x00C0;
const REG_IMS: usize = 0x00D0;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_MTA: usize = 0x5200;
const REG_RAL: usize = 0x5400;
const REG_RAH: usize = 0x5404;

// CTRL bits
const CTRL_RST: u32 = 1 << 26;
const CTRL_SLU: u32 = 1 << 6;
// STATUS bits
const STATUS_LU: u32 = 1 << 1;
// ICR/IMS bits
const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXDW: u32 = 1 << 7;
// RCTL bits
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2; // strip CRC
// TCTL bits
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3; // pad short packets
// TX desc cmd bits
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
// RX/TX desc status DD bit
const DESC_DD: u8 = 1;

const RING_RX: usize = 32;
const RING_TX: usize = 16;
/// RX buffer size fixed by RCTL.BSIZE=00.
const RX_BUF: usize = 2048;

#[repr(C)]
struct TxDesc {
    addr: u64,
    len: u16,
    cso: u8,
    cmd: u8,
    status: u8,
    css: u8,
    special: u16,
}
#[repr(C)]
struct RxDesc {
    addr: u64,
    len: u16,
    csum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

static mut MMIO: usize = 0;
static mut RX_DESC: *mut RxDesc = core::ptr::null_mut();
static mut TX_DESC: *mut TxDesc = core::ptr::null_mut();
static mut RX_BUF_PHYS: u64 = 0; // contiguous 64 KiB (RING_RX x 2048)
static mut TX_BUF_PHYS: u64 = 0; // contiguous 32 KiB (RING_TX x 2048)
static mut NEXT_RX: usize = 0;
static mut LAST_ICR: u32 = 0;

static ONLINE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

// Counters (Relaxed atomics — diagnostics only; lock-free so the ISR can
// never get tangled in a lock ordering with task context)
use core::sync::atomic::{AtomicU64, Ordering as AO};

pub static IRQ_N: AtomicU64 = AtomicU64::new(0);
pub static IRQ_KICKS: AtomicU64 = AtomicU64::new(0);
pub static RX_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static TX_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn counters() -> (u64, u64, u64, u64, u64) {
    (
        IRQ_N.load(AO::Relaxed),
        RX_TOTAL.load(AO::Relaxed),
        TX_TOTAL.load(AO::Relaxed),
        DROPPED.load(AO::Relaxed),
        IRQ_KICKS.load(AO::Relaxed),
    )
}

pub fn online() -> bool {
    ONLINE.load(core::sync::atomic::Ordering::Relaxed)
}

#[inline]
fn reg(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((MMIO + off) as *const u32) }
}
#[inline]
fn set_reg(off: usize, val: u32) {
    unsafe { core::ptr::write_volatile((MMIO + off) as *mut u32, val) }
}

pub fn mac() -> [u8; 6] {
    let lo = reg(REG_RAL);
    let hi = reg(REG_RAH);
    [
        (lo & 0xFF) as u8,
        (lo >> 8 & 0xFF) as u8,
        (lo >> 16 & 0xFF) as u8,
        (lo >> 24 & 0xFF) as u8,
        (hi & 0xFF) as u8,
        (hi >> 8 & 0xFF) as u8,
    ]
}

pub fn mac_str() -> alloc::string::String {
    let m = mac();
    alloc::format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        m[0], m[1], m[2], m[3], m[4], m[5]
    )
}

pub fn link_up() -> bool {
    reg(REG_STATUS) & STATUS_LU != 0
}

fn nic_vector() -> u8 {
    match pci::found() {
        Some(d) => 32 + d.irq,
        None => 0,
    }
}

/// The IDT asks: is vector `v` mine?
pub fn is_our_vector(v: u64) -> bool {
    online() && v == nic_vector() as u64
}

/// ISR entry (called from idt.rs; EOIs happen there).
///
/// Deliberately does NOT touch the RX ring: the ring belongs to netd
/// (NET_LOCK holder). Taking NET_LOCK here could deadlock — external IRQs
/// always land on the BSP, which may already hold the lock with IF=0 —
/// so the ISR only counts and lets netd's 5 ms tick do the draining.
pub fn on_irq() {
    let icr = reg(REG_ICR); // read-clear
    unsafe { LAST_ICR = icr };
    IRQ_N.fetch_add(1, AO::Relaxed);
    let n = IRQ_N.load(AO::Relaxed);
    // log the first few interrupts for bring-up diagnostics
    if n <= 8 {
        klog!("e1000: irq #{} icr={:#x}", n, icr);
    }
    if icr & ICR_RXDW != 0 {
        IRQ_KICKS.fetch_add(1, AO::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

pub fn init() -> bool {
    let Some(dev) = pci::found() else { return false };

    // --- map BAR0 ---------------------------------------------------------
    let phys = dev.bar0_phys as u64;
    for i in 0..MMIO_PAGES {
        let va = E1000_VIRT + (i as u64) * PAGE;
        if map_kernel_page(va, phys + (i as u64) * PAGE, PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE)
            .is_err()
        {
            klog!("e1000: failed to map mmio page {}", i);
            return false;
        }
    }
    unsafe { MMIO = E1000_VIRT as usize };

    // --- reset -------------------------------------------------------------
    set_reg(REG_CTRL, reg(REG_CTRL) | CTRL_RST);
    for _ in 0..20 {
        if reg(REG_CTRL) & CTRL_RST == 0 {
            break;
        }
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
    }
    set_reg(REG_CTRL, reg(REG_CTRL) | CTRL_SLU); // force link up (QEMU)

    // --- MAC: QEMU reloads RAL/RAH from its EEPROM at reset ----------------
    let m = mac();
    if m[0] == 0 && m[1] == 0 && m[2] == 0 {
        // no EEPROM MAC: program our "GLM" locally-administered address
        let lo = 0x4C4D_0047u32; // 'G','L','M',0x00 (LE)
        let hi = 0x8000_0001u32; // bytes 4..5 = 00 01 + AV bit
        set_reg(REG_RAL, lo);
        set_reg(REG_RAH, hi);
        klog!("e1000: empty ral/rah, programmed 47:4C:4D:00:00:01");
    }

    // --- multicast filter table: accept-all is unnecessary, clear it -------
    for i in 0..128 {
        set_reg(REG_MTA + i * 4, 0);
    }

    // --- descriptor rings + buffers ----------------------------------------
    let (Some(rx_ring), Some(tx_ring), Some(rxb), Some(txb)) = (
        frames::alloc_contig(1),
        frames::alloc_contig(1),
        frames::alloc_contig((RING_RX * RX_BUF / 4096) as usize),
        frames::alloc_contig((RING_TX * RX_BUF / 4096) as usize),
    ) else {
        klog!("e1000: out of frames for rings");
        return false;
    };
    unsafe {
        core::ptr::write_bytes(phys_to_virt(rx_ring) as *mut u8, 0, 4096);
        core::ptr::write_bytes(phys_to_virt(tx_ring) as *mut u8, 0, 4096);
        RX_DESC = phys_to_virt(rx_ring) as *mut RxDesc;
        TX_DESC = phys_to_virt(tx_ring) as *mut TxDesc;
        RX_BUF_PHYS = rxb;
        TX_BUF_PHYS = txb;
        NEXT_RX = 0;
    }

    // --- TX ring -------------------------------------------------------------
    set_reg(REG_TDBAL, tx_ring as u32);
    set_reg(REG_TDBAH, (tx_ring >> 32) as u32);
    set_reg(REG_TDLEN, (RING_TX * 16) as u32);
    set_reg(REG_TDH, 0);
    set_reg(REG_TDT, 0);
    set_reg(REG_TCTL, TCTL_EN | TCTL_PSP | (0x0F << 4) | (0x40 << 20));

    // --- RX ring -------------------------------------------------------------
    set_reg(REG_RDBAL, rx_ring as u32);
    set_reg(REG_RDBAH, (rx_ring >> 32) as u32);
    set_reg(REG_RDLEN, (RING_RX * 16) as u32);
    set_reg(REG_RDH, 0);
    set_reg(REG_RDT, (RING_RX - 1) as u32);
    // RX buffers: addr per descriptor
    for i in 0..RING_RX {
        let addr = rxb + (i as u64) * RX_BUF as u64;
        unsafe {
            (*RX_DESC.add(i)).addr = addr;
            (*RX_DESC.add(i)).status = 0;
        }
    }
    set_reg(REG_RCTL, RCTL_EN | RCTL_SBP); // BSIZE=00 -> 2048-byte buffers

    // --- interrupts ------------------------------------------------------------
    let _ = reg(REG_ICR); // clear pending
    set_reg(REG_IMS, ICR_RXDW | ICR_LSC);
    let v = nic_vector();
    if (3..=15).contains(&dev.irq) {
        unsafe {
            crate::cpu::pic::unmask(dev.irq);
        }
        klog!(
            "e1000: rings up ({} rx/{} tx desc), irq line {} -> vector {:#x}, ims=rxdw|lsc",
            RING_RX,
            RING_TX,
            dev.irq,
            v
        );
    } else {
        klog!("e1000: odd irq line {}, interrupts left masked", dev.irq);
    }

    ONLINE.store(true, core::sync::atomic::Ordering::Relaxed);
    true
}

// ---------------------------------------------------------------------------
// TX
// ---------------------------------------------------------------------------

/// Send one ethernet frame (task context only). Blocks until the NIC
/// reports the descriptor done (DD) or ~100 ms pass.
pub fn send_frame(frame: &[u8]) -> bool {
    if !online() || frame.is_empty() || frame.len() > RX_BUF {
        return false;
    }
    let _g = NET_LOCK.lock();
    let tdt = reg(REG_TDT) as usize % RING_TX;
    unsafe {
        let buf_va = phys_to_virt(TX_BUF_PHYS + (tdt as u64) * RX_BUF as u64);
        core::ptr::copy_nonoverlapping(frame.as_ptr(), buf_va as *mut u8, frame.len());
        let d = &mut *TX_DESC.add(tdt);
        d.addr = TX_BUF_PHYS + (tdt as u64) * RX_BUF as u64;
        d.len = frame.len() as u16;
        d.cso = 0;
        d.cmd = TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS;
        d.css = 0;
        d.special = 0;
        d.status = 0;
        let _ = inb(0x80); // descriptor write posting fence
        set_reg(REG_TDT, ((tdt + 1) % RING_TX) as u32);
        // wait for completion
        for _ in 0..2_000_000 {
            if (*TX_DESC.add(tdt)).status & DESC_DD != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        if (*TX_DESC.add(tdt)).status & DESC_DD != 0 {
            TX_TOTAL.fetch_add(1, AO::Relaxed);
            true
        } else {
            klog!(
                "e1000: tx timeout slot={} len={} tdh={} tdt={}",
                tdt,
                frame.len(),
                reg(REG_TDH),
                reg(REG_TDT)
            );
            false
        }
    }
}

// ---------------------------------------------------------------------------
// RX
// ---------------------------------------------------------------------------

/// Inbound frame queue: filled by the ISR/netd drain, popped by netd.
static mut RXQ: [[u8; RX_BUF]; 32] = [[0; RX_BUF]; 32];
static mut RXQ_LEN: [usize; 32] = [0; 32];
static mut RXQ_HEAD: usize = 0;
static mut RXQ_TAIL: usize = 0;

/// Walk the RX ring, move every completed frame into RXQ. Returns the
/// number of frames moved. Safe from ISR and netd (NET_LOCK held).
/// Must NOT lock IRQ_EVENTS (on_irq calls us while holding it).
fn drain_rx() -> usize {
    let mut moved = 0usize;
    unsafe {
        for _ in 0..RING_RX {
            let d = &*RX_DESC.add(NEXT_RX);
            if d.status & DESC_DD == 0 {
                break;
            }
            let len = (d.len as usize).min(RX_BUF);
            let src = phys_to_virt(RX_BUF_PHYS + (NEXT_RX as u64) * RX_BUF as u64) as *const u8;
            let tail = RXQ_TAIL;
            let next_tail = (tail + 1) % RXQ.len();
            if next_tail != RXQ_HEAD {
                core::ptr::copy_nonoverlapping(src, RXQ[tail].as_mut_ptr(), len);
                RXQ_LEN[tail] = len;
                RXQ_TAIL = next_tail;
                moved += 1;
            } else {
                DROPPED.fetch_add(1, core::sync::atomic::Ordering::Relaxed); // queue full
            }
            // hand the buffer back to the NIC
            (*RX_DESC.add(NEXT_RX)).status = 0;
            set_reg(REG_RDT, NEXT_RX as u32);
            NEXT_RX = (NEXT_RX + 1) % RING_RX;
        }
    }
    RX_TOTAL.fetch_add(moved as u64, core::sync::atomic::Ordering::Relaxed);
    moved
}

/// netd safety net: if the ISR lost a frame (spurious EOI race etc.) the
/// 5 ms tick drains the ring anyway. Returns frames moved.
pub fn poll_drain() -> usize {
    let _g = NET_LOCK.lock();
    drain_rx()
}

/// Pop one frame into `out`. Returns its length.
pub fn pop_rx(out: &mut [u8; RX_BUF]) -> usize {
    unsafe {
        if RXQ_HEAD == RXQ_TAIL {
            return 0;
        }
        let len = RXQ_LEN[RXQ_HEAD];
        out[..len].copy_from_slice(&RXQ[RXQ_HEAD][..len]);
        RXQ_HEAD = (RXQ_HEAD + 1) % RXQ.len();
        len
    }
}
