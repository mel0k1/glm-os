//! FAT32 driver (v1.5): block-device backed, READ-WRITE.
//!
//! v1.0..v1.4 this was a read-only driver over the ramdisk image bytes.
//! v1.5 moves it onto the `disk::BlockDev` abstraction: the ramdisk is
//! just a read-only block device now, and a real AHCI SATA disk is a
//! writable one. Everything above (shell ls/cat/run, ELF loader, GUI
//! monitor) keeps working through the same public API; new write
//! operations (write_file / delete) exist only when the media is
//! writable.
//!
//! Layout notes:
//!   - BPB parsed from sector 0; only 512-byte sectors supported.
//!   - The whole FAT is cached in RAM as u32 entries at mount time
//!     (capped; ~128 KiB for a 64 MiB disk). All chain walks hit the
//!     cache; every mutation rewrites the affected sector in ALL FAT
//!     copies (dual/mirror FATs kept consistent).
//!   - Directory reads keep LFN support; writes create classic 8.3
//!     short-name entries (LFN write support is deliberately out of
//!     scope — the shell workloads here don't need it).
//!   - Writes/creates/deletes are root-directory only for v1.5 (the dir
//!     chain can still be extended, but nested mkdir is future work).

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::disk::{ahci, BlockDev, RamBlock, SECTOR};
use crate::sync::Spinlock;

const EOC: u32 = 0x0FFF_FFFF;
const EOC_MIN: u32 = 0x0FFF_FFF8;
const BAD: u32 = 0x0FFF_FFF7;
/// FAT cache hard cap: 1 MiB entries (4 MiB of RAM) — plenty for our disks.
const FAT_CACHE_MAX: usize = 1 << 20;

pub struct Fat32 {
    dev: Box<dyn BlockDev>,
    sectors_per_cluster: usize,
    fat_begin: usize,   // in sectors
    num_fats: usize,
    fat_size: usize,    // sectors per FAT copy
    data_begin: usize,  // in sectors
    root_cluster: u32,
    /// Whole-FAT cache; index = cluster number.
    fat: Vec<u32>,
}

#[derive(Clone)]
pub struct DirEntry {
    pub name: String,
    pub first_cluster: u32,
    pub size: u32,
    pub is_dir: bool,
}

impl Fat32 {
    /// Mount a block device as FAT32: parse BPB, cache the FAT.
    pub fn new(mut dev: Box<dyn BlockDev>) -> Result<Self, &'static str> {
        let mut bpb = [0u8; SECTOR];
        dev.read_sectors(0, 1, &mut bpb)?;
        let rd = |off: usize, n: usize| -> usize {
            let mut v = 0usize;
            for i in 0..n {
                v |= (bpb[off + i] as usize) << (8 * i);
            }
            v
        };
        let bytes_per_sector = rd(0x0B, 2);
        let sectors_per_cluster = bpb[0x0D] as usize;
        let reserved = rd(0x0E, 2);
        let num_fats = bpb[0x10] as usize;
        let fat_size = rd(0x24, 4); // FAT32: sectors per FAT (32-bit)
        let root_cluster = rd(0x2C, 4) as u32;

        if bytes_per_sector != SECTOR {
            return Err("unsupported sector size (want 512)");
        }
        if sectors_per_cluster == 0 || num_fats == 0 || fat_size == 0 {
            return Err("bad BPB");
        }
        let entries = fat_size * SECTOR / 4;
        if entries > FAT_CACHE_MAX {
            return Err("FAT too large to cache");
        }
        // read the whole first FAT into the cache
        let mut fat_bytes = vec![0u8; entries * 4];
        // in chunks of 128 sectors (64 KiB) to keep buffers bounded
        let mut off = 0usize;
        while off < entries * 4 {
            let n = usize::min(128 * SECTOR, entries * 4 - off);
            dev.read_sectors(
                reserved as u64 + (off / SECTOR) as u64,
                (n / SECTOR) as u32,
                &mut fat_bytes[off..off + n],
            )?;
            off += n;
        }
        let fat = fat_bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();

        Ok(Self {
            dev,
            sectors_per_cluster,
            fat_begin: reserved,
            num_fats,
            fat_size,
            data_begin: reserved + num_fats * fat_size,
            root_cluster,
            fat,
        })
    }

    fn cluster_size(&self) -> usize {
        self.sectors_per_cluster * SECTOR
    }

    fn cluster_lba(&self, c: u32) -> Result<u64, &'static str> {
        if c < 2 {
            return Err("bad cluster number");
        }
        Ok((self.data_begin + (c as usize - 2) * self.sectors_per_cluster) as u64)
    }

    /// Next cluster from the cached FAT.
    fn next_cluster(&self, cluster: u32) -> Option<u32> {
        let v = *self.fat.get(cluster as usize)?;
        if v >= EOC_MIN {
            None // end of chain
        } else if v == BAD || v < 2 {
            None // bad or free
        } else {
            Some(v)
        }
    }

    /// Read a full cluster chain into a Vec.
    fn read_chain(&mut self, start: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut c = start;
        let mut guard = 0;
        let cs = self.cluster_size();
        let mut buf = vec![0u8; cs];
        while c >= 2 && guard < 1_000_000 {
            guard += 1;
            let Ok(lba) = self.cluster_lba(c) else { break };
            if self.dev.read_sectors(lba, self.sectors_per_cluster as u32, &mut buf).is_err() {
                break;
            }
            out.extend_from_slice(&buf);
            match self.next_cluster(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        out
    }

    /// Raw directory slots (32 bytes each) of a cluster chain.
    fn dir_slots(&mut self, cluster: u32) -> Vec<[u8; 32]> {
        let raw = self.read_chain(cluster);
        let mut slots = Vec::new();
        for chunk in raw.chunks_exact(32) {
            let mut s = [0u8; 32];
            s.copy_from_slice(chunk);
            slots.push(s);
        }
        slots
    }

    /// LBAs of every cluster in a directory chain (for slot patching).
    fn dir_chain_lbas(&mut self, cluster: u32) -> Vec<u64> {
        let mut out = Vec::new();
        let mut c = cluster;
        let mut guard = 0;
        while c >= 2 && guard < 1_000_000 {
            guard += 1;
            let Ok(lba) = self.cluster_lba(c) else { break };
            out.push(lba);
            match self.next_cluster(c) {
                Some(n) => c = n,
                None => break,
            }
        }
        out
    }

    /// Parse directory slots into entries (LFN-aware). Same rules as the
    /// v1.0 read-only driver.
    fn parse_dir(slots: &[[u8; 32]]) -> Vec<DirEntry> {
        let mut out = Vec::new();
        let mut lfn_parts: Vec<String> = Vec::new();

        for e in slots {
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
            let ext_str: String = ext
                .iter()
                .take_while(|&&b| b != b' ')
                .map(|&b| b as char)
                .collect();
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
    pub fn lookup(&mut self, path: &str) -> Option<DirEntry> {
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
            let entries = Self::parse_dir(&self.dir_slots(current.first_cluster));
            let wanted = component.to_ascii_uppercase();
            current = entries
                .into_iter()
                .find(|e| e.name.to_ascii_uppercase() == wanted)?;
        }
        Some(current)
    }

    /// Does this path exist (file or directory)?
    pub fn exists(&mut self, path: &str) -> bool {
        self.lookup(path).is_some()
    }

    /// List a directory path.
    pub fn ls(&mut self, path: &str) -> Result<Vec<DirEntry>, &'static str> {
        let entry = self.lookup(path).ok_or("no such file or directory")?;
        if !entry.is_dir {
            return Err("not a directory");
        }
        Ok(Self::parse_dir(&self.dir_slots(entry.first_cluster)))
    }

    /// Read a file (contents trimmed to the reported size).
    pub fn cat(&mut self, path: &str) -> Result<Vec<u8>, &'static str> {
        let entry = self.lookup(path).ok_or("no such file or directory")?;
        if entry.is_dir {
            return Err("is a directory");
        }
        let mut data = self.read_chain(entry.first_cluster);
        data.truncate(entry.size as usize);
        Ok(data)
    }

    /// Count files in root (stats display).
    pub fn root_file_count(&mut self) -> usize {
        Self::parse_dir(&self.dir_slots(self.root_cluster))
            .iter()
            .filter(|e| !e.is_dir)
            .count()
    }

    pub fn total_bytes(&self) -> usize {
        (self.dev.sector_count() as usize) * SECTOR
    }

    pub fn is_writable(&self) -> bool {
        self.dev.writable()
    }

    pub fn device_info(&self) -> String {
        self.dev.describe()
    }

    // -------------------------------------------------------------------
    // v1.5: write support
    // -------------------------------------------------------------------

    /// Rewrite one FAT sector in the cache AND in every FAT copy on disk.
    fn fat_set(&mut self, cluster: u32, val: u32) -> Result<(), &'static str> {
        let idx = cluster as usize;
        if idx >= self.fat.len() {
            return Err("fat index out of range");
        }
        self.fat[idx] = val;
        let ents_per_sec = SECTOR / 4;
        let sec_off = idx / ents_per_sec; // sector within one FAT copy
        let base = sec_off * ents_per_sec;
        let mut sec = [0u8; SECTOR];
        for k in 0..ents_per_sec {
            let v = if base + k < self.fat.len() { self.fat[base + k] } else { 0 };
            sec[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        for f in 0..self.num_fats {
            let lba = (self.fat_begin + f * self.fat_size + sec_off) as u64;
            self.dev.write_sectors(lba, 1, &sec)?;
        }
        Ok(())
    }

    /// Allocate one free cluster (marked EOC in cache + on disk).
    fn alloc_cluster(&mut self) -> Option<u32> {
        for idx in 2..self.fat.len() {
            if self.fat[idx] == 0 {
                self.fat[idx] = EOC;
                if self.fat_set(idx as u32, EOC).is_err() {
                    self.fat[idx] = 0;
                    return None;
                }
                return Some(idx as u32);
            }
        }
        None
    }

    /// Free a cluster chain (marks every member 0).
    fn free_chain(&mut self, start: u32) -> Result<(), &'static str> {
        let mut c = start;
        let mut guard = 0;
        while c >= 2 && (c as usize) < self.fat.len() && guard < 1_000_000 {
            guard += 1;
            let next = self.fat[c as usize];
            self.fat[c as usize] = 0;
            self.fat_set(c, 0)?;
            if next >= EOC_MIN || next < 2 {
                break;
            }
            c = next;
        }
        Ok(())
    }

    /// Allocate a fresh chain of `n` clusters and fill it with `data`
    /// (zero-padded to the last sector). Returns the first cluster
    /// (0 for empty data).
    fn write_chain(&mut self, data: &[u8]) -> Result<u32, &'static str> {
        let cs = self.cluster_size();
        let need = data.len().div_ceil(cs);
        if need == 0 {
            return Ok(0);
        }
        let mut chain: Vec<u32> = Vec::with_capacity(need);
        for i in 0..need {
            match self.alloc_cluster() {
                Some(c) => {
                    if i > 0 {
                        self.fat_set(chain[i - 1], c)?;
                    }
                    chain.push(c);
                }
                None => {
                    // rollback: free what we took (each is a 1-cluster chain)
                    for c in chain.drain(..) {
                        let _ = self.free_chain(c);
                    }
                    return Err("disk full");
                }
            }
        }
        // write content, zero-padding every partial sector
        for (i, c) in chain.iter().enumerate() {
            let lba = self.cluster_lba(*c)?;
            for s in 0..self.sectors_per_cluster {
                let off = i * cs + s * SECTOR;
                let mut sec = [0u8; SECTOR];
                if off < data.len() {
                    let n = usize::min(SECTOR, data.len() - off);
                    sec[..n].copy_from_slice(&data[off..off + n]);
                }
                self.dev.write_sectors(lba + s as u64, 1, &sec)?;
            }
        }
        Ok(chain[0])
    }

    /// Patch one 32-byte directory slot on disk.
    fn patch_slot(
        &mut self,
        lbas: &[u64],
        idx: usize,
        entry: &[u8; 32],
    ) -> Result<(), &'static str> {
        let off = idx * 32;
        let sec_i = off / SECTOR;
        let in_off = off % SECTOR;
        if sec_i >= lbas.len() {
            return Err("dir slot beyond chain");
        }
        let mut sec = [0u8; SECTOR];
        self.dev.read_sectors(lbas[sec_i], 1, &mut sec)?;
        sec[in_off..in_off + 32].copy_from_slice(entry);
        self.dev.write_sectors(lbas[sec_i], 1, &sec)
    }

    /// Build a classic 8.3 dirent (attr = archive, fixed sane timestamp).
    fn make_dirent(base: &str, ext: &str, first: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        for (i, b) in base.bytes().enumerate() {
            e[i] = b;
        }
        for i in base.len()..8 {
            e[i] = b' ';
        }
        for (i, b) in ext.bytes().enumerate() {
            e[8 + i] = b;
        }
        for i in 8 + ext.len()..11 {
            e[i] = b' ';
        }
        e[11] = 0x20; // archive
        // create time 12:00:00, date 2026-01-01 (fixed, valid DOS format)
        let tsec10 = 0u16;
        let time = (12u16 << 11) | (0 << 5) | 0;
        let date = ((2026 - 1980) as u16) << 9 | (1 << 5) | 1;
        e[14..16].copy_from_slice(&time.to_le_bytes());
        e[16..18].copy_from_slice(&date.to_le_bytes());
        e[18..20].copy_from_slice(&date.to_le_bytes()); // last access
        e[22..24].copy_from_slice(&time.to_le_bytes()); // last write
        e[24..26].copy_from_slice(&tsec10.to_le_bytes());
        e[20..22].copy_from_slice(&((first >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&((first & 0xFFFF) as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    /// 8.3 short name from a filename: upper-case, sanitised, base <= 8,
    /// ext <= 3.
    fn to_83(name: &str) -> (String, String) {
        let name = name.trim_start_matches('/');
        let (b, e) = match name.rfind('.') {
            Some(0) | None => (name, ""),
            Some(i) => (&name[..i], &name[i + 1..]),
        };
        let fix = |s: &str, max: usize| -> String {
            let mut out = String::new();
            for ch in s.to_ascii_uppercase().chars() {
                let ok = ch.is_ascii_alphanumeric()
                    || matches!(ch, '!' | '#' | '$' | '%' | '&' | '\'' | '(' | ')' | '-'
                        | '@' | '^' | '_' | '`' | '{' | '}' | '~');
                out.push(if ok { ch } else { '_' });
                if out.len() == max {
                    break;
                }
            }
            out
        };
        (fix(b, 8), fix(e, 3))
    }

    /// Does `base.ext` collide with any short name in `slots`?
    fn short_name_taken(slots: &[[u8; 32]], base: &str, ext: &str) -> bool {
        for e in slots {
            if e[0] == 0x00 || e[0] == 0xE5 {
                continue;
            }
            if e[11] & 0x3F == 0x0F {
                continue; // LFN
            }
            let eb = core::str::from_utf8(&e[0..8]).unwrap_or("");
            let ee = core::str::from_utf8(&e[8..11]).unwrap_or("");
            let eb = eb.trim_end_matches(' ');
            let ee = ee.trim_end_matches(' ');
            if eb.eq_ignore_ascii_case(base) && ee.eq_ignore_ascii_case(ext) {
                return true;
            }
        }
        false
    }

    /// Create or replace a file in the ROOT directory. `data.len()` may be
    /// anything; empty files get a 0-size entry with no cluster chain.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), &'static str> {
        if !self.dev.writable() {
            return Err("media is read-only");
        }
        let p = path.trim_start_matches('/');
        if p.is_empty() || p.contains('/') {
            return Err("write: only root-directory paths are supported");
        }

        let root = self.root_cluster;
        let mut slots = self.dir_slots(root);
        let mut lbas = self.dir_chain_lbas(root);

        // 1. existing entry with this (display) name? -> replace in place
        let wanted = p.to_ascii_uppercase();
        let mut existing: Option<usize> = None;
        for (i, e) in slots.iter().enumerate() {
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xE5 || e[11] & 0x3F == 0x0F || e[11] & 0x08 != 0 {
                continue;
            }
            // display name of this slot (short name only; our files are 8.3)
            let eb = core::str::from_utf8(&e[0..8]).unwrap_or("").trim_end_matches(' ');
            let ee = core::str::from_utf8(&e[8..11]).unwrap_or("").trim_end_matches(' ');
            let full = if ee.is_empty() {
                alloc::format!("{}", eb)
            } else {
                alloc::format!("{}.{}", eb, ee)
            };
            if full.to_ascii_uppercase() == wanted {
                existing = Some(i);
                break;
            }
        }

        let first = self.write_chain(data)?;

        if let Some(idx) = existing {
            // free the old chain, then patch cluster+size
            let old = {
                let e = &slots[idx];
                let cl_hi = u16::from_le_bytes([e[20], e[21]]) as u32;
                let cl_lo = u16::from_le_bytes([e[26], e[27]]) as u32;
                (cl_hi << 16) | cl_lo
            };
            if old >= 2 {
                self.free_chain(old)?;
            }
            let (base, ext) = {
                let e = &slots[idx];
                let eb = core::str::from_utf8(&e[0..8]).unwrap_or("").trim_end_matches(' ');
                let ee = core::str::from_utf8(&e[8..11]).unwrap_or("").trim_end_matches(' ');
                (eb.to_string(), ee.to_string())
            };
            let ent = Self::make_dirent(&base, &ext, first, data.len() as u32);
            self.patch_slot(&lbas, idx, &ent)?;
            klog_write_ok(p, data.len());
            return Ok(());
        }

        // 2. new entry: pick a collision-free 8.3 name
        let (mut base, ext) = Self::to_83(p);
        if Self::short_name_taken(&slots, &base, &ext) {
            let stem_len = usize::min(base.len(), 6);
            let stem = base[..stem_len].to_string(); // ascii-safe (to_83 sanitised)
            base = format!("{}~1", stem);
            let mut n = 1;
            while Self::short_name_taken(&slots, &base, &ext) && n < 10 {
                base = format!("{}~{}", stem, n);
                n += 1;
            }
            if Self::short_name_taken(&slots, &base, &ext) {
                if first >= 2 {
                    self.free_chain(first)?;
                }
                return Err("write: name collision, no free 8.3 name");
            }
        }

        // 3. find a free slot: deleted (0xE5) or end-of-dir (0x00)
        let mut slot_idx: Option<usize> = None;
        for (i, e) in slots.iter().enumerate() {
            if e[0] == 0xE5 {
                slot_idx = Some(i);
                break;
            }
            if e[0] == 0x00 {
                slot_idx = Some(i);
                break;
            }
        }

        let idx = match slot_idx {
            Some(i) => i,
            None => {
                // extend the directory chain with one fresh zeroed cluster
                let newc = self.alloc_cluster().ok_or("disk full")?;
                let nlba = self.cluster_lba(newc)?;
                let zero = [0u8; SECTOR];
                for s in 0..self.sectors_per_cluster {
                    self.dev.write_sectors(nlba + s as u64, 1, &zero)?;
                }
                let tail = *lbas.last().ok_or("bad root chain")?;
                let tail_cluster = self.cluster_of_lba(tail);
                self.fat_set(tail_cluster, newc)?;
                lbas.push(nlba);
                slots.len() // first slot of the fresh cluster
            }
        };
        if idx >= lbas.len() * (SECTOR / 32) {
            return Err("dir slot beyond chain");
        }

        let ent = Self::make_dirent(&base, &ext, first, data.len() as u32);
        self.patch_slot(&lbas, idx, &ent)?;
        klog_write_ok(p, data.len());
        Ok(())
    }

    /// Map a data-area LBA back to its cluster (for chain linking).
    fn cluster_of_lba(&self, lba: u64) -> u32 {
        (((lba as usize - self.data_begin) / self.sectors_per_cluster) + 2) as u32
    }

    /// Delete a root-directory file: free its chain, mark its slots 0xE5
    /// (including any LFN group in front of it).
    pub fn delete(&mut self, path: &str) -> Result<(), &'static str> {
        if !self.dev.writable() {
            return Err("media is read-only");
        }
        let p = path.trim_start_matches('/');
        if p.is_empty() || p.contains('/') {
            return Err("delete: only root-directory paths are supported");
        }
        let root = self.root_cluster;
        let slots = self.dir_slots(root);
        let lbas = self.dir_chain_lbas(root);

        let wanted = p.to_ascii_uppercase();
        // walk with LFN group tracking
        let mut group_start: Option<usize> = None;
        for (i, e) in slots.iter().enumerate() {
            if e[0] == 0x00 {
                break;
            }
            if e[0] == 0xE5 {
                group_start = None;
                continue;
            }
            if e[11] & 0x3F == 0x0F {
                if group_start.is_none() {
                    group_start = Some(i);
                }
                continue;
            }
            if e[11] & 0x08 != 0 {
                group_start = None;
                continue; // volume label
            }
            // name match?
            let eb = core::str::from_utf8(&e[0..8]).unwrap_or("").trim_end_matches(' ');
            let ee = core::str::from_utf8(&e[8..11]).unwrap_or("").trim_end_matches(' ');
            let full = if ee.is_empty() {
                alloc::format!("{}", eb)
            } else {
                alloc::format!("{}.{}", eb, ee)
            };
            if full.to_ascii_uppercase() != wanted {
                group_start = None;
                continue;
            }
            // matched: free chain, tombstone the group
            let cl_hi = u16::from_le_bytes([e[20], e[21]]) as u32;
            let cl_lo = u16::from_le_bytes([e[26], e[27]]) as u32;
            let first = (cl_hi << 16) | cl_lo;
            if first >= 2 {
                self.free_chain(first)?;
            }
            let from = group_start.unwrap_or(i);
            for k in from..=i {
                let mut tomb = slots[k];
                tomb[0] = 0xE5;
                self.patch_slot(&lbas, k, &tomb)?;
            }
            crate::klog!("fat32: disk: deleted /{} ({} bytes freed)", p, {
                u32::from_le_bytes([e[28], e[29], e[30], e[31]])
            });
            return Ok(());
        }
        Err("no such file")
    }
}

fn klog_write_ok(path: &str, len: usize) {
    crate::klog!("fat32: disk: wrote /{} ({} bytes)", path, len);
}

// ---------------------------------------------------------------------------
// Global mounts
// ---------------------------------------------------------------------------

/// Ramdisk (Limine module) — read-only, drives `ls/cat/run` as before.
pub static FAT: Spinlock<Option<Fat32>> = Spinlock::new(None);

/// Persistent AHCI disk — read-write, the v1.5 mount.
pub static DISK: Spinlock<Option<Fat32>> = Spinlock::new(None);

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
    let dev = Box::new(RamBlock { data: slice });
    let mut fs = Fat32::new(dev)?;
    let files = fs.root_file_count();
    let total = fs.total_bytes() / (1024 * 1024);
    *FAT.lock() = Some(fs);
    crate::klog!("fat32: ramdisk {} MiB at {:#x}, {} files in root", total, addr, files);
    Ok(files)
}

/// Probe the AHCI controller and mount the disk as the writable FAT32.
/// Missing hardware is not an error — the OS runs fine without a disk.
pub fn mount_ahci_disk() {
    let Some(dev) = ahci::probe() else {
        crate::klog!("fat32: no ahci disk found, running without persistent storage");
        return;
    };
    let info = dev.describe();
    let mut fs = match Fat32::new(dev) {
        Ok(f) => f,
        Err(e) => {
            crate::klog!("fat32: disk mount failed: {}", e);
            return;
        }
    };
    let files = fs.root_file_count();
    let total = fs.total_bytes() / (1024 * 1024);
    crate::klog!(
        "fat32: disk mounted {} MiB, {} files in root ({})",
        total,
        files,
        info
    );
    *DISK.lock() = Some(fs);
}
