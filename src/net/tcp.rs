//! v1.3: TCP for ring 3 — a small, honest stream transport.
//!
//! Sockets live in a fixed table guarded by TCP_LOCK (netd ingress and
//! user syscalls both touch it; the ISR never does — netd is the only RX
//! consumer, same shape as UDP). What is implemented:
//!
//!   * passive open: listen(port) -> SYN -> SYN|ACK -> ACK; the completed
//!     connection becomes a fresh socket that accept() hands out while the
//!     listener keeps listening (one pending connection at a time);
//!   * active open: connect(ip, port) -> SYN -> SYN|ACK -> ACK, 1 s SYN
//!     retransmit, 8 s give-up;
//!   * data: in-order byte stream, immediate ACKs; out-of-order segments
//!     are dropped and re-ACKed at rcv_nxt (the peer retransmits — we
//!     never reorder); a full RX ring accepts what fits and ACKs only
//!     that, so the peer resends the tail;
//!   * retransmission: the TX ring holds unacked bytes; netd's tick
//!     resends the oldest 1400 bytes every 400 ms (8 retries -> reset);
//!   * close: FIN once, then the slot is freed (no TIME_WAIT — fine at
//!     this scale); inbound FIN moves to CloseWait, recv() reports EOF.
//!
//! Blocking (connect/accept/recv) spins in the CALLING TASK's context via
//! SYS_SLEEP(5 ms) — never with a lock held, never in the ISR. No -2
//! sentinels, no waiters: the syscall simply does not return until there
//! is an answer (the scheduler keeps everything else running).
//!
//! Lock order: TCP_LOCK -> NET_LOCK (send path) -> SERIAL; every send
//! happens AFTER the guard is dropped (ARP resolution inside send_seg
//! sleeps). Never the reverse.

use crate::klog;
use crate::net::e1000;
use crate::net::netd::arp_resolve;
use crate::net::proto::*;
use crate::net::{ip_str, OUR_IP};
use crate::sync::Spinlock;

pub const NSOCK: usize = 8;
pub const MSS: usize = 1400;
pub const TX_CAP: usize = 8192;
pub const RX_CAP: usize = 8192;
const RCV_WIN: u16 = RX_CAP as u16;
const TICK_SLEEP_MS: u64 = 5;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum St {
    Free,
    Listen,
    SynSent,
    SynRcvd,
    Estab,
    CloseWait, // peer closed; app drains + closes
    FinWait,   // we sent FIN; waiting for the final ACK
}

impl St {
    pub fn as_str(self) -> &'static str {
        match self {
            St::Free => "FREE",
            St::Listen => "LISTEN",
            St::SynSent => "SYN-SENT",
            St::SynRcvd => "SYN-RCVD",
            St::Estab => "ESTAB",
            St::CloseWait => "CLOSE-WAIT",
            St::FinWait => "FIN-WAIT",
        }
    }
}

struct TcpSock {
    used: bool,
    st: St,
    local_port: u16,
    peer_ip: u32,
    peer_port: u16,
    // sequence tracking
    iss: u32,
    snd_una: u32, // oldest unacked byte
    snd_nxt: u32, // next byte to send
    rcv_nxt: u32, // next expected byte from the peer
    // unacked TX ring (linear buffer with head/len)
    tx: [u8; TX_CAP],
    tx_head: usize,
    tx_len: usize,
    // received RX ring
    rx: [u8; RX_CAP],
    rx_head: usize,
    rx_len: usize,
    rx_eof: bool, // peer FIN seen: recv returns 0 once the ring drains
    // timers / retries
    last_tx_us: u64,
    retries: u32,
    snd_n: u64,
    rcv_n: u64,
    // listener state: the pending connection's fresh socket id (0 = none)
    pend_id: u32,
}

impl TcpSock {
    const fn fresh() -> Self {
        Self {
            used: false,
            st: St::Free,
            local_port: 0,
            peer_ip: 0,
            peer_port: 0,
            iss: 0,
            snd_una: 0,
            snd_nxt: 0,
            rcv_nxt: 0,
            tx: [0; TX_CAP],
            tx_head: 0,
            tx_len: 0,
            rx: [0; RX_CAP],
            rx_head: 0,
            rx_len: 0,
            rx_eof: false,
            last_tx_us: 0,
            retries: 0,
            snd_n: 0,
            rcv_n: 0,
            pend_id: 0,
        }
    }
}

static mut SOCKS: [TcpSock; NSOCK] = [const { TcpSock::fresh() }; NSOCK];
static TCP_LOCK: Spinlock<()> = Spinlock::new(());
static EPHEMERAL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(20_000);
static ISS_NEXT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0x474C_4D00);

fn socks() -> &'static mut [TcpSock; NSOCK] {
    unsafe { &mut *core::ptr::addr_of_mut!(SOCKS) }
}

fn now_us() -> u64 {
    crate::net::netd::now_us()
}

fn sleep_tick() {
    crate::sched::ksyscall(crate::sched::SYS_SLEEP, TICK_SLEEP_MS, 0, 0);
}

// --- ring helpers (caller holds TCP_LOCK) ---

fn ring_push(r: &mut [u8], head: &mut usize, len: &mut usize, data: &[u8]) -> usize {
    let n = data.len().min(r.len() - *len);
    for i in 0..n {
        r[(*head + *len + i) % r.len()] = data[i];
    }
    *len += n;
    n
}

fn ring_pop(r: &[u8], head: &mut usize, len: &mut usize, out: &mut [u8]) -> usize {
    let n = out.len().min(*len);
    for i in 0..n {
        out[i] = r[(*head + i) % r.len()];
    }
    *head = (*head + n) % r.len();
    *len -= n;
    n
}

fn ring_drop(head: &mut usize, len: &mut usize, cap: usize, n: usize) {
    let n = n.min(*len);
    *head = (*head + n) % cap;
    *len -= n;
}

// --- segment TX (task context only: ARP may sleep) ---

#[allow(clippy::too_many_arguments)]
fn send_seg(
    dst_ip: u32,
    dst_port: u16,
    local_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    payload: &[u8],
) -> bool {
    if !e1000::online() {
        return false;
    }
    let Some(mac) = arp_resolve(dst_ip) else { return false };
    let mut frame = [0u8; ETH_HDR + IPV4_HDR_MIN + TCP_HDR_MIN + MSS];
    let our = crate::net::netd::our_mac();
    let n = eth_put(&mut frame, &mac, &our, ETHERTYPE_IPV4);
    let ihl = ipv4_put(
        &mut frame[n..],
        PROTO_TCP,
        OUR_IP,
        dst_ip,
        TCP_HDR_MIN + payload.len(),
    );
    let tl = tcp_put(
        &mut frame[n + ihl..],
        OUR_IP,
        dst_ip,
        local_port,
        dst_port,
        seq,
        ack,
        flags,
        RCV_WIN,
        payload,
    );
    e1000::send_frame(&frame[..n + ihl + tl])
}

// --- public API (syscalls; blocking happens in the caller's context) ---

/// tcp_listen(port): allocate a socket in Listen. -1 on error.
pub fn listen(port: u16) -> i64 {
    if port == 0 {
        return -1;
    }
    let _g = TCP_LOCK.lock();
    let s = socks();
    for c in s.iter() {
        if c.used && c.local_port == port {
            return -1; // port already taken (tcp side)
        }
    }
    for (i, c) in s.iter_mut().enumerate() {
        if !c.used {
            let mut fresh = TcpSock::fresh();
            fresh.used = true;
            fresh.st = St::Listen;
            fresh.local_port = port;
            *c = fresh;
            klog!("tcp: listen id={} port={}", i, port);
            return i as i64;
        }
    }
    -1
}

/// tcp_connect(ip, port): active open. Blocks the calling task until
/// ESTAB or the 8 s timeout. Returns the socket id or -1.
pub fn connect(dst_ip: u32, dst_port: u16) -> i64 {
    if dst_port == 0 || dst_ip == OUR_IP {
        return -1; // no TCP loopback by design
    }
    let (iss, lp);
    let id;
    {
        let _g = TCP_LOCK.lock();
        let s = socks();
        let Some(slot) = s.iter_mut().position(|c| !c.used) else {
            return -1;
        };
        let mut fresh = TcpSock::fresh();
        fresh.used = true;
        fresh.st = St::SynSent;
        fresh.local_port =
            (EPHEMERAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 20000 + 20000) as u16;
        fresh.peer_ip = dst_ip;
        fresh.peer_port = dst_port;
        fresh.iss = ISS_NEXT.fetch_add(64000, core::sync::atomic::Ordering::Relaxed);
        fresh.snd_una = fresh.iss;
        fresh.snd_nxt = fresh.iss.wrapping_add(1);
        fresh.rcv_nxt = 0;
        fresh.last_tx_us = now_us();
        iss = fresh.iss;
        lp = fresh.local_port;
        s[slot] = fresh;
        id = slot as i64;
        klog!("tcp: syn sent id={} to {}:{}", slot, ip_str(dst_ip), dst_port);
    }
    let _ = send_seg(dst_ip, dst_port, lp, iss, 0, TCP_SYN, &[]);

    // wait for the handshake in OUR OWN context (no locks held here)
    let t0 = now_us();
    loop {
        {
            let _g = TCP_LOCK.lock();
            let s = socks();
            if !s[id as usize].used {
                return -1; // reset / freed by the tick
            }
            if s[id as usize].st == St::Estab {
                return id;
            }
        }
        if now_us().saturating_sub(t0) > 8_000_000 {
            let _g = TCP_LOCK.lock();
            socks()[id as usize].used = false;
            socks()[id as usize].st = St::Free;
            klog!("tcp: connect timeout id={}", id);
            return -1;
        }
        sleep_tick();
    }
}

/// tcp_accept(listen_id): blocks until a connection completes; returns
/// the fresh socket's id (the listener keeps listening).
pub fn accept(lid: u64) -> i64 {
    let Some(lid) = usize::try_from(lid).ok().filter(|v| *v < NSOCK) else {
        return -1;
    };
    loop {
        {
            let _g = TCP_LOCK.lock();
            let s = socks();
            if !s[lid].used {
                return -1;
            }
            if s[lid].pend_id != 0 {
                let id = s[lid].pend_id as i64;
                s[lid].pend_id = 0;
                return id;
            }
        }
        sleep_tick();
    }
}

/// tcp_send(id, buf, len): copy user bytes into the TX ring, transmit
/// immediately. Returns the byte count or -1.
pub fn send(id: u64, uva: u64, len: u64) -> i64 {
    let Some(id) = usize::try_from(id).ok().filter(|v| *v < NSOCK) else {
        return -1;
    };
    let len = (len as usize).min(MSS);
    if len == 0 {
        return 0;
    }
    // copy-in under the caller's CR3 before touching shared state
    let space = crate::mem::vmm::AddressSpace::from_pml4(crate::mem::paging::cr3());
    let mut tmp = [0u8; MSS];
    if crate::user::uaccess::read_user_bytes(&space, uva, &mut tmp[..len]).is_err() {
        return -1;
    }
    let dip;
    let dp;
    let lp;
    let seq;
    {
        let _g = TCP_LOCK.lock();
        let s = &mut socks()[id];
        if !s.used || (s.st != St::Estab && s.st != St::CloseWait) {
            return -1;
        }
        let free = TX_CAP - s.tx_len;
        let n = len.min(free);
        if n == 0 {
            return -1; // TX ring full: back off and retry
        }
        ring_push(&mut s.tx, &mut s.tx_head, &mut s.tx_len, &tmp[..n]);
        seq = s.snd_nxt;
        s.snd_nxt = s.snd_nxt.wrapping_add(n as u32);
        s.snd_n += n as u64;
        s.last_tx_us = now_us();
        dip = s.peer_ip;
        dp = s.peer_port;
        lp = s.local_port;
    }
    // the lock is released: send_seg may sleep in ARP
    let _ = send_seg(dip, dp, lp, seq, 0, TCP_ACK | TCP_PSH, &tmp[..len]);
    // bytes stay in the ring either way; the tick retransmits on silence
    len as i64
}

/// tcp_recv(id, buf, len): pop in-order bytes (up to one MSS per call);
/// blocks until data or EOF. 0 = the peer closed and the stream drained.
pub fn recv(id: u64, uva: u64, len: u64) -> i64 {
    let Some(id) = usize::try_from(id).ok().filter(|v| *v < NSOCK) else {
        return -1;
    };
    let len = (len as usize).min(MSS);
    if len == 0 {
        return 0;
    }
    let space = crate::mem::vmm::AddressSpace::from_pml4(crate::mem::paging::cr3());
    loop {
        enum R {
            Data(usize, [u8; MSS]),
            Eof,
            Block,
        }
        let r = {
            let _g = TCP_LOCK.lock();
            let s = &mut socks()[id];
            if s.rx_len > 0 {
                let mut tmp = [0u8; MSS];
                R::Data(ring_pop(&s.rx, &mut s.rx_head, &mut s.rx_len, &mut tmp[..len]), tmp)
            } else if !s.used {
                return -1;
            } else if s.rx_eof || s.st == St::CloseWait {
                R::Eof
            } else {
                R::Block
            }
        };
        match r {
            R::Data(n, tmp) => {
                if crate::user::uaccess::write_user_bytes(&space, uva, &tmp[..n]).is_err() {
                    return -1;
                }
                return n as i64;
            }
            R::Eof => return 0,
            R::Block => sleep_tick(), // NO lock held here
        }
    }
}

/// tcp_close(id): FIN once (fire-and-forget) and free the slot.
pub fn close(id: u64) -> i64 {
    let Some(id) = usize::try_from(id).ok().filter(|v| *v < NSOCK) else {
        return -1;
    };
    let dip;
    let dp;
    let lp;
    let seq;
    let ack;
    let snd_n;
    let rcv_n;
    {
        let _g = TCP_LOCK.lock();
        let s = &mut socks()[id];
        if !s.used {
            return -1;
        }
        dip = s.peer_ip;
        dp = s.peer_port;
        lp = s.local_port;
        seq = s.snd_nxt;
        ack = s.rcv_nxt;
        snd_n = s.snd_n;
        rcv_n = s.rcv_n;
        s.used = false;
        s.st = St::Free;
    }
    if dip != 0 {
        let flags = if ack != 0 { TCP_FIN | TCP_ACK } else { TCP_FIN };
        let _ = send_seg(dip, dp, lp, seq, ack, flags, &[]);
    }
    klog!(
        "tcp: closed id={} (sent {} bytes, recv {} bytes)",
        id,
        snd_n,
        rcv_n
    );
    0
}

// --- ingress (netd task context) ---

/// Handle one inbound TCP segment from `src_ip`.
pub fn ingress(src_ip: u32, p: &[u8]) {
    let Some(seg) = tcp_parse(p) else { return };
    if !tcp_checksum_ok(src_ip, OUR_IP, p) {
        klog!("tcp: bad checksum from {} dropped", ip_str(src_ip));
        return;
    }
    let _g = TCP_LOCK.lock();
    let s = socks();

    // 1) a socket matching the 4-tuple (post-SYN states)
    if let Some(i) = s.iter().position(|c| {
        c.used
            && c.local_port == seg.dst_port
            && c.peer_ip == src_ip
            && c.peer_port == seg.src_port
            && matches!(c.st, St::SynSent | St::SynRcvd | St::Estab | St::CloseWait | St::FinWait)
    }) {
        handle_inband(_g, i, src_ip, &seg);
        return;
    }

    // 2) listeners take SYNs
    if let Some(i) = s
        .iter()
        .position(|c| c.used && c.st == St::Listen && c.local_port == seg.dst_port)
    {
        if seg.flags & TCP_SYN != 0 && seg.flags & TCP_ACK == 0 {
            handle_syn(_g, i, src_ip, &seg);
        }
        return;
    }

    // 3) no socket: RST back for non-RST segments
    if seg.flags & TCP_RST == 0 {
        drop(_g);
        let _ = send_seg(src_ip, seg.src_port, seg.dst_port, 0, 0, TCP_RST, &[]);
    }
}

/// SYN on a listener: answer SYN|ACK and hold the pending handshake on the
/// listener itself (state SynRcvd, one pending connection at a time).
fn handle_syn(_g: crate::sync::Guard<'static, ()>, lid: usize, src_ip: u32, seg: &TcpSeg<'_>) {
    let iss = ISS_NEXT.fetch_add(64000, core::sync::atomic::Ordering::Relaxed);
    let (dp, lp, rcv);
    {
        let s = &mut socks()[lid];
        s.st = St::SynRcvd;
        s.peer_ip = src_ip;
        s.peer_port = seg.src_port;
        s.iss = iss;
        s.snd_nxt = iss.wrapping_add(1);
        s.snd_una = iss;
        s.rcv_nxt = seg.seq.wrapping_add(1); // consume the SYN
        s.last_tx_us = now_us();
        dp = seg.src_port;
        lp = s.local_port;
        rcv = s.rcv_nxt;
    }
    klog!(
        "tcp: syn from {}:{} -> port {} (listener id={})",
        ip_str(src_ip),
        seg.src_port,
        lp,
        lid
    );
    drop(_g); // send outside the lock
    let _ = send_seg(src_ip, dp, lp, iss, rcv, TCP_SYN | TCP_ACK, &[]);
}

/// Everything after a SYN for a matched socket.
fn handle_inband(_g: crate::sync::Guard<'static, ()>, i: usize, src_ip: u32, seg: &TcpSeg<'_>) {
    let st = socks()[i].st;

    // RST kills the connection in any state
    if seg.flags & TCP_RST != 0 {
        let s = &mut socks()[i];
        klog!("tcp: rst id={} ({})", i, s.st.as_str());
        s.used = false;
        s.st = St::Free;
        return;
    }

    match st {
        St::SynSent => {
            if seg.flags & TCP_SYN != 0 && seg.ack == socks()[i].snd_nxt {
                let (dp, lp, snd, rcv);
                {
                    let s = &mut socks()[i];
                    s.rcv_nxt = seg.seq.wrapping_add(1);
                    s.snd_una = seg.ack;
                    s.st = St::Estab;
                    s.last_tx_us = now_us();
                    dp = seg.src_port;
                    lp = s.local_port;
                    snd = s.snd_nxt;
                    rcv = s.rcv_nxt;
                }
                klog!(
                    "tcp: established id={} peer {}:{}",
                    i,
                    ip_str(src_ip),
                    seg.src_port
                );
                drop(_g);
                let _ = send_seg(src_ip, dp, lp, snd, rcv, TCP_ACK, &[]);
            }
        }
        St::SynRcvd => {
            // the ACK completes the handshake: promote to a fresh socket
            if seg.flags & TCP_ACK != 0 && seg.ack == socks()[i].snd_nxt {
                let (dip, dp, lp, peer_ack, rcv);
                {
                    let s = &mut socks()[i];
                    s.snd_una = seg.ack;
                    (dip, dp, lp, peer_ack, rcv) =
                        (s.peer_ip, s.peer_port, s.local_port, seg.ack, s.rcv_nxt);
                }
                let s = socks();
                let Some(ni) = s.iter_mut().position(|c| !c.used) else {
                    // table full: reset the pending connection, keep listening
                    s[i].st = St::Listen;
                    drop(_g);
                    let _ = send_seg(dip, dp, lp, peer_ack, 0, TCP_RST, &[]);
                    return;
                };
                let mut fresh = TcpSock::fresh();
                fresh.used = true;
                fresh.st = St::Estab;
                fresh.local_port = socks()[i].local_port;
                fresh.peer_ip = dip;
                fresh.peer_port = dp;
                fresh.iss = socks()[i].iss;
                fresh.snd_una = peer_ack;
                fresh.snd_nxt = peer_ack;
                fresh.rcv_nxt = rcv;
                s[ni] = fresh;
                // the listener goes back to Listen and remembers the child
                socks()[i].st = St::Listen;
                socks()[i].pend_id = ni as u32;
                klog!(
                    "tcp: accepted id={} from {}:{} (listener id={})",
                    ni,
                    ip_str(dip),
                    dp,
                    i
                );
            }
        }
        St::Estab | St::CloseWait => {
            let mut need_ack = false;
            let (dp, lp, snd, rcv);
            {
                let s = &mut socks()[i];
                // ACK advance: free acked TX bytes
                if seg.flags & TCP_ACK != 0 {
                    let una = s.snd_una;
                    let ack = seg.ack;
                    if (ack.wrapping_sub(una) as i64) > 0 && (ack.wrapping_sub(s.snd_nxt) as i64) <= 0 {
                        let acked = (ack - una) as usize;
                        ring_drop(&mut s.tx_head, &mut s.tx_len, TX_CAP, acked);
                        s.snd_una = ack;
                        s.retries = 0;
                        s.last_tx_us = now_us();
                    }
                }
                // in-order data
                let plen = seg.payload.len() as u32;
                if plen > 0 {
                    need_ack = true;
                    if seg.seq == s.rcv_nxt {
                        let fits = (RX_CAP - s.rx_len) as u32;
                        let take = plen.min(fits) as usize;
                        ring_push(&mut s.rx, &mut s.rx_head, &mut s.rx_len, &seg.payload[..take]);
                        s.rcv_n += take as u64;
                        // only consume what we actually kept: the tail is
                        // retransmitted after our ACK
                        s.rcv_nxt = s.rcv_nxt.wrapping_add(take as u32);
                    } else if (seg.seq.wrapping_sub(s.rcv_nxt) as i64) < 0 {
                        // retransmission of already-seen data: just re-ACK
                    } else {
                        // future data (gap): drop, the dup-ACK prompts resend
                    }
                }
                // FIN
                if seg.flags & TCP_FIN != 0 {
                    s.rcv_nxt = s.rcv_nxt.wrapping_add(1);
                    if s.st == St::Estab {
                        s.st = St::CloseWait;
                        s.rx_eof = true;
                    }
                    need_ack = true;
                }
                (dp, lp, snd, rcv) = (seg.src_port, s.local_port, s.snd_nxt, s.rcv_nxt);
            }
            if need_ack {
                drop(_g);
                let _ = send_seg(src_ip, dp, lp, snd, rcv, TCP_ACK, &[]);
            }
        }
        St::FinWait => {
            // final ACK for our FIN -> done
            if seg.flags & TCP_ACK != 0 {
                let s = &mut socks()[i];
                s.used = false;
                s.st = St::Free;
                klog!("tcp: fin-acked id={} -> closed", i);
            }
        }
        _ => {}
    }
}

// --- timers (netd tick) ---

enum TickAction {
    RetxSyn { id: usize, give_up: bool },
    RetxData { id: usize, give_up: bool },
}

/// Retransmission pass, called from the netd loop every ~5 ms.
pub fn tick(now_us: u64) {
    for i in 0..NSOCK {
        let action = {
            let _g = TCP_LOCK.lock();
            let s = &mut socks()[i];
            if !s.used {
                continue;
            }
            match s.st {
                St::SynSent if now_us.saturating_sub(s.last_tx_us) > 1_000_000 => {
                    s.retries += 1;
                    s.last_tx_us = now_us;
                    Some(TickAction::RetxSyn {
                        id: i,
                        give_up: s.retries > 8,
                    })
                }
                St::Estab | St::CloseWait
                    if s.tx_len > 0 && now_us.saturating_sub(s.last_tx_us) > 400_000 =>
                {
                    s.retries += 1;
                    s.last_tx_us = now_us;
                    Some(TickAction::RetxData {
                        id: i,
                        give_up: s.retries > 8,
                    })
                }
                _ => None,
            }
        };
        let Some(action) = action else { continue };
        match action {
            TickAction::RetxSyn { id, give_up } => {
                let dip;
                let dp;
                let lp;
                let iss;
                {
                    let _g = TCP_LOCK.lock();
                    let s = &mut socks()[id];
                    if give_up {
                        klog!("tcp: connect timeout id={}", id);
                        s.used = false;
                        s.st = St::Free;
                        continue;
                    }
                    dip = s.peer_ip;
                    dp = s.peer_port;
                    lp = s.local_port;
                    iss = s.iss;
                }
                let _ = send_seg(dip, dp, lp, iss, 0, TCP_SYN, &[]);
            }
            TickAction::RetxData { id, give_up } => {
                let mut tmp = [0u8; MSS];
                let dip;
                let dp;
                let lp;
                let seq;
                let n;
                {
                    let _g = TCP_LOCK.lock();
                    let s = &mut socks()[id];
                    if give_up {
                        klog!("tcp: retransmit limit id={} -> reset", id);
                        s.used = false;
                        s.st = St::Free;
                        continue;
                    }
                    n = s.tx_len.min(MSS);
                    for k in 0..n {
                        tmp[k] = s.tx[(s.tx_head + k) % TX_CAP];
                    }
                    dip = s.peer_ip;
                    dp = s.peer_port;
                    lp = s.local_port;
                    seq = s.snd_una;
                }
                let _ = send_seg(dip, dp, lp, seq, 0, TCP_ACK | TCP_PSH, &tmp[..n]);
            }
        }
    }
}

/// Snapshot for the shell's `net` command.
pub fn for_each(mut f: impl FnMut(usize, St, u16, u32, u16, usize, usize, u64, u64)) {
    let _g = TCP_LOCK.lock();
    for (i, c) in socks().iter().enumerate() {
        if c.used {
            f(
                i,
                c.st,
                c.local_port,
                c.peer_ip,
                c.peer_port,
                c.rx_len,
                c.tx_len,
                c.snd_n,
                c.rcv_n,
            );
        }
    }
}
