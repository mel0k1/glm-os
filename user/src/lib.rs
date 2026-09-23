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
// --- v1.2: ring-3 GUI (windows on the kernel desktop) -----------------------------
pub const SYS_GUI_OPEN: u64 = 24;
pub const SYS_GUI_CLOSE: u64 = 25;
pub const SYS_GUI_RECT: u64 = 26;
pub const SYS_GUI_TEXT: u64 = 27;
pub const SYS_GUI_EVENT: u64 = 28;
pub const SYS_GUI_GEO: u64 = 29;

// v1.2: packed GUI input events (kernel gui.rs is the source of truth)
pub const EV_NONE: u64 = 0;
pub const EV_CLOSE: u64 = 1;
pub const EV_CLICK: u64 = 2;
pub const EV_KEY: u64 = 3;
pub const EV_RESIZE: u64 = 4;

/// Split a packed event into (type, arg_a, arg_b).
pub fn gui_ev_parts(ev: u64) -> (u64, u64, u64) {
    ((ev >> 32) & 0xFFFF, (ev >> 16) & 0xFFFF, ev & 0xFFFF)
}

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

// --- v1.2: ring-3 GUI window API --------------------------------------------------
//
// A window is a rectangle on the kernel desktop with a per-window backing
// store. All coordinates passed to rect/text are WINDOW-LOCAL (origin at
// the top-left of the content area, under the 22px title bar). The kernel
// composites the window, handles dragging/minimize/z-order and queues
// input events that the app polls with gui_event().

/// Open a window (SYS_GUI_OPEN). Pass x == GUI_AUTO or y == GUI_AUTO
/// (-1) for automatic (cascade) placement. Returns the window id or -1.
pub const GUI_AUTO: i32 = -1;

pub fn gui_open(title: &str, x: i32, y: i32, w: i32, h: i32) -> i64 {
    let xy = ((x as i16 as u16) as u64) | (((y as i16 as u16) as u64) << 16);
    let wh = ((w as u16) as u64) | (((h as u16) as u64) << 16);
    syscall4(
        SYS_GUI_OPEN,
        title.as_ptr() as u64,
        title.len() as u64,
        xy,
        wh,
    ) as i64
}

/// Close the window owned by this task (SYS_GUI_CLOSE).
pub fn gui_close(id: i64) -> i64 {
    syscall1(SYS_GUI_CLOSE, id as u64) as i64
}

/// Fill a rect with 0xRRGGBB in window-local coordinates (SYS_GUI_RECT).
/// Returns 0, or -1 if the window is gone or resized (repaint after EV_RESIZE).
pub fn gui_rect(id: i64, x: i32, y: i32, w: i32, h: i32, rgb: u32) -> i64 {
    let xy = ((x as i16 as u16) as u64) | (((y as i16 as u16) as u64) << 16);
    let wh = ((w as u16) as u64) | (((h as u16) as u64) << 16);
    syscall4(SYS_GUI_RECT, id as u64, xy, wh, rgb as u64) as i64
}

/// Draw an ASCII string with 0xRRGGBB (SYS_GUI_TEXT). Returns the length
/// drawn, or -1 if the window is gone or resized.
pub fn gui_text(id: i64, x: i32, y: i32, s: &str, rgb: u32) -> i64 {
    let xy = ((x as i16 as u16) as u64) | (((y as i16 as u16) as u64) << 16);
    syscall5(
        SYS_GUI_TEXT,
        id as u64,
        xy,
        s.as_ptr() as u64,
        s.len() as u64,
        rgb as u64,
    ) as i64
}

/// Poll one input event (SYS_GUI_EVENT): EV_CLOSE / EV_CLICK(x,y) /
/// EV_KEY(char) / EV_RESIZE(w,h). 0 = queue empty. -1 as u64 (all ones)
/// means the window (or the whole desktop) is gone.
pub fn gui_event(id: i64) -> u64 {
    syscall1(SYS_GUI_EVENT, id as u64)
}

/// Current window size as (w, h), or None once the window is gone.
pub fn gui_geo(id: i64) -> Option<(i32, i32)> {
    let r = syscall1(SYS_GUI_GEO, id as u64) as i64;
    if r < 0 {
        None
    } else {
        Some(((r >> 16) as i32, (r & 0xFFFF) as i32))
    }
}

// --- v1.3: ring-3 TCP streams ------------------------------------------------------
//
// Same int 0x80 gate, numbers 30-35. Blocking calls (connect/accept/recv)
// sleep inside the kernel in the calling task's context — the caller just
// blocks like on any real OS. recv() returns 0 at EOF (peer closed and
// drained), -1 on error.

/// Active open (SYS_TCP_CONNECT). Blocks until ESTAB; returns the socket
/// id or -1 (timeout 8 s, refused, no route).
pub fn tcp_connect(ip: u32, port: u16) -> i64 {
    syscall2(30, ip as u64, port as u64) as i64
}

/// Passive open (SYS_TCP_LISTEN): bind a listener to `port`.
pub fn tcp_listen(port: u16) -> i64 {
    syscall1(31, port as u64) as i64
}

/// Accept one connection (SYS_TCP_ACCEPT). Blocks; returns the NEW
/// connection socket's id (the listener keeps listening).
pub fn tcp_accept(lid: i64) -> i64 {
    syscall1(32, lid as u64) as i64
}

/// Send up to 1400 bytes (SYS_TCP_SEND). Returns the byte count or -1.
pub fn tcp_send(id: i64, buf: &[u8]) -> i64 {
    syscall3(33, id as u64, buf.as_ptr() as u64, buf.len() as u64) as i64
}

/// Receive (SYS_TCP_RECV). Blocks; returns bytes read, 0 = EOF, -1 = err.
pub fn tcp_recv(id: i64, buf: &mut [u8]) -> i64 {
    syscall3(34, id as u64, buf.as_mut_ptr() as u64, buf.len() as u64) as i64
}

/// Close the connection (SYS_TCP_CLOSE): FIN once, slot freed.
pub fn tcp_close(id: i64) -> i64 {
    syscall1(35, id as u64) as i64
}

// --- v1.6: files on the persistent disk --------------------------------------------
//
// Numbers 36-42 over the same int 0x80 gate. The kernel keeps a small
// open-file table (16 slots) with whole-file buffers: open() loads the
// current contents, close() flushes dirty bytes back to the FAT32 disk.
// A process exit closes (and flushes) everything it left open.
//
// Flags (mirror of src/fs/sysfile.rs):
pub const O_CREATE: u64 = 1; // create-or-truncate
pub const O_RDWR: u64 = 2; // load existing contents, patch in place
pub const O_APPEND: u64 = 4; // open-or-create, cursor at end

pub const SYS_FILE_OPEN: u64 = 36;
pub const SYS_FILE_READ: u64 = 37;
pub const SYS_FILE_WRITE: u64 = 38;
pub const SYS_FILE_CLOSE: u64 = 39;
pub const SYS_FILE_SEEK: u64 = 40;
pub const SYS_FILE_UNLINK: u64 = 41;
pub const SYS_FILE_LIST: u64 = 42;

/// Open a file in the disk root (SYS_FILE_OPEN). Names are bare file
/// names ("NOTES.TXT") -- the kernel prefixes '/'. Returns the fd or -1.
pub fn file_open(name: &str, flags: u64) -> i64 {
    syscall3(
        SYS_FILE_OPEN,
        name.as_ptr() as u64,
        name.len() as u64,
        flags,
    ) as i64
}

/// Read at the cursor (SYS_FILE_READ). Returns bytes read, 0 = EOF.
pub fn file_read(fd: i64, buf: &mut [u8]) -> i64 {
    syscall3(
        SYS_FILE_READ,
        fd as u64,
        buf.as_mut_ptr() as u64,
        buf.len() as u64,
    ) as i64
}

/// Write at the cursor (SYS_FILE_WRITE). Returns bytes written; the data
/// becomes durable at close() (or process exit).
pub fn file_write(fd: i64, buf: &[u8]) -> i64 {
    syscall3(
        SYS_FILE_WRITE,
        fd as u64,
        buf.as_ptr() as u64,
        buf.len() as u64,
    ) as i64
}

/// Close (SYS_FILE_CLOSE): flushes a dirty file to the disk. 0 = ok.
pub fn file_close(fd: i64) -> i64 {
    syscall1(SYS_FILE_CLOSE, fd as u64) as i64
}

/// Move the cursor (SYS_FILE_SEEK): whence 0 = SET, 1 = CUR, 2 = END.
/// Returns the new absolute position or -1.
pub fn file_seek(fd: i64, off: i64, whence: u64) -> i64 {
    syscall3(SYS_FILE_SEEK, fd as u64, off as u64, whence) as i64
}

/// Delete a file (SYS_FILE_UNLINK). Refuses files that are open. 0 = ok.
pub fn file_unlink(name: &str) -> i64 {
    syscall2(SYS_FILE_UNLINK, name.as_ptr() as u64, name.len() as u64) as i64
}

/// List the disk root (SYS_FILE_LIST) into a packed byte buffer, at most
/// `max_entries` records. Returns the record count or -1. Walk the
/// records with file_record().
pub fn file_list(buf: &mut [u8], max_entries: usize) -> i64 {
    syscall2(SYS_FILE_LIST, buf.as_mut_ptr() as u64, max_entries as u64) as i64
}

/// Decode one packed directory record at `off`:
///   [u8 kind (0 file, 1 dir)][u8 name_len][name][u32 size LE]
/// Returns (kind, name bytes, size, next offset), or None at the end.
pub fn file_record(buf: &[u8], off: usize) -> Option<(u8, &[u8], u32, usize)> {
    if off + 2 > buf.len() {
        return None;
    }
    let kind = buf[off];
    let n = buf[off + 1] as usize;
    if n == 0 || off + 2 + n + 4 > buf.len() {
        return None;
    }
    let name = &buf[off + 2..off + 2 + n];
    let mut size = [0u8; 4];
    size.copy_from_slice(&buf[off + 2 + n..off + 2 + n + 4]);
    Some((kind, name, u32::from_le_bytes(size), off + 2 + n + 4))
}

// --- v1.7: exec — become another program ---------------------------------------
//
// SYS_EXEC (43) replaces the CALLING process image. On success the call
// NEVER RETURNS (the process is now `path`, starting at its _start with
// rdi=argc, rsi=argv). On failure returns -1 and the caller continues.
// Compose with fork()/wait() for the classic Unix launcher:
//
//     let pid = fork();
//     if pid == 0 {
//         if exec("/BIN/ARGS.ELF", &["ARGS.ELF", "hi"]) == -1 { exit(127); }
//     }
//     let code = wait(pid as u64);

pub const SYS_EXEC: u64 = 43;

/// exec(path, args) over int 0x80 #43. `args` must be non-empty; by
/// convention args[0] is the program name (pass `path` if unsure).
/// argv is packed as [u32 argc][u32 len][bytes]... — the same layout the
/// kernel parses in src/user/exec.rs.
pub fn exec(path: &str, args: &[&str]) -> i64 {
    let mut block = [0u8; 2060]; // 4 + 16 * (4 + 128) worst case
    if args.is_empty() || args.len() > 16 {
        return -1;
    }
    block[0..4].copy_from_slice(&(args.len() as u32).to_le_bytes());
    let mut off = 4;
    for a in args {
        if a.len() > 128 {
            return -1;
        }
        block[off..off + 4].copy_from_slice(&(a.len() as u32).to_le_bytes());
        off += 4;
        block[off..off + a.len()].copy_from_slice(a.as_bytes());
        off += a.len();
    }
    syscall4(
        SYS_EXEC,
        path.as_ptr() as u64,
        path.len() as u64,
        block.as_ptr() as u64,
        off as u64,
    ) as i64
}

/// Copy the NUL-terminated string at `p` into `buf` (no alloc). Returns
/// the bytes WITHOUT the NUL; an overlong string is truncated.
///
/// SAFETY: `p` must point to a NUL-terminated byte string in this
/// address space (e.g. an element of the argv array given to _start).
pub fn cstr_into(p: *const u8, buf: &mut [u8]) -> &[u8] {
    let mut i = 0usize;
    unsafe {
        while i < buf.len() {
            let c = *p.add(i);
            if c == 0 {
                break;
            }
            buf[i] = c;
            i += 1;
        }
    }
    &buf[..i]
}

/// Turn a copied argv byte slice into a &str (lossy-safe: argv produced
/// by the kernel-side writers is always valid UTF-8 ASCII).
pub fn arg_str(bytes: &[u8]) -> &str {
    core::str::from_utf8(bytes).unwrap_or("?")
}
