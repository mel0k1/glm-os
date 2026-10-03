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
//!   SYS_FILE_LIST    42  list(path_ptr, path_len, buf, max) -> count
//!
//! v1.9 adds the directory dimension — the persistent tree is no longer
//! flat:
//!
//!   SYS_CHDIR        49  chdir(path_ptr, path_len)       -> 0
//!   SYS_GETCWD       50  getcwd(buf, max)                -> len
//!   SYS_MKDIR        51  mkdir(path_ptr, path_len)       -> 0
//!   SYS_RMDIR        52  rmdir(path_ptr, path_len)       -> 0
//!
//! Every path-taking call (open/unlink/list) resolves RELATIVE paths
//! against the calling task's working directory (sched::cwd_of_pid) and
//! normalizes "."/".." segments; open stores the resolved ABSOLUTE path
//! in the open-file table so close/flush lands the bytes in the right
//! directory even after the task moved elsewhere.
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
const MAX_NAME: usize = 64; // v1.9: nested paths allowed; bounded but roomy
const MAX_FILE: usize = 256 * 1024; // heap protection per open file
const MAX_RW: usize = 8192; // single syscall transfer cap
const MAX_LIST: usize = 32; // entries per list() call

struct OpenFile {
    owner: u64,
    name: String, // v1.9: resolved ABSOLUTE path ("/HOME/DOCS/F.TXT")
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
    // v1.9: '/' is LEGAL now — paths resolve through resolve_path();
    // control chars and spaces stay forbidden (FAT names)
    if raw[..n].iter().any(|&b| b < 0x21 || b > 0x7E) {
        return None;
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
    open_name(pid, &name, flags)
}

/// v1.9: resolve a user-supplied path against the task's working
/// directory: absolute paths pass through, relative paths join the cwd;
/// both then get "."/".." segments collapsed. Always returns a
/// normalized absolute path.
pub fn resolve_path(pid: u64, path: &str) -> String {
    let cwd = if path.starts_with('/') {
        String::new()
    } else {
        crate::sched::cwd_of_pid(pid)
    };
    let mut stack: Vec<String> = Vec::new();
    for part in cwd.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(String::from(other)),
        }
    }
    if stack.is_empty() {
        String::from("/")
    } else {
        let mut out = String::new();
        for p in &stack {
            out.push('/');
            out.push_str(p);
        }
        out
    }
}

/// v1.8: kernel-internal open by &str (the shell's redirect builder).
/// v1.9: the name may be a nested path ("/HOME/F.TXT") or relative
/// (resolved against the calling task's cwd); only printable ASCII is
/// accepted, whitespace still forbidden.
pub fn open_name(pid: u64, name: &str, flags: u64) -> i64 {
    if name.is_empty()
        || name.len() > MAX_NAME
        || name
            .bytes()
            .any(|b| b < 0x21 || b > 0x7E)
    {
        klog!("file: open: bad name (kernel path)");
        return -1;
    }
    if (flags & !0x7) != 0 {
        klog!("file: open {}: bad flags {:#b}", name, flags);
        return -1;
    }
    let name = String::from(name);
    let path = resolve_path(pid, &name);
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
        name: path,
        buf,
        pos,
        dirty: false,
    });
    let size = files[slot].as_ref().map_or(0, |f| f.buf.len());
    klog!(
        "file: open fd={} {} ({} bytes, {})",
        slot,
        files[slot].as_ref().map_or(String::new(), |f| f.name.clone()),
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
        Some(fs) => match fs.write_file(&name, &data) {
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

pub fn file_unlink(pid: u64, uptr: u64, name_len: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(name) = read_name(&space, uptr, name_len) else {
        return -1;
    };
    // v1.9: resolve BEFORE taking any other lock (sched is a leaf here)
    let path = resolve_path(pid, &name);
    let files = FILES.lock();
    // POSIX-ish honesty: refuse to unlink a file that is currently open
    for (i, f) in files.iter().enumerate() {
        if let Some(f) = f {
            if f.name == path {
                klog!("file: unlink {}: open fd={} blocks it", path, i);
                return -1;
            }
        }
    }
    drop(files);
    let mut fs = DISK.lock();
    match fs.as_mut() {
        Some(fs) => match fs.delete(&path) {
            Ok(_) => {
                klog!("file: unlink {} ok", path);
                0
            }
            Err(e) => {
                klog!("file: unlink {}: {}", path, e);
                -1
            }
        },
        None => -1,
    }
}

/// List a directory of the persistent disk into a packed user buffer,
/// one record per entry:
///   [u8 kind (0 file, 1 dir)][u8 name_len][name bytes...][u32 size LE]
/// v1.9: the path may be nested or relative (resolved against the
/// calling task's cwd); an empty path means "/". Returns the number of
/// records written, or -1.
pub fn file_list(pid: u64, path_ptr: u64, path_len: u64, uptr: u64, max_entries: u64) -> i64 {
    if max_entries == 0 {
        return 0;
    }
    let raw = if path_len == 0 {
        String::new()
    } else {
        let space = AddressSpace::from_pml4(cr3());
        match read_name(&space, path_ptr, path_len) {
            Some(p) => p,
            None => return -1,
        }
    };
    let path = resolve_path(pid, &raw);
    let max = (max_entries as usize).min(MAX_LIST);
    let entries = {
        let mut dg = DISK.lock();
        let Some(fs) = dg.as_mut() else {
            return -1;
        };
        match fs.ls(&path) {
            Ok(e) => e,
            Err(e) => {
                klog!("file: list {}: {}", path, e);
                return -1;
            }
        }
    };
    let space = AddressSpace::from_pml4(cr3());
    let mut packed: Vec<u8> = Vec::new();
    let mut count = 0usize;
    for e in entries.iter() {
        // v1.9: hide the "." / ".." self/parent slots from listings —
        // ring-3 walkers (TREE) would otherwise loop on them forever
        if e.name == "." || e.name == ".." {
            continue;
        }
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
            klog!("file: list {} -> {} entries ({} bytes)", path, count, packed.len());
            count as i64
        }
        Err(_) => -1,
    }
}

// ---------------------------------------------------------------------------
// v1.9: the directory syscalls — chdir / getcwd / mkdir / rmdir
// ---------------------------------------------------------------------------

/// chdir(path) -> 0: resolve, require an existing directory on the
/// persistent disk, then move the calling task's cwd there.
pub fn file_chdir(pid: u64, uptr: u64, len: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(raw) = read_name(&space, uptr, len) else {
        klog!("file: chdir: bad path");
        return -1;
    };
    chdir_name(pid, &raw)
}

/// v1.9: kernel-internal chdir (the shell's `cd`); the caller is the
/// current task (the shell always is).
pub fn chdir_name(pid: u64, raw: &str) -> i64 {
    let path = resolve_path(pid, raw);
    let ok = {
        let mut dg = DISK.lock();
        match dg.as_mut() {
            Some(fs) => match fs.lookup(&path) {
                Some(e) if e.is_dir => true,
                Some(_) => {
                    klog!("file: chdir {}: not a directory", path);
                    false
                }
                None => {
                    klog!("file: chdir {}: no such directory", path);
                    false
                }
            },
            None => false,
        }
    };
    if ok {
        klog!("file: chdir pid {} -> {} ok", pid, path);
        crate::sched::set_cwd_current(path);
        0
    } else {
        -1
    }
}

/// getcwd(buf, max) -> len: copy the calling task's cwd out.
pub fn file_getcwd(pid: u64, uptr: u64, max: u64) -> i64 {
    if max == 0 {
        return -1;
    }
    let cwd = crate::sched::cwd_of_pid(pid);
    let bytes = cwd.as_bytes();
    if bytes.len() as u64 > max {
        klog!("file: getcwd: buffer too small ({} < {})", max, bytes.len());
        return -1;
    }
    let space = AddressSpace::from_pml4(cr3());
    match copy_user_out(&space, uptr, bytes) {
        Ok(_) => {
            klog!("file: getcwd pid {} -> {}", pid, cwd);
            bytes.len() as i64
        }
        Err(_) => -1,
    }
}

/// mkdir(path) -> 0: create a directory node (parent must exist).
pub fn file_mkdir(pid: u64, uptr: u64, len: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(raw) = read_name(&space, uptr, len) else {
        return -1;
    };
    mkdir_name(pid, &raw)
}

/// v1.9: kernel-internal mkdir (the shell's `mkdir`).
pub fn mkdir_name(pid: u64, raw: &str) -> i64 {
    let path = resolve_path(pid, raw);
    let mut dg = DISK.lock();
    match dg.as_mut() {
        Some(fs) => match fs.mkdir(&path) {
            Ok(_) => 0,
            Err(e) => {
                klog!("file: mkdir {}: {}", path, e);
                -1
            }
        },
        None => -1,
    }
}

/// rmdir(path) -> 0: remove an empty directory node.
pub fn file_rmdir(pid: u64, uptr: u64, len: u64) -> i64 {
    let space = AddressSpace::from_pml4(cr3());
    let Some(raw) = read_name(&space, uptr, len) else {
        return -1;
    };
    rmdir_name(pid, &raw)
}

/// v1.9: kernel-internal rmdir (the shell's `rmdir`).
pub fn rmdir_name(pid: u64, raw: &str) -> i64 {
    let path = resolve_path(pid, raw);
    let mut dg = DISK.lock();
    match dg.as_mut() {
        Some(fs) => match fs.rmdir(&path) {
            Ok(_) => 0,
            Err(e) => {
                klog!("file: rmdir {}: {}", path, e);
                -1
            }
        },
        None => -1,
    }
}

// ---------------------------------------------------------------------------
// v1.8: kernel-internal byte I/O on an open fd (the stdio-redirect path)
// ---------------------------------------------------------------------------

/// Append `data` at the file's cursor (stdout-redirect target). The data
/// becomes durable on close/flush, exactly like the syscall path. Returns
/// bytes written or -1 (bad fd / over the size limit). No ownership check:
/// the fd belongs to the SHELL, the writing child only borrows it.
pub fn write_bytes_fd(fd: u64, data: &[u8]) -> i64 {
    if data.is_empty() {
        return 0;
    }
    if fd as usize >= NFILES {
        return -1;
    }
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_mut() else {
        return -1;
    };
    let len = data.len().min(MAX_RW);
    if f.pos + len > MAX_FILE {
        klog!("file: write_bytes_fd {}: {} limit hit", fd, f.name);
        return -1;
    }
    if f.pos > f.buf.len() {
        f.buf.resize(f.pos, 0);
    }
    let end = f.pos + len;
    if end > f.buf.len() {
        f.buf.resize(end, 0);
    }
    f.buf[f.pos..end].copy_from_slice(&data[..len]);
    f.pos = end;
    f.dirty = true;
    len as i64
}

/// Read at the file's cursor into a kernel buffer (stdin-redirect source).
/// Returns bytes read, 0 = EOF, -1 = bad fd.
pub fn read_bytes_fd(fd: u64, out: &mut [u8]) -> i64 {
    if out.is_empty() {
        return 0;
    }
    if fd as usize >= NFILES {
        return -1;
    }
    let mut files = FILES.lock();
    let Some(f) = files[fd as usize].as_mut() else {
        return -1;
    };
    let n = (f.buf.len() - f.pos.min(f.buf.len())).min(out.len());
    out[..n].copy_from_slice(&f.buf[f.pos..f.pos + n]);
    f.pos += n;
    n as i64
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
