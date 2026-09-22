//! v0.8: networking — PCI probe, Intel e1000 driver, ARP/IPv4/ICMP and the
//! `netd` kernel task.
//!
//! Layer map:
//!   pci.rs    walk the config space, find 8086:100e, enable it
//!   e1000.rs  MMIO register file, RX/TX descriptor rings, NIC IRQ
//!   proto.rs  Ethernet II / ARP / IPv4 / ICMP build+parse + checksums
//!   netd.rs   kernel task: drains the NIC, answers ARP/ICMP, ping engine
//!
//! Concurrency model:
//!   - NET_LOCK guards the NIC rings, the inbound frame queue and the ARP
//!     table. The ISR drains the RX ring into the queue under this lock;
//!     netd does the same on its 5 ms tick (idempotent — both paths walk
//!     only descriptors whose DD bit is still set).
//!   - Protocol handling (ARP/ICMP replies) happens OUTSIDE the lock in
//!     netd, on a stack copy of the frame.
//!   - TX is task-context only (ping/netd/shell never run in IRQ).

pub mod e1000;
pub mod netd;
pub mod pci;
pub mod proto;
pub mod sock;

use crate::sync::Spinlock;

/// NET trunk lock (see the module doc for the ordering rules).
pub(crate) static NET_LOCK: Spinlock<()> = Spinlock::new(());

/// Boot-time network configuration for QEMU user-mode networking (slirp):
/// the guest behaves exactly like a machine that got this DHCP lease.
pub const OUR_IP: u32 = 0x0A00_020F; // 10.0.2.15
pub const OUR_MASK: u32 = 0xFFFF_FF00; // /24
pub const GW_IP: u32 = 0x0A00_0202; // 10.0.2.2

pub fn ip_str(ip: u32) -> alloc::string::String {
    alloc::format!(
        "{}.{}.{}.{}",
        ip >> 24 & 0xFF,
        ip >> 16 & 0xFF,
        ip >> 8 & 0xFF,
        ip & 0xFF
    )
}

/// Did the probe find and initialize a NIC?
pub fn online() -> bool {
    e1000::online()
}

/// Boot facts for the kmain oklines.
pub struct BootInfo {
    pub pci_slot: alloc::string::String,
    pub bar0_phys: u32,
    pub irq: u8,
    pub vector: u8,
}

/// Whole-subsystem init: PCI probe -> e1000 rings -> netd task.
/// Called once from kmain after SMP is up (netd needs the scheduler).
/// Returns boot facts for the console, or None when no NIC exists.
pub fn init() -> Option<BootInfo> {
    if !pci::probe() {
        return None;
    }
    if !e1000::init() {
        return None;
    }
    netd::spawn();
    let d = pci::found()?;
    Some(BootInfo {
        pci_slot: alloc::format!("00:{:02x}.{}", d.dev, d.fun),
        bar0_phys: d.bar0_phys,
        irq: d.irq,
        vector: 32 + d.irq,
    })
}
