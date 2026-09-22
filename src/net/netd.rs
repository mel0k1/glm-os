//! netd — the kernel network task: ARP cache + ICMP engine.
//!
//! Loop: drain the NIC (safety net alongside the ISR), pop queued frames,
//! answer ARP requests and ICMP echo requests, note echo replies for the
//! ping engine. Everything blocking (ARP resolve, ping waits) happens in
//! task context via SYS_SLEEP — never in the ISR.

use crate::klog;
use crate::mem::vmm::kernel_cr3;
use crate::net::e1000;
use crate::net::proto::*;
use crate::net::{ip_str, NET_LOCK, OUR_IP};
use crate::sched::{self, ksyscall, NewTask, SYS_SLEEP};
use crate::sync::Spinlock;
use core::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// µs clock: TSC calibrated once at netd start (uptime_ms has 10 ms pitch)
// ---------------------------------------------------------------------------

static TSC_PER_MS: AtomicU64 = AtomicU64::new(0);
static CAL_TSC0: AtomicU64 = AtomicU64::new(0);

#[inline]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack)) };
    ((hi as u64) << 32) | lo as u64
}

fn calibrate() {
    let t0 = rdtsc();
    ksyscall(SYS_SLEEP, 250, 0, 0);
    let t1 = rdtsc();
    let per_ms = (t1 - t0) / 250;
    TSC_PER_MS.store(per_ms.max(1), Ordering::Relaxed);
    CAL_TSC0.store(t0, Ordering::Relaxed);
    klog!("netd: tsc calibrated, {} ticks/ms ({} mhz est)", per_ms, per_ms / 1000);
}

/// Microseconds since netd calibration (monotonic).
pub fn now_us() -> u64 {
    let per = TSC_PER_MS.load(Ordering::Relaxed);
    if per == 0 {
        return crate::cpu::pit::uptime_ms() * 1000;
    }
    (rdtsc() - CAL_TSC0.load(Ordering::Relaxed)) * 1000 / per
}

/// monotonically increasing IPv4 id field.
pub fn ipv4_id() -> u16 {
    (IPV4_ID.fetch_add(1, Ordering::Relaxed) & 0xFFFF) as u16
}
static IPV4_ID: AtomicU64 = AtomicU64::new(0x474C);

// ---------------------------------------------------------------------------
// ARP table
// ---------------------------------------------------------------------------

const ARP_SLOTS: usize = 8;

#[derive(Clone, Copy)]
struct ArpEntry {
    ip: u32,
    mac: [u8; 6],
    stamp_us: u64,
    valid: bool,
}

static mut ARP_TABLE: [ArpEntry; ARP_SLOTS] = [const {
    ArpEntry {
        ip: 0,
        mac: [0; 6],
        stamp_us: 0,
        valid: false,
    }
}; ARP_SLOTS];

fn arp_learn(ip: u32, mac: &[u8; 6]) {
    if ip == 0 {
        return;
    }
    let now = now_us();
    unsafe {
        // update in place?
        for e in ARP_TABLE.iter_mut() {
            if e.valid && e.ip == ip {
                e.mac = *mac;
                e.stamp_us = now;
                return;
            }
        }
        // free slot?
        for e in ARP_TABLE.iter_mut() {
            if !e.valid {
                e.ip = ip;
                e.mac = *mac;
                e.stamp_us = now;
                e.valid = true;
                klog!(
                    "netd: arp learned {} at {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
                    ip_str(ip),
                    mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                );
                return;
            }
        }
        // evict the oldest
        let mut oldest = 0usize;
        for i in 1..ARP_SLOTS {
            if ARP_TABLE[i].stamp_us < ARP_TABLE[oldest].stamp_us {
                oldest = i;
            }
        }
        ARP_TABLE[oldest] = ArpEntry {
            ip,
            mac: *mac,
            stamp_us: now,
            valid: true,
        };
    }
}

fn arp_lookup(ip: u32) -> Option<[u8; 6]> {
    unsafe {
        ARP_TABLE
            .iter()
            .find(|e| e.valid && e.ip == ip)
            .map(|e| e.mac)
    }
}

pub fn our_mac() -> [u8; 6] {
    e1000::mac()
}

fn send_arp_request(ip: u32) {
    let mut frame = [0u8; ETH_HDR + ARP_PKT];
    let mac_buf = our_mac();
    let n = eth_put(&mut frame, &MAC_BCAST, &mac_buf, ETHERTYPE_ARP);
    let pkt_len = arp_put(
        &mut frame[n..],
        ARP_REQUEST,
        &mac_buf,
        OUR_IP,
        &[0; 6],
        ip,
    );
    let ok = e1000::send_frame(&frame[..n + pkt_len]);
    klog!("netd: arp request tx ok={} ({} bytes)", ok, n + pkt_len);
}

/// Resolve an IPv4 address to a MAC (blocking, task context).
pub fn arp_resolve(ip: u32) -> Option<[u8; 6]> {
    for try_n in 0..5 {
        if let Some(m) = arp_lookup(ip) {
            return Some(m);
        }
        klog!("netd: arp try {} for {}", try_n, ip_str(ip));
        send_arp_request(ip);
        ksyscall(SYS_SLEEP, 120, 0, 0);
        if let Some(m) = arp_lookup(ip) {
            return Some(m);
        }
    }
    klog!("netd: arp resolve failed for {}", ip_str(ip));
    None
}

// ---------------------------------------------------------------------------
// TX helpers (eth+ip+icmp sandwiches)
// ---------------------------------------------------------------------------

fn send_ipv4(dst_mac: &[u8; 6], dst_ip: u32, proto: u8, payload: &[u8]) {
    let mut frame = [0u8; ETH_HDR + 20 + 8 + 32];
    let mac = our_mac();
    let n = eth_put(&mut frame, dst_mac, &mac, ETHERTYPE_IPV4);
    let ihl = ipv4_put(&mut frame[n..], proto, OUR_IP, dst_ip, payload.len());
    frame[n + ihl..n + ihl + payload.len()].copy_from_slice(payload);
    e1000::send_frame(&frame[..n + ihl + payload.len()]);
}

pub const PING_ID: u16 = 0x474C; // 'GL'

fn send_echo_request(dst_ip: u32, seq: u16) -> bool {
    let Some(mac) = arp_lookup(dst_ip) else { return false };
    let mut icmp = [0u8; 8 + 32];
    let mut payload = [0u8; 32];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = b"GLM-OS-PING"[i % 11];
    }
    icmp_echo_put(&mut icmp, ICMP_ECHO_REQUEST, PING_ID, seq, &payload);
    send_ipv4(&mac, dst_ip, PROTO_ICMP, &icmp[..8 + 32]);
    true
}

// ---------------------------------------------------------------------------
// Inbound processing (netd task context)
// ---------------------------------------------------------------------------

fn handle_frame(frame: &[u8], len: usize) {
    let Some(eth) = eth_parse(&frame[..len]) else { return };
    match eth.ethertype {
        ETHERTYPE_ARP => handle_arp(eth.payload, &eth.src),
        ETHERTYPE_IPV4 => handle_ipv4(eth.payload, &eth.src),
        _ => {}
    }
}

fn handle_arp(p: &[u8], src: &[u8; 6]) {
    let Some(arp) = arp_parse(p) else { return };
    if arp.spa != 0 {
        arp_learn(arp.spa, &arp.sha);
    }
    if arp.oper == ARP_REQUEST && is_ours(arp.tpa) {
        // answer: who-has OUR_IP? tell <sender>
        let mut frame = [0u8; ETH_HDR + ARP_PKT];
        let mac = our_mac();
        let n = eth_put(&mut frame, src, &mac, ETHERTYPE_ARP);
        let pkt_len = arp_put(
            &mut frame[n..],
            ARP_REPLY,
            &mac,
            OUR_IP,
            &arp.sha,
            arp.spa,
        );
        e1000::send_frame(&frame[..n + pkt_len]);
    }
}

fn handle_ipv4(p: &[u8], src: &[u8; 6]) {
    let Some(ip) = ipv4_parse(p) else { return };
    if !is_ours(ip.dst) {
        return;
    }
    match ip.proto {
        PROTO_ICMP => handle_icmp(ip, src),
        PROTO_UDP => handle_udp(ip),
        _ => {}
    }
}

fn handle_icmp(ip: Ipv4Hdr<'_>, src: &[u8; 6]) {
    let Some(echo) = icmp_echo_parse(ip.payload) else { return };
    match echo.kind {
        ICMP_ECHO_REQUEST => {
            // reply with the same id/seq/payload
            let mut icmp = [0u8; 8 + 32];
            icmp_echo_put(&mut icmp, ICMP_ECHO_REPLY, echo.id, echo.seq, &echo.payload[..echo.payload_len]);
            send_ipv4(src, ip.src, PROTO_ICMP, &icmp[..8 + echo.payload_len]);
        }
        ICMP_ECHO_REPLY => {
            if echo.id == PING_ID {
                klog!(
                    "netd: icmp echo reply seq={} ttl={} ({} bytes)",
                    echo.seq,
                    ip.ttl,
                    echo.payload_len
                );
                let mut slot = REPLY.lock();
                slot.ip = ip.src;
                slot.seq = echo.seq;
                slot.ttl = ip.ttl;
                slot.t_us = now_us();
                slot.valid = true;
            }
        }
        _ => {}
    }
}

/// Inbound UDP: demux to the userland socket table (v0.9). Bad checksum
/// or an unbound port are silently counted/dropped at the socket layer.
fn handle_udp(ip: Ipv4Hdr<'_>) {
    if let Some(u) = udp_parse(ip.payload) {
        crate::net::sock::deliver(u.dst_port, ip.src, u.src_port, u.payload);
    }
}

/// TX path for userland sendto(): loopback was already handled in
/// sock.rs; this resolves ARP (sleeping in task context is fine) and
/// hands the frame to the NIC. Returns success.
pub fn udp_send(dst_ip: u32, dst_port: u16, src_port: u16, payload: &[u8]) -> bool {
    if !e1000::online() {
        return false;
    }
    let Some(mac) = arp_resolve(dst_ip) else { return false };
    let mut frame = [0u8; ETH_HDR + IPV4_HDR_MIN + UDP_HDR + 256];
    let our = our_mac();
    let n = eth_put(&mut frame, &mac, &our, ETHERTYPE_IPV4);
    let ihl = ipv4_put(
        &mut frame[n..],
        PROTO_UDP,
        OUR_IP,
        dst_ip,
        UDP_HDR + payload.len(),
    );
    let ul = udp_put(
        &mut frame[n + ihl..],
        OUR_IP,
        dst_ip,
        src_port,
        dst_port,
        payload,
    );
    e1000::send_frame(&frame[..n + ihl + ul])
}

// ---------------------------------------------------------------------------
// Ping engine (shell-facing)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct ReplySlot {
    ip: u32,
    seq: u16,
    ttl: u8,
    t_us: u64,
    valid: bool,
}
static REPLY: Spinlock<ReplySlot> = Spinlock::new(ReplySlot {
    ip: 0,
    seq: 0,
    ttl: 0,
    t_us: 0,
    valid: false,
});

/// Parse a dotted quad ("10.0.2.2").
pub fn parse_ip(s: &str) -> Option<u32> {
    let mut parts = [0u32; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n >= 4 {
            return None;
        }
        let v: u32 = part.parse().ok()?;
        if v > 255 {
            return None;
        }
        parts[n] = v;
        n += 1;
    }
    if n != 4 {
        return None;
    }
    Some((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3])
}

/// The shell `ping` command: 4 echo requests, classic report.
pub fn ping_shell(arg: &str) {
    use crate::console::{print, print_color, GLM_CYAN, GLM_GRAY, GLM_GREEN, GLM_YELLOW};

    if !e1000::online() {
        print_color("net: offline (no nic detected at boot)\n", GLM_YELLOW);
        return;
    }
    let target = match arg {
        "" => crate::net::GW_IP,
        s => match parse_ip(s) {
            Some(ip) => ip,
            None => {
                print_color("usage: ping <a.b.c.d>  (empty = default gateway)\n", GLM_YELLOW);
                return;
            }
        },
    };

    print(&alloc::format!("PING {}: 32 data bytes\n", ip_str(target)));

    // ARP resolve up-front so the report is honest about it
    print(&alloc::format!("ARP resolving {} ... ", ip_str(target)));
    let mac = arp_resolve(target);
    match &mac {
        Some(m) => {
            print_color(&alloc::format!(
                "ok ({:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X})\n",
                m[0], m[1], m[2], m[3], m[4], m[5]
            ), GLM_GREEN);
        }
        None => {
            print_color("no answer\n", GLM_YELLOW);
            print_color("--- ping aborted (host unreachable at layer 2) ---\n", GLM_GRAY);
            return;
        }
    }

    let mut sent = 0usize;
    let mut received = 0usize;
    for seq in 0u16..4 {
        REPLY.lock().valid = false;
        if !send_echo_request(target, seq) {
            print_color("net: arp entry vanished, re-resolving failed\n", GLM_YELLOW);
            break;
        }
        sent += 1;
        let t0 = now_us();
        let mut got = false;
        while now_us().saturating_sub(t0) < 2_000_000 {
            ksyscall(SYS_SLEEP, 2, 0, 0);
            let mut slot = REPLY.lock();
            if slot.valid && slot.seq == seq {
                let rtt_us = slot.t_us.saturating_sub(t0);
                let ttl = slot.ttl;
                slot.valid = false;
                drop(slot);
                received += 1;
                print_color(
                    &alloc::format!(
                        "64 bytes from {}: icmp_seq={} ttl={} time={}.{} ms\n",
                        ip_str(target),
                        seq,
                        ttl,
                        rtt_us / 1000,
                        (rtt_us % 1000) / 100
                    ),
                    GLM_CYAN,
                );
                got = true;
                break;
            }
        }
        if !got {
            print_color(&alloc::format!(
                "request timeout for icmp_seq {}\n",
                seq
            ), GLM_YELLOW);
        }
        ksyscall(SYS_SLEEP, 200, 0, 0);
    }

    let loss = if sent == 0 { 100 } else { (sent - received) * 100 / sent };
    print_color(
        &alloc::format!(
            "--- {} ping statistics ---\n{} packets transmitted, {} received, {}% packet loss\n",
            ip_str(target),
            sent,
            received,
            loss
        ),
        GLM_GRAY,
    );
}

// ---------------------------------------------------------------------------
// Shell status printers
// ---------------------------------------------------------------------------

pub fn net_status() {
    use crate::console::{print, print_color, GLM_CYAN, GLM_GRAY, GLM_WHITE, GLM_YELLOW};
    if !e1000::online() {
        print_color("net: offline (no nic detected at boot)\n", GLM_YELLOW);
        return;
    }
    let (irq, rx, tx, dropped, kicks) = e1000::counters();
    print_color(&alloc::format!(
        "NIC:      intel e1000, mmio {}, mac {}\n",
        crate::net::pci::found().map(|d| alloc::format!("bar0={:#x}", d.bar0_phys)).unwrap_or_default(),
        e1000::mac_str()
    ), GLM_WHITE);
    print(&alloc::format!(
        "IPv4:     {}/24 via {} (qemu slirp)\n",
        ip_str(OUR_IP),
        ip_str(crate::net::GW_IP)
    ));
    print(&alloc::format!(
        "Link:     {}, irq vector {:#x}\n",
        if e1000::link_up() { "up" } else { "down" },
        crate::net::pci::found().map(|d| 32 + d.irq).unwrap_or(0)
    ));
    print_color(&alloc::format!(
        "Counters: {} irq ({} rx kicks), {} rx, {} tx, {} dropped\n",
        irq, kicks, rx, tx, dropped
    ), GLM_CYAN);

    // v0.9: userland UDP sockets
    let mut any_sock = false;
    crate::net::sock::for_each(|id, port, queued, recv_n, drop_n, waiting| {
        if !any_sock {
            print_color("Sockets:  ID PORT  QUEUE  RECV  DROP  WAITER\n", GLM_WHITE);
            any_sock = true;
        }
        print(&alloc::format!(
            "          {:>2} {:>4}  {:>5}  {:>4}  {:>4}  {}\n",
            id,
            port,
            queued,
            recv_n,
            drop_n,
            if waiting { "recvfrom" } else { "-" }
        ));
    });
    if !any_sock {
        print_color("Sockets:  (none bound - userland can bind via int 0x80)\n", GLM_GRAY);
    }
}

pub fn arp_dump() {
    use crate::console::{print, print_color, GLM_CYAN, GLM_GRAY, GLM_YELLOW};
    if !e1000::online() {
        print_color("net: offline\n", GLM_YELLOW);
        return;
    }
    print_color("ARP cache:\n", GLM_CYAN);
    let _g = NET_LOCK.lock();
    unsafe {
        let mut any = false;
        for e in ARP_TABLE.iter() {
            if e.valid {
                any = true;
                print(&alloc::format!(
                    "  {} at {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X} ({} s old)\n",
                    ip_str(e.ip),
                    e.mac[0], e.mac[1], e.mac[2], e.mac[3], e.mac[4], e.mac[5],
                    (now_us() - e.stamp_us) / 1_000_000
                ));
            }
        }
        if !any {
            print_color("  (empty - run ping to populate)\n", GLM_GRAY);
        }
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

fn netd_main() -> ! {
    calibrate();
    klog!("netd: online (arp + icmp, 5 ms tick)");
    let mut buf = [0u8; 2048];
    let mut ticks: u64 = 0;
    loop {
        // safety-net drain (the ISR drains too; both paths are idempotent)
        e1000::poll_drain();
        let mut n = e1000::pop_rx(&mut buf);
        while n > 0 {
            handle_frame(&buf, n);
            n = e1000::pop_rx(&mut buf);
        }
        ticks += 1;
        if ticks % 1200 == 0 {
            klog!("netd: alive, tick {}", ticks);
        }
        ksyscall(SYS_SLEEP, 5, 0, 0);
    }
}

/// Spawn netd (called from net::init, after the scheduler is up).
pub fn spawn() {
    let _ = sched::spawn(NewTask {
        name: "netd",
        entry: netd_main as *const () as u64,
        user_rsp: None,
        pml4: unsafe { kernel_cr3() },
        is_user: false,
        user_space: None,
        pinned_cpu: sched::CPU_ANY,
    });
}
