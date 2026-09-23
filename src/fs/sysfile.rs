//! v1.6: file syscalls for ring 3 — userland programs finally own the
//! persistent disk.
//!
//! Until now the FAT32 disk was reachable only from kernel shell commands
//! (dsave/dcat/ddel/drun). This module exposes a small honest POSIX-ish
//! subset over `int 0x80`:
//!
//!   SYS_FILE_OPEN    36  open(name_ptr, name_len, flags) -> fd
//!   SYS_FILE_READ    37  read(fd, buf, len)              -> n
//!   SYS_FILE_WRITE   38  write(fd, buf, len)             -> n
//!   SYS_FILE_CLOSE   39  close(fd)                       -> 0
//!   SYS_FILE_SEEK    40  seek(fd, off, whence)           -> new pos
//!   SYS_FILE_UNLINK  41  unlink(name_ptr, name_len)      -> 0
//!   SYS_FILE_LIST    42  list(buf, max_entries)          -> count
//!
//! Open-file model: the disk layer reads and writes WHOLE files
//! (fs.cat / fs.write_file), so an open file is a heap buffer with a
//! cursor. open() loads the current contents (when the mode says so);
//! close() flushes a dirty buffer back to the FAT. This keeps the FS
//! layer untouched and gives userland real read-modify-write semantics.
//!
//! Flags (classic low bits):
//!   O_CREATE (1)  create-or-truncate
//!   O_RDWR   (2)  load existing contents, cursor 0, in-place patch
//!   O_APPEND (4)  open-or-create, cursor at end
//!   plain 0        read-only, must exist
//!
//! Files live in a small global table (16 slots, one global lock — same
//! shape as net::sock). Each slot records its owner pid: a process exit
//! closes (and flushes) everything it left open via on_task_exit(), so a
//! crashing program cannot leak slots or lose buffered bytes.
//!
//! Everything is synchronous (polled AHCI under the hood, like the shell
//! d-commands), so no would-block protocol is needed here.
//!
//! Lock order: SYSFILE_LOCK -> DISK (fat32). Never taken from an IRQ.

use crate::fs::fat32::{self, DISK};
use crate::klog;
use crate::mem::paging::{cr3, phys_to_virt};
use crate::mem::vmm::{AddressSpace, PAGE};
use crate::sync::Spinlock;

use alloc::string::String;
use alloc::vec::Vec;

pub const SYS_FILE_OPEN: u64 = 36;
pub const SYS_FILE_READ: u64 = 37;
pub const SYS_FILE_WRITE: u64 = 38;
pub const SYS_FILE_CLOSE: u64 = 39;
pub const SYS_FILE_SEEK: u64 = 40;
pub const SYS_FILE_UNLINK: u64 = 41;
pub const SYS_FILE_LIST: u64 = 42;

// flag bits (shared with user/src/lib.rs)
pub const O_CREATE: u64 = 1;
pub const O_RDWR: u64 = 2;
pub const O_APPEND: u64 = 4;

const NFILES: usize = 16;
const MAX_NAME: usize = 32; // 8.3 names are <= 12; be generous but bounded
const MAX_FILE: usize = 256 * 1024; // heap protection per open file
const MAX_RW: usize = 8192; // single syscall transfer cap
const MAX_LIST: usize = 32; // entries per list() call

struct OpenFile {
    owner: u64,
    name: String, // bare file name, no leading '/'
    buf: Vec<u8>,
    pos: usize,
    dirty: bool,
}

static FILES: Spinlock<[Option<OpenFile>; NFILES]> = Spinlock::new([const { None }; NFILES]);

/// Copy user memory in, page by page (same shape as sys_write's reader).
fn copy_user_in(space: &AddressSpace, uva: u64, dst: &mut [u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < dst.len() {
        let va = uva + done as u64;
        let page_left = (PAGE - (va & (PAGE - 1))) as usize;
        let n = (dst.len() - done).min(page_left);
        let phys = space.translate(va).ok_or(())?;
        unsafe {
            core::ptr::copy(
                phys_to_virt(phys) as *const u8,
                dst[done..].as_mut_ptr(),
                n,
            );
        }
        done += n;
    }
    Ok(())
}

/// Copy user memory out, page by page.
fn copy_user_out(space: &AddressSpace, uva: u64, src: &[u8]) -> Result<(), ()> {
    let mut done = 0usize;
    while done < src.len() {
        let va = uva + done as u64;
        let page_left = (PAGE - (va & (PAGE - 1))) as usize;
        let n = (src.len() - done).min(page_left);
        let phys = space.translate(va).ok_or(())?;
        unsafe {
            core::ptr::copy(
                src[done..].as_ptr(),
                phys_to_virt(phys) as *mut u8,
                n,
            );
        }
        done += n;
    }
    Ok(())
}

/// Read a user-supplied name (bounded, sanitized: printable, no '/').
fn read_name(space: &AddressSpace, uptr: u64, len: u64) -> Option<String> {
    if len == 0 || len as usize > MAX_NAME {
        return None;
    }
    let mut raw = [0u8; MAX_NAME];
    let n = len as usize;
    copy_user_in(space, uptr, &mut raw[..n]).ok()?;
    if raw[..n].iter().any(|&b| b < 0x21 || b > 0x7E || b == b'/') {
        return None; // no control chars, no spaces (FAT names), no paths
    }
    Some(String::from(core::str::from_utf8(&raw[..n]).ok()?))
}

// ---------------------------------------------------------------------------
// syscalls (called with the caller's CR3 loaded; pid = current task)
// ---------------------------------------------------------------------------

pub fn file_open(pid: u64, uptr: u64, name_len: u64, flags: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(name) = read_name(&space, uptr, name_len) else {
        klog!("file: open: bad name (len {})", name_len);
        return -1;
    };
    if (flags & !0x7) != 0 {
        klog!("file: open {}: bad flags {:#b}", name, flags);
        return -1;
    }

    let path = alloc::format!("/{}", name);
    let mut files = FILES.lock();
    let slot = match files.iter().position(|f| f.is_none()) {
        Some(i) => i,
        None => {
            klog!("file: open {}: table full", name);
            return -1;
        }
    };

    let mut dg = DISK.lock();
    let Some(fs) = dg.as_mut() else {
        klog!("file: open {}: no disk", name);
        return -1;
    };

    let exists = fs.lookup(&path).is_some();
    if !exists && (flags & (O_CREATE | O_RDWR | O_APPEND)) == 0 {
        klog!("file: open {}: miss (no O_CREATE)", name);
        return -1;
    }

    let (buf, note): (Vec<u8>, &'static str) =
        if (flags & O_CREATE) != 0 && (flags & (O_RDWR | O_APPEND)) == 0 {
            // pure O_CREATE: truncate
            (Vec::new(), if exists { "trunc" } else { "creat" })
        } else {
            match fs.cat(&path) {
                Ok(b) => {
                    if (flags & O_CREATE) != 0 {
                        // O_RDWR|O_CREATE / O_APPEND|O_CREATE keep contents
                        (b, if exists { "hit" } else { "creat" })
                    } else {
                        (b, "hit")
                    }
                }
            Err(_) if (flags & O_CREATE) != 0 => (Vec::new(), "creat"),
            Err(_) => {
                klog!("file: open {}: read error", name);
                return -1;
            }
        }
    };
    if buf.len() > MAX_FILE {
        klog!("file: open {}: too big ({} bytes)", name, buf.len());
        return -1;
    }
    let mut pos = 0usize;
    if (flags & O_APPEND) != 0 {
        pos = buf.len();
    }

    files[slot] = Some(OpenFile {
        owner: pid,
        name,
        buf,
        pos,
        dirty: false,
    });
    let size = files[slot].as_ref().map_or(0, |f| f.buf.len());
    klog!(
        "file: open fd={} {} ({} bytes, {})",
        slot,
        path,
        size,
        note
    );
    slot as i64
}

pub fn file_read(_pid: u64, fd: u64, uptr: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    let len = (len as usize).min(MAX_RW);
    if fd as usize >= NFILES {
        return -1;
    }
    let space = AddressSpace::from_pml4(cr3());
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_mut() else {
        return -1;
    };
    let n = (f.buf.len() - f.pos.min(f.buf.len())).min(len);
    let out = &f.buf[f.pos..f.pos + n];
    match copy_user_out(&space, uptr, out) {
        Ok(_) => {
            f.pos += n;
            klog!("file: read fd={} n={}", fd, n);
            n as i64
        }
        Err(_) => -1,
    }
}

pub fn file_write(_pid: u64, fd: u64, uptr: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    let len = (len as usize).min(MAX_RW);
    if fd as usize >= NFILES {
        return -1;
    }
    let space = AddressSpace::from_pml4(cr3());
    let mut chunk = alloc::vec![0u8; len];
    if copy_user_in(&space, uptr, &mut chunk).is_err() {
        klog!("file: write fd={}: bad user buffer", fd);
        return -1;
    }
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_mut() else {
        return -1;
    };
    if f.pos + len > MAX_FILE {
        klog!("file: write fd={}: {} limit hit", fd, f.name);
        return -1;
    }
    if f.pos > f.buf.len() {
        // write past EOF: fill the gap with zeros like POSIX
        f.buf.resize(f.pos, 0);
    }
    let end = f.pos + len;
    if end > f.buf.len() {
        f.buf.resize(end, 0);
    }
    f.buf[f.pos..end].copy_from_slice(&chunk);
    f.pos = end;
    f.dirty = true;
    klog!("file: write fd={} n={} ({} bytes)", fd, len, end);
    len as i64
}

pub fn file_seek(_pid: u64, fd: u64, off: i64, whence: u64) -> i64 {
    if fd as usize >= NFILES {
        return -1;
    }
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_mut() else {
        return -1;
    };
    let base: i64 = match whence {
        0 => 0,
        1 => f.pos as i64,
        2 => f.buf.len() as i64,
        _ => return -1,
    };
    let new = base + off;
    if new < 0 || new as usize > MAX_FILE {
        return -1;
    }
    f.pos = new as usize;
    klog!("file: seek fd={} whence={} -> {}", fd, whence, f.pos);
    f.pos as i64
}

/// Flush a dirty file to the FAT. Caller holds FILES.
fn flush(files: &mut [Option<OpenFile>; NFILES], slot: usize) {
    let Some(f) = files[slot].as_mut() else {
        return;
    };
    if !f.dirty {
        return;
    }
    let (name, data) = (f.name.clone(), core::mem::take(&mut f.buf));
    f.pos = 0;
    f.dirty = false;
    match DISK.lock().as_mut() {
        Some(fs) => match fs.write_file(&alloc::format!("/{}", name), &data) {
            Ok(_) => klog!("file: flush {} ({} bytes)", name, data.len()),
            Err(e) => {
                // the buffer was already taken -- report loudly instead of
                // pretending the data is safe
                klog!("file: flush {}: FAILED ({}) -- {} bytes lost", name, e, data.len());
            }
        },
        None => klog!("file: flush {}: no disk ({} bytes lost)", name, data.len()),
    }
}

pub fn file_close(pid: u64, fd: u64) -> i64 {
    if fd as usize >= NFILES {
        return -1;
    }
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_ref() else {
        return -1;
    };
    if f.owner != pid {
        klog!("file: close fd={}: not owner ({} != {})", fd, pid, f.owner);
        return -1;
    }
    flush(&mut files, fd as usize);
    let name = files[fd as usize].as_ref().unwrap().name.clone();
    files[fd as usize] = None;
    klog!("file: close fd={} {} (by task {})", fd, name, pid);
    0
}

pub fn file_unlink(_pid: u64, uptr: u64, name_len: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(name) = read_name(&space, uptr, name_len) else {
        return -1;
    };
    let files = FILES.lock();
    // POSIX-ish honesty: refuse to unlink a file that is currently open
    for (i, f) in files.iter().enumerate() {
        if let Some(f) = f {
            if f.name == name {
                klog!("file: unlink {}: open fd={} blocks it", name, i);
                return -1;
            }
        }
    }
    drop(files);
    let mut fs = DISK.lock();
    match fs.as_mut() {
        Some(fs) => match fs.delete(&alloc::format!("/{}", name)) {
            Ok(_) => {
                klog!("file: unlink {} ok", name);
                0
            }
            Err(e) => {
                klog!("file: unlink {}: {}", name, e);
                -1
            }
        },
        None => -1,
    }
}

/// List the disk root into a packed user buffer, one record per entry:
///   [u8 kind (0 file, 1 dir)][u8 name_len][name bytes...][u32 size LE]
/// Returns the number of records written, or -1.
pub fn file_list(_pid: u64, uptr: u64, max_entries: u64) -> i64 {
    if max_entries == 0 {
        return 0;
    }
    let max = (max_entries as usize).min(MAX_LIST);
    let entries = {
        let mut dg = DISK.lock();
        let Some(fs) = dg.as_mut() else {
            return -1;
        };
        match fs.ls("/") {
            Ok(e) => e,
            Err(_) => return -1,
        }
    };
    let space = AddressSpace::from_pml4(cr3());
    let mut packed: Vec<u8> = Vec::new();
    let mut count = 0usize;
    for e in entries.iter().take(max) {
        let name = &e.name;
        if name.len() > 255 {
            continue;
        }
        let rec = 2 + name.len() + 4;
        if packed.len() + rec > MAX_RW {
            break;
        }
        packed.push(if e.is_dir { 1 } else { 0 });
        packed.push(name.len() as u8);
        packed.extend_from_slice(name.as_bytes());
        packed.extend_from_slice(&e.size.to_le_bytes());
        count += 1;
    }
    match copy_user_out(&space, uptr, &packed) {
        Ok(_) => {
            klog!("file: list -> {} entries ({} bytes)", count, packed.len());
            count as i64
        }
        Err(_) => -1,
    }
}

/// Process exit: close (and flush) every file owned by `pid`. Called from
/// sched::exit_current AFTER SCHED_LOCK is released (same rule as
/// gui::on_task_exit -- lock order forbids SCHED_LOCK -> SYSFILE_LOCK).
pub fn on_task_exit(pid: u64) {
    let mut files = FILES.lock();
    for slot in 0..NFILES {
        let owned = files[slot].as_ref().map_or(false, |f| f.owner == pid);
        if owned {
            flush(&mut files, slot);
            let name = files[slot].as_ref().unwrap().name.clone();
            klog!("file: task {} exit: closed fd {} {}", pid, slot, name);
            files[slot] = None;
        }
    }
}

/// Shell/diag introspection: how many slots are open right now.
pub fn open_count() -> usize {
    FILES.lock().iter().filter(|f| f.is_some()).count()
}
