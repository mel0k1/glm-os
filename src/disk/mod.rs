//! Block device layer (v1.5): the storage abstraction under the filesystem.
//!
//! A `BlockDev` is a fixed-granularity (512-byte sector) store that the
//! FAT32 driver reads and writes through. Two implementations exist:
//!   - `RamBlock` (here): the Limine ramdisk module, read-only;
//!   - `ahci::Ahci` (disk/ahci.rs): a real SATA disk behind the AHCI HBA.

use alloc::string::String;

/// Sector size every implementation must speak (FAT32 BPB is checked to
/// match; see fs/fat32.rs).
pub const SECTOR: usize = 512;

pub trait BlockDev: Send {
    /// Read `count` consecutive 512-byte sectors starting at `lba` into
    /// `out` (must be exactly count * 512 bytes).
    fn read_sectors(
        &mut self,
        lba: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), &'static str>;

    /// Write `count` consecutive sectors from `data`. May be refused
    /// (read-only media).
    fn write_sectors(&mut self, lba: u64, count: u32, data: &[u8]) -> Result<(), &'static str>;

    /// Total addressable 512-byte sectors.
    fn sector_count(&self) -> u64;

    /// Whether write_sectors can ever succeed.
    fn writable(&self) -> bool;

    /// Human-readable device line for `dstat`.
    fn describe(&self) -> String;
}

/// The ramdisk as a block device: plain slice reads, no writes.
pub struct RamBlock {
    pub data: &'static [u8],
}

impl BlockDev for RamBlock {
    fn read_sectors(
        &mut self,
        lba: u64,
        count: u32,
        out: &mut [u8],
    ) -> Result<(), &'static str> {
        let start = (lba as usize) * SECTOR;
        let end = start + count as usize * SECTOR;
        if end > self.data.len() || out.len() != count as usize * SECTOR {
            return Err("ramdisk: read out of range");
        }
        out.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn write_sectors(&mut self, _lba: u64, _count: u32, _data: &[u8]) -> Result<(), &'static str> {
        Err("ramdisk is read-only")
    }

    fn sector_count(&self) -> u64 {
        (self.data.len() / SECTOR) as u64
    }

    fn writable(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        alloc::format!(
            "ramdisk (Limine module), {} KiB, read-only",
            self.data.len() / 1024
        )
    }
}

pub mod ahci;
