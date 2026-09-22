//! glm-user: freestanding syscall library for GLM OS ring-3 programs.
//!
//! ABI: `int 0x80`, number in rax, args in rdi/rsi/rdx, result in rax.
//! The kernel preserves every GP register except rax across the gate.

#![no_std]

pub const SYS_WRITE: u64 = 0;
pub const SYS_READCHAR: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_UPTIME: u64 = 3;
pub const SYS_GETPID: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_SLEEP: u64 = 6;
pub const SYS_WAIT: u64 = 7;
pub const SYS_KILL: u64 = 8;
pub const SYS_SIGACTION: u64 = 9;
pub const SYS_SIGRETURN: u64 = 10;
pub const SYS_CHAN_OPEN: u64 = 11;
pub const SYS_CHAN_SEND: u64 = 12;
pub const SYS_CHAN_RECV: u64 = 13;
// --- v0.6: copy-on-write fork -------------------------------------------------
pub const SYS_FORK: u64 = 14;
// --- v0.7: threads -------------------------------------------------------------
pub const SYS_CLONE: u64 = 15;
pub const SYS_TEXIT: u64 = 16;
pub const SYS_JOIN: u64 = 17;
pub const SYS_SET_FS: u64 = 18;
// --- v0.9: userland UDP sockets --------------------------------------------------
pub const SYS_NET_BIND: u64 = 19;
pub const SYS_NET_SENDTO: u64 = 20;
pub const SYS_NET_RECVFROM: u64 = 21;
pub const SYS_NET_CLOSE: u64 = 22;
pub const SYS_NET_INFO: u64 = 23;

// signal numbers (mirror of the kernel table)
pub const SIGKILL: u64 = 9;
pub const SIGUSR1: u64 = 10;
pub const SIGUSR2: u64 = 12;
pub const SIGTERM: u64 = 15;

/// Give the CPU back to the scheduler (SYS_YIELD).
pub fn yield_now() {
    syscall0(SYS_YIELD);
}

/// Sleep for `ms` milliseconds (SYS_SLEEP) — other tasks run meanwhile.
pub fn sleep_ms(ms: u64) {
    syscall1(SYS_SLEEP, ms);
}

#[inline]
pub fn syscall0(n: u64) -> u64 {
    let ret: u64;
    unsafe { core::arch::asm!("int 0x80", inlateout("rax") n => ret, options(nostack)) };
    ret
}

#[inline]
pub fn syscall1(n: u64, a1: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            options(nostack)
        )
    };
    ret
}

#[inline]
pub fn syscall2(n: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            options(nostack)
        )
    };
    ret
}

#[inline]
pub fn syscall3(n: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            options(nostack)
        )
    };
    ret
}

/// Print a string on the kernel console (SYS_WRITE).
pub fn write(s: &str) {
    syscall2(SYS_WRITE, s.as_ptr() as u64, s.len() as u64);
}

/// Terminate the task (SYS_EXIT). Never returns.
pub fn exit(code: i64) -> ! {
    syscall1(SYS_EXIT, code as u64);
    loop {
        core::hint::spin_loop();
    }
}

/// Milliseconds since boot (SYS_UPTIME).
pub fn uptime_ms() -> u64 {
    syscall0(SYS_UPTIME)
}

/// Current task id (SYS_GETPID).
pub fn getpid() -> u64 {
    syscall0(SYS_GETPID)
}

// --- v0.6: fork ----------------------------------------------------------------

/// Copy-on-write fork (SYS_FORK). Returns:
///   *  0  in the child — it resumes from the same point with its own
///         private copy of every page it modifies;
///   * > 0 in the parent — the child's pid;
///   * -1  on failure (no free slot, out of frames or kernel task).
pub fn fork() -> i64 {
    syscall0(SYS_FORK) as i64
}

/// Wait for a child to exit (SYS_WAIT). `target` = child pid or 0 for any.
/// Returns the child's exit code; blocks the caller until it arrives.
pub fn wait(target: u64) -> i64 {
    syscall1(SYS_WAIT, target) as i64
}

// --- v0.7: threads ------------------------------------------------------------

/// Spawn a thread in the SAME address space (SYS_CLONE).
///
/// `entry` receives `arg` in its first argument and must never return —
/// it has to finish with `thread_exit(code)`. `stack_top` is the TOP of a
/// stack the CALLER owns (a static buffer, an array, ...); pass
/// `base + size - 8` so the function ABI sees rsp % 16 == 8 at entry.
///
/// Returns the new thread's tid (> 0), or -1 on failure.
pub fn clone(entry: u64, stack_top: u64, arg: u64) -> i64 {
    syscall3(SYS_CLONE, entry, stack_top, arg) as i64
}

/// Terminate the CALLING thread only (SYS_TEXIT). The rest of the process
/// keeps running; a joiner receives `code`. Never returns.
pub fn thread_exit(code: i64) -> ! {
    syscall1(SYS_TEXIT, code as u64);
    loop {
        core::hint::spin_loop();
    }
}

/// Wait until sibling thread `tid` exits and get its exit code (SYS_JOIN).
/// `tid = 0` joins any sibling; -1 means the tid is unknown/already joined.
pub fn join(tid: u64) -> i64 {
    syscall1(SYS_JOIN, tid) as i64
}

/// Install a user TLS block for the CALLING thread (SYS_SET_FS): the
/// kernel writes FS.BASE so FS-relative addressing reaches `base`.
pub fn set_fs(base: u64) -> i64 {
    syscall1(SYS_SET_FS, base) as i64
}

// --- v0.5: signals + ipc channels --------------------------------------------

/// Install a signal handler. Returns the previous handler (0 = default).
pub fn sigaction(sig: u64, handler: extern "C" fn(u64)) -> i64 {
    syscall2(SYS_SIGACTION, sig, handler as usize as u64) as i64
}

/// Send a signal to another task. 0 on success.
pub fn kill(pid: u64, sig: u64) -> i64 {
    syscall2(SYS_KILL, pid, sig) as i64
}

/// Open (or join by rendezvous key) a channel. Returns the channel id.
pub fn chan_open(key: u64) -> i64 {
    syscall1(SYS_CHAN_OPEN, key) as i64
}

/// Blocking send: the kernel returns -2 (would-block) and parks us until a
/// peer recv()s; retry transparently so callers see blocking semantics.
pub fn chan_send(id: u64, buf: &[u8]) -> i64 {
    loop {
        let r = syscall3(SYS_CHAN_SEND, id, buf.as_ptr() as u64, buf.len() as u64) as i64;
        if r != -2 {
            return r;
        }
        yield_now();
    }
}

/// Blocking recv: same retry protocol on the kernel's -2 sentinel.
pub fn chan_recv(id: u64, buf: &mut [u8]) -> i64 {
    loop {
        let r = syscall3(SYS_CHAN_RECV, id, buf.as_mut_ptr() as u64, buf.len() as u64) as i64;
        if r != -2 {
            return r;
        }
        yield_now();
    }
}

/// Format an unsigned integer as decimal ASCII into `buf`,
/// returning the digits slice.
pub fn fmt_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}

// --- v0.9: userland UDP sockets ------------------------------------------------

#[inline]
fn syscall4(n: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("rcx") a4,
            options(nostack)
        )
    };
    ret
}

#[inline]
fn syscall5(n: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("rcx") a4,
            in("r8") a5,
            options(nostack)
        )
    };
    ret
}

/// Bind a UDP socket to `port` (SYS_NET_BIND). Returns the socket id or -1
/// (port taken, port 0, table full).
pub fn net_bind(port: u16) -> i64 {
    syscall1(SYS_NET_BIND, port as u64) as i64
}

/// Send one UDP datagram (SYS_NET_SENDTO). `ip` is big-endian packed
/// (a<<24 | b<<16 | c<<8 | d). Returns the payload length or -1. A send to
/// the machine's own address takes the kernel loopback fast path.
pub fn net_sendto(id: i64, ip: u32, port: u16, buf: &[u8]) -> i64 {
    syscall5(
        SYS_NET_SENDTO,
        id as u64,
        ip as u64,
        port as u64,
        buf.as_ptr() as u64,
        buf.len() as u64,
    ) as i64
}

/// Source address written by the kernel on recvfrom: [ip 4 BE][port 2 BE].
#[repr(C)]
pub struct SrcAddr {
    pub bytes: [u8; 6],
}

impl SrcAddr {
    pub const fn new() -> Self {
        Self { bytes: [0; 6] }
    }
    /// Packed big-endian IPv4 (a<<24 | b<<16 | c<<8 | d).
    pub fn ip(&self) -> u32 {
        u32::from_be_bytes([self.bytes[0], self.bytes[1], self.bytes[2], self.bytes[3]])
    }
    pub fn port(&self) -> u16 {
        u16::from_be_bytes([self.bytes[4], self.bytes[5]])
    }
}

/// Receive one UDP datagram (SYS_NET_RECVFROM). Blocks: the kernel parks
/// the task until a datagram lands in the socket queue; the -2 sentinel
/// is retried transparently. Returns the payload length or -1 (closed);
/// `src` gets the sender's ip:port so the peer can be answered.
pub fn net_recvfrom(id: i64, buf: &mut [u8], src: &mut SrcAddr) -> i64 {
    loop {
        let r = syscall4(
            SYS_NET_RECVFROM,
            id as u64,
            buf.as_mut_ptr() as u64,
            buf.len() as u64,
            src as *mut SrcAddr as u64,
        ) as i64;
        if r != -2 {
            return r;
        }
        yield_now();
    }
}

/// Close a socket (SYS_NET_CLOSE). Frees the port; queued datagrams die.
pub fn net_close(id: i64) -> i64 {
    syscall1(SYS_NET_CLOSE, id as u64) as i64
}

/// Kernel network facts (SYS_NET_INFO): what 0 = our IPv4 (packed BE),
/// 1 = the default gateway.
pub fn net_info(what: u64) -> i64 {
    syscall1(SYS_NET_INFO, what) as i64
}
