//! v1.7: `exec` — replace the calling process image, entirely from ring 3.
//!
//! Until now only the kernel could start programs (shell `run`/`drun`,
//! terminal sessions, the start menu). fork() existed since v0.6, wait()
//! since v0.3 — but the third leg of the Unix process model was missing:
//! a ring-3 process could not BECOME another program. SYS_EXEC closes
//! that gap, and fork+exec+wait composes into the classic pattern:
//!
//!     pid = fork();
//!     if (pid == 0) exec("/BIN/ARGS.ELF", ["ARGS.ELF", "hi"]);
//!     code = wait(pid);
//!
//! ABI (int 0x80, number 43):
//!   rdi = path pointer, rsi = path length
//!   rdx = argv block pointer, rcx = argv block length
//!   argv block = [u32 argc][u32 len0][bytes0][u32 len1][bytes1]...
//!   argv[0] must be present (the program name, classic convention).
//!
//! Returns -1 on failure and the caller keeps running its old image.
//! On success the syscall NEVER RETURNS to the caller: the saved ring-3
//! frame is rewritten in place (rip/rsp/rdi/rsi) to land at the new
//! image's entry, CR3 is switched to the new address space on this very
//! interrupt, and the old image is destroyed before the iretq.
//!
//! Semantics kept honestly small:
//!   * refused inside a thread group (POSIX would kill the siblings) —
//!     returns -1 instead of surprising thread semantics;
//!   * open file descriptors survive (they are keyed by pid, exec keeps
//!     the pid) — same as POSIX;
//!   * caught signal handlers reset to default, pending signals dropped
//!     (POSIX exec), TLS base cleared (the old TLS block dies with the
//!     old address space);
//!   * the image is looked up on the PERSISTENT DISK first (v1.5 home of
//!     userland files), ramdisk second (seeded build artifacts).
//!
//! Failure atomicity: the new address space is built COMPLETELY before
//! anything in the current task is touched; every failure path just
//! returns -1 with the old image intact.

use crate::cpu::idt::Regs;
use crate::klog;
use crate::mem::paging::cr3;
use crate::mem::vmm::AddressSpace;
use crate::sched;

use super::task::{build_user_image, spawn_limits};
use super::uaccess::read_user_bytes;

pub const SYS_EXEC: u64 = 43;

const MAX_PATH: usize = 64;
const MAX_BLOCK: usize = 2048;

/// Entry syscall: never returns on success; on failure regs.rax = -1 and
/// the caller's image is untouched.
pub fn sys_exec(regs: &mut Regs) {
    regs.rax = (-1i64) as u64; // provisional: every early-out leaves this

    let upath = regs.rdi;
    let path_len = regs.rsi as usize;
    let ublock = regs.rdx;
    let block_len = regs.rcx as usize;

    if path_len == 0 || path_len > MAX_PATH || block_len == 0 || block_len > MAX_BLOCK {
        klog!("exec: rejected sizes path_len={} block_len={}", path_len, block_len);
        return;
    }

    // 1. copy path + argv block in from the CALLER's address space (the
    //    interrupt came from ring 3, so CR3 is the caller's table set)
    let space = AddressSpace::from_pml4(cr3());
    let mut pbuf = [0u8; MAX_PATH];
    if read_user_bytes(&space, upath, &mut pbuf[..path_len]).is_err() {
        klog!("exec: unreadable path pointer {:#x}", upath);
        return;
    }
    if !pbuf[..path_len].iter().all(u8::is_ascii_graphic) {
        klog!("exec: bad bytes in path");
        return;
    }
    let path = core::str::from_utf8(&pbuf[..path_len]).unwrap_or("");
    if path.is_empty() {
        return;
    }

    let mut bbuf = [0u8; MAX_BLOCK];
    if read_user_bytes(&space, ublock, &mut bbuf[..block_len]).is_err() {
        klog!("exec: unreadable argv block {:#x}", ublock);
        return;
    }
    let args = match parse_block(&bbuf[..block_len]) {
        Some(a) => a,
        None => {
            klog!("exec: malformed argv block ({} bytes)", block_len);
            return;
        }
    };

    // 2. read the image: persistent disk first, ramdisk second
    let (bytes, source) = match read_image(path) {
        Ok(r) => r,
        Err(e) => {
            klog!("exec: pid {} '{}': {}", sched::current_pid(), path, e);
            return;
        }
    };

    // 3. build the complete replacement image BEFORE touching this task
    let short = short_name(path);
    let arg_refs: alloc::vec::Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let img = match build_user_image(&bytes, short, &arg_refs) {
        Ok(i) => i,
        Err(e) => {
            klog!("exec: pid {} '{}': build failed: {}", sched::current_pid(), path, e);
            return;
        }
    };

    // 4. swap: address space, name, frame surgery, CR3, old-image teardown
    let frame = regs as *mut Regs;
    match sched::exec_replace_image(img.space, img.entry, img.rsp, img.argc, img.argv, short, frame) {
        Ok(()) => {
            // success: this context never runs again — the frame now
            // points at the new image's entry; iretq lands there.
            klog!(
                "exec: pid {} became {} ({} args, from {}, {} bytes)",
                sched::current_pid(),
                path,
                args.len(),
                source,
                bytes.len()
            );
        }
        Err(e) => {
            // the image was dropped inside exec_replace_image
            klog!("exec: pid {} refused: {}", sched::current_pid(), e);
        }
    }
}

/// argv block layout: [u32 argc][u32 len][bytes]... (little-endian).
/// argc must be >= 1 and <= spawn_limits::MAX_ARGC (argv[0] = program
/// name is the classic convention and our demos rely on it).
fn parse_block(block: &[u8]) -> Option<alloc::vec::Vec<alloc::string::String>> {
    if block.len() < 4 {
        return None;
    }
    let argc = u32::from_le_bytes([block[0], block[1], block[2], block[3]]) as usize;
    if argc == 0 || argc > spawn_limits::MAX_ARGC {
        return None;
    }
    let mut off = 4;
    let mut out = alloc::vec::Vec::with_capacity(argc);
    for _ in 0..argc {
        if off + 4 > block.len() {
            return None;
        }
        let l = u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]])
            as usize;
        off += 4;
        if l > spawn_limits::MAX_ARG || off + l > block.len() {
            return None;
        }
        let s = core::str::from_utf8(&block[off..off + l]).ok()?;
        out.push(alloc::string::String::from(s));
        off += l;
    }
    Some(out)
}

/// FAT-style lookup: the name as given (uppercased, leading '/'), plus a
/// /BIN/ variant for bare names — the same convention as shell `run`.
/// Disk first (persistent), ramdisk second (seeded).
fn read_image(path: &str) -> Result<(alloc::vec::Vec<u8>, &'static str), &'static str> {
    let mut tried: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();

    let base = if path.contains('/') {
        alloc::format!("/{}", path.trim_start_matches('/'))
    } else {
        alloc::format!("/{}", path.to_ascii_uppercase())
    };
    tried.push(base.clone());
    let bin = if path.contains('/') {
        base.clone()
    } else {
        alloc::format!("/BIN/{}", path.to_ascii_uppercase())
    };
    if bin != base {
        tried.push(bin.clone());
    }

    // 1) persistent AHCI disk (v1.5): the authoritative home of userland
    {
        let mut d = crate::fs::fat32::DISK.lock();
        if let Some(fs) = d.as_mut() {
            for cand in &tried {
                if let Ok(b) = fs.cat(cand) {
                    return Ok((b, "disk"));
                }
            }
        }
    }
    // 2) ramdisk (build-seeded FAT32)
    {
        let mut fat = crate::fs::fat32::FAT.lock();
        if let Some(fs) = fat.as_mut() {
            for cand in &tried {
                if let Ok(b) = fs.cat(cand) {
                    return Ok((b, "ramdisk"));
                }
            }
        }
    }
    Err("no such file on disk or ramdisk")
}

/// Last path component — the ps-visible task name. `set_name` copies it
/// into the Task's fixed [u8; 12] under the scheduler lock, so a plain
/// borrow of the caller's path is enough (no leak, no allocation).
fn short_name<'a>(path: &'a str) -> &'a str {
    path.rsplit('/').next().unwrap_or(path)
}
