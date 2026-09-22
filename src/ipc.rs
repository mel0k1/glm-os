//! Named byte-stream channels: the GLM OS IPC primitive (v0.5).
//!
//! Eight global channels, each a 256-byte ring buffer with blocking-ish
//! send/recv semantics. "Blocking" in this kernel works by cooperative
//! de-scheduling: a syscall that cannot make progress registers itself as
//! the channel's waiter and returns -2 (WOULD-BLOCK) to ring 3; the task
//! is marked `BlockedChan` BEFORE the channel lock is released, so a peer
//! on another CPU can never miss the wakeup. The scheduler keeps the task
//! asleep until the peer completes an opposite operation, and userland
//! (glm-user) transparently retries the syscall.
//!
//! Lock order: IPC_LOCK -> SCHED_LOCK. post_dispatch only ever takes
//! SCHED_LOCK, and channel ops never run under it, so the order is global.

use crate::mem::paging::cr3;
use crate::mem::vmm::AddressSpace;
use crate::sched;
use crate::sync::Spinlock;

use crate::user::uaccess::{read_user_bytes, write_user_bytes};

pub const NCHANS: usize = 8;
pub const CAP: usize = 256; // ring capacity in bytes
pub const MAXMSG: usize = 64; // one send() carries at most this many bytes
const NONE: u16 = 0; // "no waiter" (slot indices are stored +1)

pub const WOULD_BLOCK: i64 = -2;

struct Chan {
    used: bool,
    key: u64, // rendezvous key (0 = private); open(key) joins an existing one
    buf: [u8; CAP],
    head: usize, // next write position
    count: usize,
    wait_recv: u16, // task slot + 1, single waiter per side
    wait_send: u16,
    sent: u64,
    got: u64,
}

impl Chan {
    const fn fresh() -> Self {
        Self {
            used: false,
            key: 0,
            buf: [0; CAP],
            head: 0,
            count: 0,
            wait_recv: NONE,
            wait_send: NONE,
            sent: 0,
            got: 0,
        }
    }
}

static mut CHANS: [Chan; NCHANS] = [const { Chan::fresh() }; NCHANS];
static IPC_LOCK: Spinlock<()> = Spinlock::new(());

fn chans() -> &'static mut [Chan; NCHANS] {
    unsafe { &mut *core::ptr::addr_of_mut!(CHANS) }
}

/// Open (or join, when key != 0) a channel; returns its id or -1.
pub fn open(key: u64) -> i64 {
    let _g = IPC_LOCK.lock();
    let ch = chans();
    if key != 0 {
        for (i, c) in ch.iter().enumerate() {
            if c.used && c.key == key {
                return i as i64;
            }
        }
    }
    for (i, c) in ch.iter_mut().enumerate() {
        if !c.used {
            let mut fresh = Chan::fresh();
            fresh.used = true;
            fresh.key = key;
            *c = fresh;
            return i as i64;
        }
    }
    -1
}

/// send(id, buf, len): enqueue up to MAXMSG bytes. Returns len, -1 on
/// error, or WOULD_BLOCK after parking the task (userland retries).
pub fn send(id: u64, uva: u64, len: u64) -> i64 {
    let Some(id) = chan_index(id) else { return -1 };
    let len = (len as usize).min(MAXMSG);
    if len == 0 {
        return 0;
    }
    // copy-in happens under the caller's own CR3 before touching shared state
    let space = AddressSpace::from_pml4(cr3());
    let mut tmp = [0u8; MAXMSG];
    if read_user_bytes(&space, uva, &mut tmp[..len]).is_err() {
        return -1;
    }

    {
        let _g = IPC_LOCK.lock();
        let c = &mut chans()[id];
        if !c.used {
            return -1;
        }
        if c.count + len <= CAP {
            for k in 0..len {
                c.buf[(c.head + k) % CAP] = tmp[k];
            }
            c.head = (c.head + len) % CAP;
            c.count += len;
            c.sent += 1;
            let w = c.wait_recv;
            c.wait_recv = NONE;
            drop(_g);
            sched::wake_chan_waiter(w);
            return len as i64;
        }
        // full: register as the sender-waiter, then park (state is set
        // under IPC_LOCK, so no peer can slip a wakeup in front of us)
        c.wait_send = sched::current_slot() as u16 + 1;
        sched::mark_blocked_chan();
    }
    sched::request_switch();
    WOULD_BLOCK
}

/// recv(id, buf, len): drain up to len bytes. Same parking protocol.
pub fn recv(id: u64, uva: u64, len: u64) -> i64 {
    let Some(id) = chan_index(id) else { return -1 };
    let len = (len as usize).min(MAXMSG);
    if len == 0 {
        return 0;
    }
    let space = AddressSpace::from_pml4(cr3());

    let n;
    {
        let _g = IPC_LOCK.lock();
        let c = &mut chans()[id];
        if !c.used {
            return -1;
        }
        if c.count == 0 {
            c.wait_recv = sched::current_slot() as u16 + 1;
            sched::mark_blocked_chan();
            sched::request_switch();
            return WOULD_BLOCK;
        }
        n = c.count.min(len);
        // copy-out through the ring (tail = head - count)
        let tail = (c.head + CAP - c.count) % CAP;
        let mut out = [0u8; MAXMSG];
        for k in 0..n {
            out[k] = c.buf[(tail + k) % CAP];
        }
        c.count -= n;
        c.got += n as u64;
        if write_user_bytes(&space, uva, &out[..n]).is_err() {
            return -1; // bytes consumed but undeliverable: report error
        }
        let w = c.wait_send;
        c.wait_send = NONE;
        drop(_g);
        sched::wake_chan_waiter(w);
        return n as i64;
    }
}

/// Snapshot for the shell's `ipc` command. Caller provides the sink.
pub fn for_each(mut f: impl FnMut(usize, u64, usize, u64, u64, bool, bool)) {
    let _g = IPC_LOCK.lock();
    for (i, c) in chans().iter().enumerate() {
        if c.used {
            f(
                i,
                c.key,
                c.count,
                c.sent,
                c.got,
                c.wait_send != NONE,
                c.wait_recv != NONE,
            );
        }
    }
}

fn chan_index(id: u64) -> Option<usize> {
    let i = id as usize;
    if i < NCHANS {
        Some(i)
    } else {
        None
    }
}
