//! PCI configuration space probe: find the NIC, wire it up.
//!
//! The 82540EM (QEMU's e1000) sits on bus 0. We walk device/function 0,
//! match vendor:device against 8086:100e, then:
//!   - capture BAR0 (MMIO window) and the firmware-routed IRQ line
//!   - enable memory decoding + bus mastering in the command register.
//!
//! Ports 0xCF8/0xCFC are the standard PCI CONFIG_ADDRESS / CONFIG_DATA.

use crate::io::ports::{inl, outl};
use crate::klog;

const CFG_ADDR: u16 = 0xCF8;
const CFG_DATA: u16 = 0xCFC;

pub const VENDOR_INTEL: u16 = 0x8086;
pub const DEVICE_E1000: u16 = 0x100E; // 82540EM, QEMU's default e1000

#[inline]
fn cfg_read_dword(bus: u8, dev: u8, fun: u8, off: u8) -> u32 {
    let addr = 0x8000_0000
        | (bus as u32) << 16
        | (dev as u32) << 11
        | (fun as u32) << 8
        | (off as u32 & 0xFC);
    unsafe {
        outl(CFG_ADDR, addr);
        inl(CFG_DATA)
    }
}

#[inline]
fn cfg_write_dword(bus: u8, dev: u8, fun: u8, off: u8, val: u32) {
    let addr = 0x8000_0000
        | (bus as u32) << 16
        | (dev as u32) << 11
        | (fun as u32) << 8
        | (off as u32 & 0xFC);
    unsafe {
        outl(CFG_ADDR, addr);
        outl(CFG_DATA, val);
    }
}

/// Everything the rest of the stack needs to know about the NIC slot.
pub struct PciDev {
    pub bus: u8,
    pub dev: u8,
    pub fun: u8,
    pub vendor: u16,
    pub device: u16,
    /// BAR0 — 32-bit MMIO base (physical).
    pub bar0_phys: u32,
    /// ISA IRQ the firmware routed this device to (config 0x3C).
    pub irq: u8,
}

static mut FOUND: Option<PciDev> = None;

pub fn found() -> Option<&'static PciDev> {
    unsafe { (&raw const FOUND).as_ref().and_then(|o| o.as_ref()) }
}

/// Scan bus 0 for the e1000. Returns true and fills `FOUND` on success.
pub fn probe() -> bool {
    for dev in 0..32u8 {
        let fun = 0u8;
        let id = cfg_read_dword(0, dev, fun, 0x00);
        let vendor = (id & 0xFFFF) as u16;
        let device = (id >> 16) as u16;
        if vendor == 0xFFFF {
            continue; // empty slot
        }
        let class_rev = cfg_read_dword(0, dev, fun, 0x08);
        let class = (class_rev >> 24) as u8;
        let subclass = (class_rev >> 16) as u8;
        if class == 0x02 && subclass == 0x00 {
            klog!(
                "pci: 00:{:02x}.{} network controller {:#06x}:{:#06x}",
                dev,
                fun,
                vendor,
                device
            );
        }
        if vendor == VENDOR_INTEL && device == DEVICE_E1000 {
            // BAR0: read, size it, rewrite the original (standard BAR dance;
            // for e1000 it is a fixed 128 KiB MMIO window so we keep it simple)
            let bar0 = cfg_read_dword(0, dev, fun, 0x10);
            if bar0 & 1 != 0 {
                klog!("pci: e1000 bar0 is i/o ports - unexpected, skipping");
                continue;
            }
            // Command register: enable memory space + bus master + no mem writes
            let cmd = cfg_read_dword(0, dev, fun, 0x04) & 0xFFFF;
            cfg_write_dword(0, dev, fun, 0x04, cmd | 0x6); // MEM | BUS_MASTER
            let bytes = cfg_read_dword(0, dev, fun, 0x3C).to_le_bytes();
            let irq = bytes[0]; // Interrupt Line (firmware-routed ISA irq)
            let pin = bytes[1]; // Interrupt Pin (INTA# = 1)
            unsafe {
                FOUND = Some(PciDev {
                    bus: 0,
                    dev,
                    fun,
                    vendor,
                    device,
                    bar0_phys: bar0 & 0xFFFF_FFF0,
                    irq,
                });
            }
            klog!(
                "pci: intel e1000 at 00:{:02x}.{} bar0={:#010x} irq={} (pin INT{})",
                dev,
                fun,
                bar0 & 0xFFFF_FFF0,
                irq,
                (b'A' + pin.saturating_sub(1)) as char
            );
            return true;
        }
    }
    false
}
