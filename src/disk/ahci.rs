//! AHCI (SATA) host controller driver for GLM OS (v1.5 "storage edition").
//!
//! Talks to the QEMU q35 built-in ICH9 AHCI controller (PCI class 0106)
//! and to real AHCI HBAs that behave the same way:
//!
//!   1. PCI walk: match class 01/06, lift BAR5 (the ABAR MMIO window),
//!      enable MEM + BUS_MASTER decoding.
//!   2. HBA: ensure GHC.AE, walk the PI implemented-ports bitmap, pick the
//!      first port with a device present (PxSSTS.DET == 3) whose PxSIG says
//!      ATA (0x101) — ATAPI (cdrom) is skipped on purpose.
//!   3. Port: stop engine if running, publish the command list / FIS
//!      receive / command table areas, start it (FRE | ST).
//!   4. IDENTIFY DEVICE -> model + LBA48 capacity.
//!   5. READ/WRITE DMA EXT via command slot 0, one PRDT entry per chunk,
//!      completion detected by POLLING PxCI (no interrupts by design:
//!      the block layer holds a spinlock, and polled 32 KiB chunks on
//!      QEMU/hypervisors finish in microseconds; an MSI path can be added
//!      behind the same interface later without touching FAT32).
//!
//! DMA memory comes from the frame allocator (physically contiguous) and
//! is reached through the HHDM direct map. x86 bus mastering is cache
//! coherent on ICH9, so no explicit cache maintenance is needed.
//!
//! Locking: one Ahci instance is owned by one Fat32 mount under its
//! spinlock — every public call is serialized by the caller. No IRQs are
//! registered.

use alloc::boxed::Box;
use alloc::string::String;

use crate::disk::{BlockDev, SECTOR};
use crate::io::ports::{inl, outl};
use crate::klog;
use crate::mem::frames;
use crate::mem::paging::phys_to_virt;
use crate::mem::vmm::{map_kernel_page, NO_CACHE, NO_EXECUTE, PAGE, PRESENT, WRITABLE};

// --- PCI config space (same mechanics as net/pci.rs, kept local) -----------

const CFG_ADDR: u16 = 0xCF8;
const CFG_DATA: u16 = 0xCFC;

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

// --- HBA MMIO --------------------------------------------------------------

/// Fixed kernel VA for the 4 KiB ABAR window (free range between the
/// e1000 BAR0 window at 0xFFFF_FFFF_FE00_0000 and the LAPIC page).
const AHCI_VIRT: u64 = 0xFFFF_FFFF_FE80_0000;

const GHC: usize = 0x04; // global host control
const GHC_AE: u32 = 1 << 31; // AHCI enable
const PI: usize = 0x0C; // ports implemented

// per-port register offsets (port base = 0x100 + n * 0x80)
const P_CLB: usize = 0x00; // command list base
const P_CLBU: usize = 0x04;
const P_FB: usize = 0x08; // FIS receive base
const P_FBU: usize = 0x0C;
const P_IS: usize = 0x10; // interrupt status (write-1-clear)
const P_IE: usize = 0x14; // interrupt enable
const P_CMD: usize = 0x18; // command/status
const P_TFD: usize = 0x20; // task file data
const P_SIG: usize = 0x24; // signature
const P_SSTS: usize = 0x28; // status/control

// PxCMD bits
const CMD_ST: u32 = 1 << 0; // start
const CMD_FRE: u32 = 1 << 2; // FIS receive enable
const CMD_SUD: u32 = 1 << 4; // spin up device
const CMD_FR: u32 = 1 << 14; // FIS receive running
const CMD_CR: u32 = 1 << 15; // command list running
// PxSSTS
const SSTS_DET: u32 = 0xF;
const DET_PRESENT: u32 = 3;
// PxSIG values
const SIG_ATA: u32 = 0x0000_0101;
const SIG_ATAPI: u32 = 0xEB14_0101;
// PxIS error bits we check (write-1-clear them all after each command)
const IS_ERR_MASK: u32 = 0x7FFF_FFFF;

// H2D register FIS
const FIS_H2D: u8 = 0x27;
// ATA commands
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;

/// Sectors moved per single DMA command (32 KiB with one PRDT entry).
const CHUNK_SECTORS: u32 = 64;

/// Max polls for command completion (~5 s at QEMU MMIO speeds; a 32 KiB
/// chunk finishes in well under a thousand iterations, so this is really
/// an error guard).
const CMD_POLLS: u64 = 20_000_000;

pub struct Ahci {
    /// HBA port index.
    port: usize,
    /// Command list (32 x 32 B, 1 KiB-aligned page).
    clb_phys: u64,
    /// FIS receive area (page).
    fb_phys: u64,
    /// Command table slot 0 (CFIS 64 B + ATAPI + PRDT, page-aligned).
    ct_phys: u64,
    /// DMA scratch (CHUNK_SECTORS sectors, physically contiguous).
    scratch_phys: u64,
    sectors: u64,
    model: String,
}

// The struct only stores physical addresses and plain data; every pointer
// access goes through phys_to_virt() on use. Safe to move across threads.
unsafe impl Send for Ahci {}

fn hreg(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((AHCI_VIRT as usize + off) as *const u32) }
}

fn hset(off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((AHCI_VIRT as usize + off) as *mut u32, v) }
}

impl Ahci {
    fn port_base(&self) -> usize {
        AHCI_VIRT as usize + 0x100 + self.port * 0x80
    }

    fn preg(&self, off: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.port_base() + off) as *const u32) }
    }

    fn pset(&self, off: usize, v: u32) {
        unsafe { core::ptr::write_volatile((self.port_base() + off) as *mut u32, v) }
    }

    /// Build one DMA command in slot 0 and wait for completion (polling).
    fn issue(
        &self,
        ata_cmd: u8,
        write: bool,
        lba: u64,
        count: u16,
        buf_phys: u64,
        bytes: usize,
    ) -> Result<(), &'static str> {
        // --- command table: CFIS + one PRDT entry
        let ct = phys_to_virt(self.ct_phys) as *mut u8;
        unsafe { core::ptr::write_bytes(ct, 0, 128) };
        // Register FIS H2D layout (matches Linux ata_tf_to_fis — the
        // device/LBA-select byte is [7], NOT [4]:
        //   [2]=cmd [3]=feat lo [4..6]=LBA 23:0 [7]=device(0x40=LBA)
        //   [8]=feat hi [9..11]=LBA 47:24 [12..13]=count)
        unsafe {
            *ct.add(0) = FIS_H2D;
            *ct.add(1) = 0x80; // C bit: command FIS
            *ct.add(2) = ata_cmd;
            *ct.add(3) = 0; // features lo
            *ct.add(4) = (lba & 0xFF) as u8;
            *ct.add(5) = ((lba >> 8) & 0xFF) as u8;
            *ct.add(6) = ((lba >> 16) & 0xFF) as u8;
            *ct.add(7) = 0x40 | ((lba >> 24) & 0x0F) as u8; // device: LBA mode
            *ct.add(8) = 0; // features hi
            *ct.add(9) = ((lba >> 24) & 0xFF) as u8;
            *ct.add(10) = ((lba >> 32) & 0xFF) as u8;
            *ct.add(11) = ((lba >> 40) & 0xFF) as u8;
            *ct.add(12) = (count & 0xFF) as u8;
            *ct.add(13) = (count >> 8) as u8;
        }
        let prdt = unsafe { ct.add(0x80) } as *mut u32;
        unsafe {
            *prdt.add(0) = buf_phys as u32;
            *prdt.add(1) = (buf_phys >> 32) as u32;
            *prdt.add(2) = 0;
            *prdt.add(3) = ((bytes as u32) - 1) & 0x3F_FFFF; // DBC = bytes-1
        }

        // --- command list entry 0: CFL=5 dwords, W bit, PRDTL=1
        let cl = phys_to_virt(self.clb_phys) as *mut u32;
        unsafe {
            *cl.add(0) = 5 | (if write { 0x40 } else { 0 }) | (1 << 16);
            *cl.add(1) = 0;
            *cl.add(2) = self.ct_phys as u32;
            *cl.add(3) = (self.ct_phys >> 32) as u32;
        }

        // --- go
        self.pset(P_IS, IS_ERR_MASK); // clear stale status
        self.pset(P_CMD, self.preg(P_CMD) | CMD_ST);
        let ci_mask = 1u32;
        self.pset(0x38, ci_mask); // PxCI (0x100+0x38 slot0 in-port offset)

        // poll command issued: PxCI bit clears
        let mut ok = false;
        for _ in 0..CMD_POLLS {

            let ci = self.preg(0x38);
            if ci & ci_mask == 0 {
                ok = true;
                break;
            }
            core::hint::spin_loop();
        }
        let is = self.preg(P_IS);
        self.pset(P_IS, is & IS_ERR_MASK); // write-1-clear
        let tfd = self.preg(P_TFD);
        if !ok || is & (1 << 30) != 0 || tfd & 1 != 0 {
            return Err("ahci: command failed (timeout or TFES/ERR)");
        }
        Ok(())
    }

    /// Move `count` sectors via the 32 KiB scratch buffer. Exactly one of
    /// `read_into` / `write_from` is Some.
    fn transfer(
        &mut self,
        lba: u64,
        count: u32,
        read_into: Option<&mut [u8]>,
        write_from: Option<&[u8]>,
    ) -> Result<(), &'static str> {
        let write = write_from.is_some();
        let buf: &mut [u8] = if let Some(b) = read_into {
            b
        } else {
            // writes only read from the source slice; this scratch view is
            // never dereferenced in the write path
            &mut []
        };
        let total = if write {
            write_from.unwrap().len()
        } else {
            count as usize * SECTOR
        };
        if !write && buf.len() < total {
            return Err("ahci: buffer too small");
        }
        let mut done = 0usize;
        while done < total {
            let n = usize::min(CHUNK_SECTORS as usize * SECTOR, total - done);
            let secs = (n / SECTOR) as u16;
            let cur_lba = lba + (done / SECTOR) as u64;
            let cmd = if write {
                ATA_WRITE_DMA_EXT
            } else {
                ATA_READ_DMA_EXT
            };
            if write {
                let sv = phys_to_virt(self.scratch_phys) as *mut u8;
                unsafe {
                    core::ptr::copy_nonoverlapping(write_from.unwrap().as_ptr().add(done), sv, n);
                }
            }
            self.issue(cmd, write, cur_lba, secs, self.scratch_phys, n)?;
            if !write {
                let sv = phys_to_virt(self.scratch_phys) as *const u8;
                unsafe {
                    core::ptr::copy_nonoverlapping(sv, buf.as_mut_ptr().add(done), n);
                }
            }
            done += n;
        }
        Ok(())
    }

    /// IDENTIFY DEVICE: fill model + capacity.
    fn identify(&mut self) -> Result<(), &'static str> {
        self.issue(ATA_IDENTIFY, false, 0, 0, self.scratch_phys, SECTOR)?;
        let id = unsafe { core::slice::from_raw_parts(phys_to_virt(self.scratch_phys) as *const u16, 256) };
        // words 27..47: model string, big-endian byte pairs
        let mut model = String::new();
        for w in &id[27..47] {
            let b = w.to_be_bytes();
            for byte in b {
                if byte != 0 && byte != b' ' {
                    model.push(byte as char);
                }
            }
        }
        self.model = model;
        // words 100..104: LBA48 total user sectors (LE u32 pairs)
        let sectors = (id[100] as u64)
            | ((id[101] as u64) << 16)
            | ((id[102] as u64) << 32)
            | ((id[103] as u64) << 48);
        if sectors == 0 {
            return Err("ahci: device reports zero capacity");
        }
        self.sectors = sectors;
        Ok(())
    }
}

impl BlockDev for Ahci {
    fn read_sectors(
        &mut self,
        lba: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), &'static str> {
        if lba + count as u64 > self.sector_count() {
            return Err("ahci: read past end of disk");
        }
        self.transfer(lba, count, Some(out), None)
    }

    fn write_sectors(&mut self, lba: u64, count: u32, data: &[u8]) -> Result<(), &'static str> {
        if lba + count as u64 > self.sector_count() {
            return Err("ahci: write past end of disk");
        }
        if data.len() != count as usize * SECTOR {
            return Err("ahci: write buffer size mismatch");
        }
        self.transfer(lba, count, None, Some(data))
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn writable(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        alloc::format!(
            "sata disk, port {}, \"{}\", {} sectors ({} MiB), read-write",
            self.port,
            self.model,
            self.sectors,
            self.sectors * 512 / (1024 * 1024)
        )
    }
}

/// Walk bus 0 for an AHCI controller, bring up the first ATA disk port.
/// Returns the device as a boxed BlockDev (ownership moves to the mount).
pub fn probe() -> Option<Box<dyn BlockDev>> {
    // --- 1. find the controller
    // NOTE: the q35 ICH9 SATA controller sits at 00:1f.2 — function 2 of
    // the LPC bridge device — so ALL functions must be scanned, not just 0.
    let mut ctl = None;
    'outer: for dev in 0..32u8 {
        for fun in 0..8u8 {
            let id = cfg_read_dword(0, dev, fun, 0x00);
            let vendor = (id & 0xFFFF) as u16;
            if vendor == 0xFFFF {
                continue;
            }
            let class_rev = cfg_read_dword(0, dev, fun, 0x08);
            let class = (class_rev >> 24) as u8;
            let subclass = (class_rev >> 16) as u8;
            if class == 0x01 && subclass == 0x06 {
                ctl = Some((dev, fun));
                klog!(
                    "pci: 00:{:02x}.{} sata controller (ahci) {:#06x}:{:#06x}",
                    dev,
                    fun,
                    vendor,
                    (id >> 16) as u16
                );
                break 'outer;
            }
        }
    }
    let (dev, fun) = ctl?;

    // --- 2. BAR5 + command register
    let bar5 = cfg_read_dword(0, dev, fun, 0x24);
    if bar5 & 1 != 0 {
        klog!("ahci: bar5 is i/o ports - unexpected, giving up");
        return None;
    }
    let abar = (bar5 & 0xFFFF_FFF0) as u64;
    let cmd = cfg_read_dword(0, dev, fun, 0x04) & 0xFFFF;
    cfg_write_dword(0, dev, fun, 0x04, cmd | 0x6); // MEM | BUS_MASTER

    // --- 3. map the ABAR page (no-cache, like every MMIO window here)
    for i in 0..1usize {
        let va = AHCI_VIRT + (i as u64) * PAGE;
        if map_kernel_page(va, abar + (i as u64) * PAGE, PRESENT | WRITABLE | NO_CACHE | NO_EXECUTE)
            .is_err()
        {
            klog!("ahci: failed to map abar page");
            return None;
        }
    }

    // --- 4. enable AHCI mode
    if hreg(GHC) & GHC_AE == 0 {
        hset(GHC, GHC_AE);
    }

    // --- 5. first implemented port with an ATA disk behind it
    let pi = hreg(PI);
    let mut found = None;
    for p in 0..6usize {
        if pi & (1 << p) == 0 {
            continue;
        }
        let tmp = Ahci {
            port: p,
            clb_phys: 0,
            fb_phys: 0,
            ct_phys: 0,
            scratch_phys: 0,
            sectors: 0,
            model: String::new(),
        };
        let ssts = tmp.preg(P_SSTS);
        if ssts & SSTS_DET != DET_PRESENT {
            continue;
        }
        let sig = tmp.preg(P_SIG);
        if sig == SIG_ATAPI {
            klog!("ahci: port {} is atapi (cdrom), skipped", p);
            continue;
        }
        found = Some(p);
        break;
    }
    let port = found?;

    // --- 6. DMA areas (all physically contiguous, page-aligned)
    let clb_phys = frames::alloc_contig(1)?; // 1 KiB needed, page is fine
    let fb_phys = frames::alloc_contig(1)?;
    let ct_phys = frames::alloc_contig(1)?;
    let scratch_phys = frames::alloc_contig(CHUNK_SECTORS as usize * SECTOR / (PAGE as usize))?;

    let mut d = Ahci {
        port,
        clb_phys,
        fb_phys,
        ct_phys,
        scratch_phys,
        sectors: 0,
        model: String::new(),
    };

    // --- 7. stop engine, publish areas, start engine
    let mut pc = d.preg(P_CMD);
    if pc & CMD_ST != 0 {
        pc &= !CMD_ST;
        d.pset(P_CMD, pc);
        let mut guard = 0;
        while d.preg(P_CMD) & CMD_CR != 0 && guard < 1_000_000 {
            guard += 1;
            core::hint::spin_loop();
        }
    }
    if d.preg(P_CMD) & CMD_FRE != 0 {
        d.pset(P_CMD, d.preg(P_CMD) & !CMD_FRE);
        let mut guard = 0;
        while d.preg(P_CMD) & CMD_FR != 0 && guard < 1_000_000 {
            guard += 1;
            core::hint::spin_loop();
        }
    }
    d.pset(P_IE, 0); // polling driver: no per-port interrupts
    d.pset(P_CLB, clb_phys as u32);
    d.pset(P_CLBU, (clb_phys >> 32) as u32);
    d.pset(P_FB, fb_phys as u32);
    d.pset(P_FBU, (fb_phys >> 32) as u32);
    // zero the command list + FIS area once
    unsafe {
        core::ptr::write_bytes(phys_to_virt(clb_phys) as *mut u8, 0, 1024);
        core::ptr::write_bytes(phys_to_virt(fb_phys) as *mut u8, 0, 256);
    }
    d.pset(P_CMD, d.preg(P_CMD) | CMD_SUD | CMD_FRE | CMD_ST);

    // --- 8. identify
    if let Err(e) = d.identify() {
        klog!("ahci: identify failed: {}", e);
        return None;
    }
    klog!(
        "ahci: sata disk on port {}: \"{}\", {} MiB",
        port,
        d.model,
        d.sectors * 512 / (1024 * 1024)
    );
    Some(Box::new(d))
}
