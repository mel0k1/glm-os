//! v0.9: UDP sockets for ring 3 — bind / sendto / recvfrom / close.
//!
//! Eight global sockets, each a fixed-size datagram queue. This is the
//! first transport that user programs own end to end: a program binds a
//! port, the netd demux (or the loopback fast path) drops datagrams into
//! its queue, and the program pulls them with recvfrom.
//!
//! Blocking follows the established IPC protocol: a syscall that cannot
//! make progress registers the calling task as the socket's waiter and
//! returns -2 (WOULD-BLOCK); the task is marked `BlockedSock` BEFORE the
//! socket lock is released, so a concurrent deliver() can never miss the
//! wakeup. The scheduler keeps the task asleep until a datagram lands.
//!
//! Loopback: sendto() to OUR_IP never touches the NIC or ARP — the
//! datagram is delivered straight into the destination socket's queue.
//! That makes two userland processes on one machine full network peers.
//!
//! Lock order: SOCK_LOCK -> SCHED_LOCK -> SERIAL (same shape as ipc.rs).

use crate::klog;
use crate::mem::paging::cr3;
use crate::mem::vmm::AddressSpace;
use crate::net::{ip_str, OUR_IP};
use crate::sched;
use crate::sync::Spinlock;

use crate::user::uaccess::{read_user_bytes, write_user_bytes};

pub const NSOCK: usize = 8;
pub const DGRAM_MAX: usize = 256; // payload bytes per datagram
pub const QUEUE_DEPTH: usize = 8; // datagrams per socket
const NONE: u16 = 0; // "no waiter" (slot indices are stored +1)

pub const WOULD_BLOCK: i64 = -2;

struct Dgram {
    valid: bool,
    src_ip: u32,
    src_port: u16,
    len: usize,
    data: [u8; DGRAM_MAX],
}

impl Dgram {
    const fn fresh() -> Self {
        Self {
            valid: false,
            src_ip: 0,
            src_port: 0,
            len: 0,
            data: [0; DGRAM_MAX],
        }
    }
}

struct UdpSock {
    used: bool,
    port: u16,
    queue: [Dgram; QUEUE_DEPTH],
    head: usize,  // next enqueue position
    count: usize, // datagrams currently queued
    waiter: u16,  // task slot + 1, single recv waiter
    recv_n: u64,  // datagrams delivered to userland
    drop_n: u64,  // datagrams dropped (queue full / dead socket)
}

impl UdpSock {
    const fn fresh() -> Self {
        Self {
            used: false,
            port: 0,
            queue: [const { Dgram::fresh() }; QUEUE_DEPTH],
            head: 0,
            count: 0,
            waiter: NONE,
            recv_n: 0,
            drop_n: 0,
        }
    }
}

static mut SOCKS: [UdpSock; NSOCK] = [const { UdpSock::fresh() }; NSOCK];
static SOCK_LOCK: Spinlock<()> = Spinlock::new(());

fn socks() -> &'static mut [UdpSock; NSOCK] {
    unsafe { &mut *core::ptr::addr_of_mut!(SOCKS) }
}

/// Bind a fresh socket to `port`. Port 0 is rejected (no ephemerals yet);
/// a port already bound to another socket yields -1 (EADDRINUSE-flavoured).
/// Returns the socket id or -1.
pub fn bind(port: u16) -> i64 {
    if port == 0 {
        return -1;
    }
    let _g = SOCK_LOCK.lock();
    let s = socks();
    for c in s.iter() {
        if c.used && c.port == port {
            return -1;
        }
    }
    for (i, c) in s.iter_mut().enumerate() {
        if !c.used {
            let mut fresh = UdpSock::fresh();
            fresh.used = true;
            fresh.port = port;
            *c = fresh;
            klog!("sock: bound id={} port={}", i, port);
            return i as i64;
        }
    }
    -1
}

/// Close a socket: drop any queued datagrams and free the slot. A parked
/// recvfrom on this socket stays parked (the closing task IS the waiter —
/// signals interrupt the retry loop, which then sees -1 and bails out).
pub fn close(id: u64) -> i64 {
    let Some(id) = sock_index(id) else { return -1 };
    let _g = SOCK_LOCK.lock();
    let s = &mut socks()[id];
    if !s.used {
        return -1;
    }
    let port = s.port;
    *s = UdpSock::fresh();
    klog!("sock: closed id={} port={}", id, port);
    0
}

/// sendto(id, dst, dst_port, payload): send one datagram.
///
///   * dst == OUR_IP  -> loopback: deliver straight into the peer queue
///   * otherwise      -> ARP resolve (may sleep in task context) + NIC TX
///
/// The source port is the sending socket's own bound port, so the peer can
/// reply without learning anything else. Returns the payload length, -1 on
/// error, or WOULD_BLOCK after parking the task (queue full on loopback).
pub fn sendto(id: u64, dst_ip: u32, dst_port: u16, uva: u64, len: u64) -> i64 {
    let Some(id) = sock_index(id) else { return -1 };
    let len = (len as usize).min(DGRAM_MAX);
    if len == 0 {
        return 0;
    }
    // copy-in happens under the caller's own CR3 before touching shared state
    let space = AddressSpace::from_pml4(cr3());
    let mut tmp = [0u8; DGRAM_MAX];
    if read_user_bytes(&space, uva, &mut tmp[..len]).is_err() {
        return -1;
    }

    let src_port;
    {
        let _g = SOCK_LOCK.lock();
        let s = &socks()[id];
        if !s.used {
            return -1;
        }
        src_port = s.port;
    } // lock released: the TX path may sleep (ARP retries)

    if dst_ip == OUR_IP {
        // loopback fast path: no NIC, no ARP
        if deliver(dst_port, dst_ip, src_port, &tmp[..len]) {
            return len as i64;
        }
        // queue full or no listener: park like a full channel? no — UDP
        // semantics are best-effort; report the drop as an error.
        return -1;
    }

    match super::netd::udp_send(dst_ip, dst_port, src_port, &tmp[..len]) {
        true => len as i64,
        false => -1,
    }
}

/// recvfrom(id, buf, len, out_uva): pop one datagram (whole-datagram
/// semantics: a short user buffer truncates and the rest is discarded,
/// like real UDP). When out_uva != 0, six bytes of source metadata
/// ([ip 4][port 2 BE]) are written there so the peer can be answered.
/// Parks the task when the queue is empty (see the module doc).
pub fn recvfrom(id: u64, uva: u64, len: u64, out_uva: u64) -> i64 {
    let Some(id) = sock_index(id) else { return -1 };
    let len = (len as usize).min(DGRAM_MAX);
    if len == 0 {
        return 0;
    }
    let space = AddressSpace::from_pml4(cr3());

    let out_len;
    {
        let _g = SOCK_LOCK.lock();
        let s = &mut socks()[id];
        if !s.used {
            return -1;
        }
        if s.count == 0 {
            s.waiter = sched::current_slot() as u16 + 1;
            sched::mark_blocked_sock();
            sched::request_switch();
            return WOULD_BLOCK;
        }
        // tail = oldest datagram
        let tail = (s.head + QUEUE_DEPTH - s.count) % QUEUE_DEPTH;
        let d = &mut s.queue[tail];
        out_len = d.len.min(len);
        let mut out = [0u8; DGRAM_MAX];
        out[..out_len].copy_from_slice(&d.data[..out_len]);
        let src_ip = d.src_ip;
        let src_port = d.src_port;
        let n_total = d.len;
        let id_port = s.port;
        d.valid = false;
        s.count -= 1;
        s.recv_n += 1;
        drop(_g);
        if write_user_bytes(&space, uva, &out[..out_len]).is_err() {
            return -1; // datagram consumed but undeliverable: report error
        }
        if out_uva != 0 {
            let mut meta = [0u8; 6];
            meta[0..4].copy_from_slice(&src_ip.to_be_bytes());
            meta[4..6].copy_from_slice(&src_port.to_be_bytes());
            let _ = write_user_bytes(&space, out_uva, &meta);
        }
        klog!(
            "sock: recv id={} port={} udp from {}:{} ({} bytes)",
            id,
            id_port,
            ip_str(src_ip),
            src_port,
            n_total
        );
        return out_len as i64;
    }
}

fn sock_index(id: u64) -> Option<usize> {
    let i = id as usize;
    if i < NSOCK {
        Some(i)
    } else {
        None
    }
}

/// Deliver one datagram into the socket bound to `dst_port` (netd demux
/// and the loopback fast path both funnel through here).
/// Returns false when nobody is listening or the queue is full.
pub fn deliver(dst_port: u16, src_ip: u32, src_port: u16, payload: &[u8]) -> bool {
    let n = payload.len().min(DGRAM_MAX);
    let waiter;
    {
        let _g = SOCK_LOCK.lock();
        let s = socks();
        let Some(slot) = s.iter_mut().find(|c| c.used && c.port == dst_port) else {
            // no listener: count against the closest match? nothing to do
            klog!("sock: udp to port {} dropped (no listener)", dst_port);
            return false;
        };
        if slot.count == QUEUE_DEPTH {
            slot.drop_n += 1;
            klog!("sock: udp to port {} dropped (queue full)", dst_port);
            return false;
        }
        let d = &mut slot.queue[slot.head];
        d.valid = true;
        d.src_ip = src_ip;
        d.src_port = src_port;
        d.len = n;
        d.data[..n].copy_from_slice(&payload[..n]);
        slot.head = (slot.head + 1) % QUEUE_DEPTH;
        slot.count += 1;
        waiter = slot.waiter;
        slot.waiter = NONE;
        klog!(
            "sock: queued {} bytes -> port {} (from {}:{})",
            n,
            dst_port,
            ip_str(src_ip),
            src_port
        );
    }
    // wake outside the lock; state flip already happened under SOCK_LOCK,
    // so the wakeup is un-loseable (same argument as ipc.rs)
    sched::wake_sock_waiter(waiter);
    true
}

/// Snapshot for the shell's `net` command.
pub fn for_each(mut f: impl FnMut(usize, u16, usize, u64, u64, bool)) {
    let _g = SOCK_LOCK.lock();
    for (i, c) in socks().iter().enumerate() {
        if c.used {
            f(i, c.port, c.count, c.recv_n, c.drop_n, c.waiter != NONE);
        }
    }
}
