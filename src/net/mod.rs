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
pub mod tcp;

use crate::klog;
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU32, Ordering};

/// NET trunk lock (see the module doc for the ordering rules).
pub(crate) static NET_LOCK: Spinlock<()> = Spinlock::new(());

// ---------------------------------------------------------------------------
// v2.6: runtime IPv4 configuration.
//
// Until now the guest address was carved in stone (boot-time consts matching
// QEMU slirp's defaults). A DHCP client in ring 3 (DHCP.ELF) needs to APPLY a
// lease, so the four numbers became atomics behind tiny accessors: readers are
// hot paths in TX/RX (checksums, next-hop, demux) and must never take a lock;
// a torn read is impossible on x86_64 aligned u32. The initial values are the
// classic slirp lease, so an OS that never runs `dhcp` behaves exactly as
// before — every pre-2.6 test passes unchanged.
// ---------------------------------------------------------------------------

static OUR_IP_A: AtomicU32 = AtomicU32::new(0x0A00_020F); // 10.0.2.15
static OUR_MASK_A: AtomicU32 = AtomicU32::new(0xFFFF_FF00); // /24
static GW_IP_A: AtomicU32 = AtomicU32::new(0x0A00_0202); // 10.0.2.2
static DNS_IP_A: AtomicU32 = AtomicU32::new(0x0A00_0203); // 10.0.2.3

#[inline]
pub fn our_ip() -> u32 {
    OUR_IP_A.load(Ordering::Relaxed)
}
#[inline]
pub fn our_mask() -> u32 {
    OUR_MASK_A.load(Ordering::Relaxed)
}
#[inline]
pub fn gw_ip() -> u32 {
    GW_IP_A.load(Ordering::Relaxed)
}
#[inline]
pub fn dns_ip() -> u32 {
    DNS_IP_A.load(Ordering::Relaxed)
}

/// v2.6: apply a full IPv4 configuration from ring 3 (SYS_NET_SETCONF,
/// driven by DHCP.ELF; also used to drop the address to 0.0.0.0 for the
/// RFC 2131 discovery dance). Gateway and DNS may be 0 while unconfigured.
/// The klog line is the machine-checkable trace the tests grep for.
pub fn set_config(ip: u32, mask: u32, gw: u32, dns: u32) -> i64 {
    OUR_MASK_A.store(mask, Ordering::Relaxed);
    OUR_IP_A.store(ip, Ordering::Relaxed);
    GW_IP_A.store(gw, Ordering::Relaxed);
    DNS_IP_A.store(dns, Ordering::Relaxed);
    klog!(
        "net: config set from ring 3: ip={} mask={} gw={} dns={}",
        ip_str(ip),
        ip_str(mask),
        ip_str(gw),
        ip_str(dns)
    );
    0
}

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
