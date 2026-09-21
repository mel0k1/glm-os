//! FAT32 read-only driver over a ramdisk image loaded as a Limine module.
//!
//! Supports: BPB parsing, cluster chains, directory traversal (incl. LFN
//! long names), path lookup with subdirectories, and file reads.

use alloc::{string::String, vec::Vec};

pub struct Fat32 {
    data: &'static [u8],
    bytes_per_sector: usize,
    sectors_per_cluster: usize,
    fat_begin: usize,   // in sectors
    data_begin: usize,  // in sectors
    root_cluster: u32,
}

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub first_cluster: u32,
    pub size: u32,
    pub is_dir: bool,
}

impl Fat32 {
    /// Parse the ramdisk image bytes (must be a FAT32 filesystem).
    pub fn new(data: &'static [u8]) -> Result<Self, &'static str> {
        if data.len() < 512 {
            return Err("ramdisk too small");
        }
        let rd = |off: usize, n: usize| -> usize {
            let mut v = 0usize;
            for i in 0..n {
                v |= (data[off + i] as usize) << (8 * i);
            }
            v
        };
        let bytes_per_sector = rd(0x0B, 2);
        let sectors_per_cluster = data[0x0D] as usize;
        let reserved = rd(0x0E, 2);
        let num_fats = data[0x10] as usize;
        let fat_size = rd(0x24, 4); // FAT32: sectors per FAT (32-bit)
        let root_cluster = rd(0x2C, 4) as u32;

        if bytes_per_sector != 512 {
            return Err("unsupported sector size (want 512)");
        }
        if sectors_per_cluster == 0 || num_fats == 0 || fat_size == 0 {
            return Err("bad BPB");
        }
        Ok(Self {
            data,
            bytes_per_sector,
            sectors_per_cluster,
            fat_begin: reserved,
            data_begin: reserved + num_fats * fat_size,
            root_cluster,
        })
    }

    fn cluster_offset(&self, cluster: u32) -> usize {
        let c = cluster as usize;
        (self.data_begin + (c - 2) * self.sectors_per_cluster) * self.bytes_per_sector
    }

    fn next_cluster(&self, cluster: u32) -> Option<u32> {
        let fat_off = self.fat_begin * self.bytes_per_sector + (cluster as usize) * 4;
        if fat_off + 4 > self.data.len() {
            return None;
        }
        let v = u32::from_le_bytes([
            self.data[fat_off],
            self.data[fat_off + 1],
            self.data[fat_off + 2],
            self.data[fat_off + 3],
        ]);
        if v >= 0x0FFF_FFF8 {
            None // end of chain
        } else if v == 0x0FFF_FFF7 {
            None // bad cluster
        } else {
            Some(v)
        }
    }

    fn cluster_size(&self) -> usize {
        self.sectors_per_cluster * self.bytes_per_sector
    }

    fn read_chain(&self, start: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = start;
        let mut guard = 0;
        while c >= 2 && guard < 1_000_000 {
            guard += 1;
            let off = self.cluster_offset(c);
            let cs = self.cluster_size();
            if off + cs > self.data.len() {
                break;
            }
            out.extend_from_slice(&self.data[off..off + cs]);
            match self.next_cluster(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        out
    }

    /// Read a directory (cluster chain of 32-byte entries) with LFN support.
    fn read_dir(&self, cluster: u32) -> Vec<DirEntry> {
        let raw = self.read_chain(cluster);
        let mut out = Vec::new();
        let mut lfn_parts: Vec<String> = Vec::new();

        let mut i = 0;
        while i + 32 <= raw.len() {
            let e = &raw[i..i + 32];
            i += 32;
            if e[0] == 0x00 {
                break; // end of directory
            }
            if e[0] == 0xE5 {
                lfn_parts.clear();
                continue; // deleted
            }
            let attr = e[11];
            if attr & 0x3F == 0x0F {
                // long file name entry
                let mut part = String::new();
                for idx in [1usize, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30] {
                    let lo = e[idx] as u32;
                    let hi = e[idx + 1] as u32;
                    let ch = lo | (hi << 8);
                    if ch == 0 || ch == 0xFFFF {
                        break;
                    }
                    if let Some(c) = char::from_u32(ch) {
                        if c.is_ascii() {
                            part.push(c);
                        }
                    }
                }
                lfn_parts.push(part);
                continue;
            }
            if attr & 0x08 != 0 {
                lfn_parts.clear();
                continue; // volume label
            }

            // 8.3 name
            let base = &e[0..8];
            let ext = &e[8..11];
            let mut name = String::new();
            for &b in base {
                if b == b' ' {
                    break;
                }
                name.push(b as char);
            }
            let ext_str: String = ext.iter().take_while(|&&b| b != b' ').map(|&b| b as char).collect();
            if !ext_str.is_empty() {
                name.push('.');
                name.push_str(&ext_str);
            }

            if !lfn_parts.is_empty() {
                // LFN parts are stored in reverse order
                lfn_parts.reverse();
                name = lfn_parts.concat();
                lfn_parts.clear();
            }

            let cl_hi = u16::from_le_bytes([e[20], e[21]]) as u32;
            let cl_lo = u16::from_le_bytes([e[26], e[27]]) as u32;
            let size = u32::from_le_bytes([e[28], e[29], e[30], e[31]]);

            out.push(DirEntry {
                name,
                first_cluster: (cl_hi << 16) | cl_lo,
                size,
                is_dir: attr & 0x10 != 0,
            });
        }
        out
    }

    /// Resolve a path like "/README.TXT" or "/DOCS/GLM.TXT" to an entry.
    pub fn lookup(&self, path: &str) -> Option<DirEntry> {
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            return Some(DirEntry {
                name: "/".into(),
                first_cluster: self.root_cluster,
                size: 0,
                is_dir: true,
            });
        }
        let mut current = DirEntry {
            name: "/".into(),
            first_cluster: self.root_cluster,
            size: 0,
            is_dir: true,
        };
        for component in path.split('/') {
            if component.is_empty() {
                continue;
            }
            if !current.is_dir {
                return None;
            }
            let entries = self.read_dir(current.first_cluster);
            let wanted = component.to_ascii_uppercase();
            current = entries
                .into_iter()
                .find(|e| e.name.to_ascii_uppercase() == wanted)?;
        }
        Some(current)
    }

    /// List a directory path.
    pub fn ls(&self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        let entry = self.lookup(path).ok_or("no such file or directory")?;
        if !entry.is_dir {
            return Err("not a directory");
        }
        Ok(self.read_dir(entry.first_cluster))
    }

    /// Read a file (contents trimmed to the reported size).
    pub fn cat(&self, path: &str) -> Result<Vec<u8>, &'static str> {
        let entry = self.lookup(path).ok_or("no such file or directory")?;
        if entry.is_dir {
            return Err("is a directory");
        }
        let mut data = self.read_chain(entry.first_cluster);
        data.truncate(entry.size as usize);
        Ok(data)
    }

    /// Count files recursively (root only for stats display).
    pub fn root_file_count(&self) -> usize {
        self.read_dir(self.root_cluster)
            .iter()
            .filter(|e| !e.is_dir)
            .count()
    }

    pub fn total_bytes(&self) -> usize {
        self.data.len()
    }
}

// ---------------------------------------------------------------------------
// Global ramdisk instance
// ---------------------------------------------------------------------------




use crate::sync::Spinlock;

pub static FAT: Spinlock<Option<Fat32>> = Spinlock::new(None);

/// Mount the first Limine module as a FAT32 ramdisk. Returns file count.
pub fn mount_from_limine() -> Result<usize, &'static str> {
    let resp = crate::limine_reqs::MODULE_REQUEST
        .get_response()
        .ok_or("bootloader provided no modules")?;
    let file = resp.modules().first().ok_or("no ramdisk module")?;
    let addr = file.addr() as usize;
    let size = file.size() as usize;
    if size == 0 {
        return Err("ramdisk module is empty");
    }
    let slice: &'static [u8] = unsafe { core::slice::from_raw_parts(addr as *const u8, size) };
    let fs = Fat32::new(slice)?;
    let files = fs.root_file_count();
    let total = fs.total_bytes() / (1024 * 1024);
    *FAT.lock() = Some(fs);
    crate::klog!("fat32: ramdisk {} MiB at {:#x}, {} files in root", total, addr, files);
    Ok(files)
}
