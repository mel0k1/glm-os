//! v1.8: kernel pipes — the classic Unix byte stream between processes.
//!
//! A pipe is a 4 KiB ring buffer with two ends. `pipe()` returns BOTH ends
//! packed into one u64 (read handle in the low half, write handle in the
//! high half); the shell hands one end to each pipeline stage. Ends are
//! reference counted: every stage (and the shell, until it explicitly
//! closes its copies) holds a count, and
//!
//!   * read() returns 0 (EOF) only when the buffer is empty AND writers==0
//!   * write() fails with -1 (broken pipe) when readers==0
//!
//! so downstream stages see a clean end-of-stream exactly when every
//! upstream stage is gone.
//!
//! Blocking is the honest TCP v1.3 pattern: a syscall that cannot make
//! progress drops the pipe lock and sleeps a tick in the CALLING task's
//! context (ksyscall SYS_SLEEP), then retries. No waiter registration, no
//! wakeup races, and the CPU is yielded while waiting.
//!
//! Every task records its open pipe ends in `Task::pipes_mask` (bit N =
//! end N): fork copies the mask (Unix fd inheritance), spawn starts
//! empty, and task exit releases the task's ends so a killed process can
//! never wedge a pipeline. Handles are `slot * 2 + dir` (0 = read, 1 =
//! write), 8 pipes -> 16 mask bits.
//!
//! Lock order: SCHED_LOCK -> PIPE_LOCK (post_dispatch releases a dying
//! task's ends under SCHED_LOCK). The reverse never happens: PIPE_LOCK is
//! always dropped before sleeping or calling into the scheduler.

use crate::klog;
use crate::sched;
use crate::sync::Spinlock;
use crate::user::uaccess::{read_user_bytes, write_user_bytes};

pub const NPIPES: usize = 8;
pub const CAP: usize = 4096;
/// Max bytes one SYS_WRITE-redirect feed chunk pushes at a time: small
/// enough that a full pipe frees space between chunks.
const FEED_CHUNK: usize = 512;
const TICK_SLEEP_MS: u64 = 5;

pub const REDIR_PIPE: u32 = 0x1_0000; // value = pipe handle (slot*2+dir)
pub const REDIR_FILE: u32 = 0x2_0000; // value = sysfile fd
/// v1.8: sentinel for "keep the inherited side" in an explicit redirect
/// tuple (NewTask.redirect is Some, but this side stays as-is).
pub const REDIR_INHERIT: u32 = u32::MAX;
/// Pack a redirect word: kind flag | 16-bit value.
pub const fn redir(kind: u32, v: u64) -> u32 {
    kind | (v as u16 as u32)
}
/// Unpack: Some((kind, value)) or None (0 = no redirect).
pub const fn redir_parts(w: u32) -> Option<(u32, u64)> {
    if w == 0 {
        None
    } else {
        Some((w & 0xFFFF_0000, (w & 0xFFFF) as u64))
    }
}

struct Pipe {
    used: bool,
    buf: [u8; CAP],
    head: usize, // next write position
    len: usize,
    readers: u32,
    writers: u32,
    wrote: u64,
    got: u64,
}

impl Pipe {
    const fn fresh() -> Self {
        Self {
            used: false,
            buf: [0; CAP],
            head: 0,
            len: 0,
            readers: 0,
            writers: 0,
            wrote: 0,
            got: 0,
        }
    }
}

static mut PIPES: [Pipe; NPIPES] = [const { Pipe::fresh() }; NPIPES];
static PIPE_LOCK: Spinlock<()> = Spinlock::new(());

fn pipes() -> &'static mut [Pipe; NPIPES] {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

// --- handle helpers (caller holds PIPE_LOCK or validated earlier) ---

const fn slot_of(h: u64) -> Option<usize> {
    let s = (h / 2) as usize;
    if s < NPIPES {
        Some(s)
    } else {
        None
    }
}
const fn is_write_end(h: u64) -> bool {
    h & 1 == 1
}

fn end_counts(h: u64) -> Option<(&'static mut u32, &'static mut u32)> {
    let s = slot_of(h)?;
    let p = &mut pipes()[s];
    if !p.used {
        return None;
    }
    Some((&mut p.readers, &mut p.writers))
}

// ---------------------------------------------------------------------------
// syscalls
// ---------------------------------------------------------------------------

/// pipe() -> ((write_handle) << 32) | read_handle, or -1. The caller owns
/// BOTH ends; it must close them when done (normally right after handing
/// them to the pipeline stages).
pub fn open() -> i64 {
    let _g = PIPE_LOCK.lock();
    let slot = match pipes().iter().position(|p| !p.used) {
        Some(i) => i,
        None => {
            klog!("pipe: table full");
            return -1;
        }
    };
    pipes()[slot] = Pipe {
        used: true,
        readers: 1,
        writers: 1,
        ..Pipe::fresh()
    };
    let (rh, wh) = ((slot * 2) as u64, (slot * 2 + 1) as u64);
    klog!("pipe: slot {} open (rh={} wh={})", slot, rh, wh);
    ((wh << 32) | rh) as i64
}

/// close(handle): drop one end. When both ends hit zero the slot is freed.
pub fn close(h: u64) -> i64 {
    let _g = PIPE_LOCK.lock();
    let Some((readers, writers)) = end_counts(h) else {
        return -1;
    };
    if is_write_end(h) {
        if *writers == 0 {
            return -1;
        }
        *writers -= 1;
    } else {
        if *readers == 0 {
            return -1;
        }
        *readers -= 1;
    }
    let s = slot_of(h).unwrap_or(0);
    let dead = pipes()[s].readers == 0 && pipes()[s].writers == 0;
    if dead {
        let (wrote, got) = (pipes()[s].wrote, pipes()[s].got);
        pipes()[s] = Pipe::fresh();
        drop(_g);
        klog!("pipe: slot {} freed ({} bytes written, {} read)", s, wrote, got);
    } else {
        drop(_g);
        klog!("pipe: end {} closed (slot {})", h, s);
    }
    0
}

/// Release every end whose bit is set in `mask` (task exit hook). Returns
/// the number of ends released. Safe to call with 0.
pub fn release_task_ends(mask: u16) -> usize {
    if mask == 0 {
        return 0;
    }
    let mut n = 0;
    for bit in 0..16u16 {
        if mask & (1 << bit) != 0 {
            if close(bit as u64) == 0 {
                n += 1;
            }
        }
    }
    let pid = sched::current_pid();
    klog!("pipe: task {} exit released {} pipe end(s)", pid, n);
    n
}

/// v1.8: add one reference to a pipe end for a NEW owner (spawn/fork
/// recording a pipe redirect into the child's mask). Unix dup2 semantics:
/// the end's refcount must cover every task that holds the handle, or a
/// shell dropping its own copies would free a live pipe.
/// Called with SCHED_LOCK held (SCHED -> PIPE order).
pub fn acquire_end(h: u64) {
    let _g = PIPE_LOCK.lock();
    let Some(s) = slot_of(h) else { return };
    let p = &mut pipes()[s];
    if !p.used {
        return;
    }
    if is_write_end(h) {
        p.writers += 1;
    } else {
        p.readers += 1;
    }
}

/// Collect the pipe-end bits implied by a redirect tuple (spawn/fork).
pub fn redirect_pipe_mask(out_dst: u32, in_src: u32) -> u16 {
    let mut m = 0u16;
    for w in [out_dst, in_src] {
        if let Some((REDIR_PIPE, h)) = redir_parts(w) {
            if (h as usize) < 16 {
                m |= 1 << h as u16;
            }
        }
    }
    m
}

/// pipe_read(h, uva, len): blocking read; 0 = EOF (drained, no writers);
/// -1 = bad handle; -2 = bad user buffer. TCP-style sleep loop.
pub fn read(h: u64, uva: u64, len: u64) -> i64 {
    let len = (len as usize).min(CAP);
    if len == 0 {
        return 0;
    }
    let space = crate::mem::vmm::AddressSpace::from_pml4(crate::mem::paging::cr3());
    loop {
        enum R {
            Data(usize, [u8; CAP]),
            Eof,
            Block,
        }
        let r = {
            let _g = PIPE_LOCK.lock();
            let Some(s) = slot_of(h) else {
                return -1;
            };
            let p = &mut pipes()[s];
            if !p.used || p.readers == 0 {
                return -1;
            }
            if p.len > 0 {
                let n = p.len.min(len);
                let mut tmp = [0u8; CAP];
                let tail = (p.head + CAP - p.len) % CAP;
                for k in 0..n {
                    tmp[k] = p.buf[(tail + k) % CAP];
                }
                p.len -= n;
                p.head = (p.head + n) % CAP; // head only matters when len>0
                p.got += n as u64;
                R::Data(n, tmp)
            } else if p.writers == 0 {
                R::Eof
            } else {
                R::Block
            }
        };
        match r {
            R::Data(n, tmp) => {
                if write_user_bytes(&space, uva, &tmp[..n]).is_err() {
                    return -2;
                }
                return n as i64;
            }
            R::Eof => return 0,
            R::Block => {
                sched::ksyscall(sched::SYS_SLEEP, TICK_SLEEP_MS, 0, 0);
            }
        }
    }
}

/// pipe_write(h, uva, len): blocking write; -1 = broken pipe (no readers);
/// -2 = bad user buffer.
pub fn write(h: u64, uva: u64, len: u64) -> i64 {
    if len == 0 {
        return 0;
    }
    let space = crate::mem::vmm::AddressSpace::from_pml4(crate::mem::paging::cr3());
    let mut chunk = [0u8; CAP];
    let mut done = 0usize;
    let total = len as usize;
    while done < total {
        let n = (total - done).min(CAP);
        if read_user_bytes(&space, uva + done as u64, &mut chunk[..n]).is_err() {
            return -2;
        }
        match write_kernel(h, &chunk[..n]) {
            ok if ok == n as i64 => done += n,
            -1 => return if done > 0 { done as i64 } else { -1 },
            -2 => return -2,
            _ => return -1,
        }
    }
    done as i64
}

/// Kernel-internal blocking push (used by pipe_write and by the SYS_WRITE
/// stdout-redirect feed). Pushes ALL bytes or fails: -1 broken pipe.
pub fn write_kernel(h: u64, data: &[u8]) -> i64 {
    let mut off = 0usize;
    loop {
        let pushed = {
            let _g = PIPE_LOCK.lock();
            let Some(s) = slot_of(h) else {
                return -1;
            };
            let p = &mut pipes()[s];
            if !p.used {
                return -1;
            }
            if p.readers == 0 {
                klog!("pipe: broken pipe on slot {} (write of {} bytes)", s, data.len());
                return -1;
            }
            let free = CAP - p.len;
            if free == 0 {
                0
            } else {
                let n = free.min(data.len() - off);
                for k in 0..n {
                    p.buf[(p.head + p.len + k) % CAP] = data[off + k];
                }
                p.head = (p.head + n) % CAP;
                p.len += n;
                p.wrote += n as u64;
                n
            }
        };
        off += pushed;
        if off >= data.len() {
            return data.len() as i64;
        }
        sched::ksyscall(sched::SYS_SLEEP, TICK_SLEEP_MS, 0, 0);
    }
}

/// Feed a whole buffer through one pipe end in bounded chunks (the
/// SYS_WRITE redirect path: the task's console output becomes the pipe's
/// input). Blocking; -1 on broken pipe.
pub fn feed(h: u64, data: &[u8]) -> i64 {
    let mut off = 0usize;
    while off < data.len() {
        let n = (data.len() - off).min(FEED_CHUNK);
        match write_kernel(h, &data[off..off + n]) {
            -1 => return -1,
            m if m == n as i64 => off += n,
            _ => return -1,
        }
    }
    data.len() as i64
}

/// One pipe byte for the SYS_STDIN_READ file/pipe stdin path. Returns
/// bytes read (1..=out.len()), 0 at EOF, -1 on a bad handle.
pub fn read_kernel(h: u64, out: &mut [u8]) -> i64 {
    if out.is_empty() {
        return 0;
    }
    loop {
        let r = {
            let _g = PIPE_LOCK.lock();
            let Some(s) = slot_of(h) else {
                return -1;
            };
            let p = &mut pipes()[s];
            if !p.used || p.readers == 0 {
                return -1;
            }
            if p.len > 0 {
                let n = p.len.min(out.len());
                let tail = (p.head + CAP - p.len) % CAP;
                for k in 0..n {
                    out[k] = p.buf[(tail + k) % CAP];
                }
                p.len -= n;
                p.head = (p.head + n) % CAP;
                p.got += n as u64;
                n as i64
            } else if p.writers == 0 {
                0 // EOF
            } else {
                -3 // would block
            }
        };
        if r != -3 {
            return r;
        }
        sched::ksyscall(sched::SYS_SLEEP, TICK_SLEEP_MS, 0, 0);
    }
}

/// Snapshot for the shell's `pipe` command. Caller provides the sink;
/// printing happens AFTER the lock is released (the closure only copies).
pub fn for_each(mut f: impl FnMut(usize, usize, usize, u64, u64)) {
    let _g = PIPE_LOCK.lock();
    for (i, p) in pipes().iter().enumerate() {
        if p.used {
            f(i, p.readers as usize, p.writers as usize, p.wrote, p.got);
        }
    }
}
