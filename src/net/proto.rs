//! Minimal protocol encoders/decoders: Ethernet II, ARP, IPv4, ICMP echo.
//!
//! Everything is plain byte surgery on fixed-size stack buffers — no
//! allocation on the hot path. The internet checksum is the classic
//! 16-bit ones' complement fold.

use crate::net::{OUR_IP};

pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;

pub const ETH_HDR: usize = 14;
pub const ARP_PKT: usize = 28;
pub const IPV4_HDR_MIN: usize = 20;
pub const ICMP_HDR: usize = 8;

pub const ICMP_ECHO_REPLY: u8 = 0;
pub const ICMP_ECHO_REQUEST: u8 = 8;

/// Broadcast MAC.
pub const MAC_BCAST: [u8; 6] = [0xFF; 6];

// ---------------------------------------------------------------------------
// Ethernet II
// ---------------------------------------------------------------------------

/// [dst 6][src 6][type 2] then payload.
pub fn eth_put(buf: &mut [u8], dst: &[u8; 6], src: &[u8; 6], etype: u16) -> usize {
    buf[0..6].copy_from_slice(dst);
    buf[6..12].copy_from_slice(src);
    buf[12] = (etype >> 8) as u8;
    buf[13] = (etype & 0xFF) as u8;
    ETH_HDR
}

pub struct EthFrame<'a> {
    pub dst: [u8; 6],
    pub src: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn eth_parse(frame: &[u8]) -> Option<EthFrame<'_>> {
    if frame.len() < ETH_HDR {
        return None;
    }
    let mut dst = [0u8; 6];
    let mut src = [0u8; 6];
    dst.copy_from_slice(&frame[0..6]);
    src.copy_from_slice(&frame[6..12]);
    Some(EthFrame {
        dst,
        src,
        ethertype: ((frame[12] as u16) << 8) | frame[13] as u16,
        payload: &frame[ETH_HDR..],
    })
}

// ---------------------------------------------------------------------------
// ARP
// ---------------------------------------------------------------------------

pub const ARP_REQUEST: u16 = 1;
pub const ARP_REPLY: u16 = 2;

/// Build an ARP packet into `buf` (after the 14-byte eth header).
/// Returns the packet length (28).
pub fn arp_put(buf: &mut [u8], oper: u16, sender_mac: &[u8; 6], sender_ip: u32, target_mac: &[u8; 6], target_ip: u32) -> usize {
    buf[0..2].copy_from_slice(&1u16.to_be_bytes()); // htype: ethernet
    buf[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes()); // ptype: ipv4
    buf[4] = 6; // hlen
    buf[5] = 4; // plen
    buf[6..8].copy_from_slice(&oper.to_be_bytes());
    buf[8..14].copy_from_slice(sender_mac);
    buf[14..18].copy_from_slice(&sender_ip.to_be_bytes());
    buf[18..24].copy_from_slice(target_mac);
    buf[24..28].copy_from_slice(&target_ip.to_be_bytes());
    ARP_PKT
}

pub struct ArpPkt {
    pub oper: u16,
    pub sha: [u8; 6],
    pub spa: u32,
    pub tha: [u8; 6],
    pub tpa: u32,
}

pub fn arp_parse(p: &[u8]) -> Option<ArpPkt> {
    if p.len() < ARP_PKT {
        return None;
    }
    let mut sha = [0u8; 6];
    let mut tha = [0u8; 6];
    sha.copy_from_slice(&p[8..14]);
    tha.copy_from_slice(&p[18..24]);
    Some(ArpPkt {
        oper: ((p[6] as u16) << 8) | p[7] as u16,
        sha,
        spa: u32::from_be_bytes([p[14], p[15], p[16], p[17]]),
        tha,
        tpa: u32::from_be_bytes([p[24], p[25], p[26], p[27]]),
    })
}

// ---------------------------------------------------------------------------
// IPv4
// ---------------------------------------------------------------------------

/// Build an IPv4 header into `buf`. Returns the header length (20).
pub fn ipv4_put(buf: &mut [u8], proto: u8, src: u32, dst: u32, payload_len: usize) -> usize {
    let total_len = (IPV4_HDR_MIN + payload_len) as u16;
    buf[0] = 0x45; // v4, IHL=5
    buf[1] = 0; // DSCP/ECN
    buf[2..4].copy_from_slice(&total_len.to_be_bytes());
    let id = crate::net::netd::ipv4_id();
    buf[4..6].copy_from_slice(&id.to_be_bytes());
    buf[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF, no frag
    buf[8] = 64; // TTL
    buf[9] = proto;
    buf[10..12].copy_from_slice(&[0, 0]); // checksum placeholder
    buf[12..16].copy_from_slice(&src.to_be_bytes());
    buf[16..20].copy_from_slice(&dst.to_be_bytes());
    let sum = checksum(&buf[..IPV4_HDR_MIN]);
    buf[10..12].copy_from_slice(&sum.to_be_bytes());
    IPV4_HDR_MIN
}

pub struct Ipv4Hdr<'a> {
    pub ihl: usize,
    pub proto: u8,
    pub src: u32,
    pub dst: u32,
    pub ttl: u8,
    pub payload: &'a [u8],
}

pub fn ipv4_parse(p: &[u8]) -> Option<Ipv4Hdr<'_>> {
    if p.len() < IPV4_HDR_MIN {
        return None;
    }
    let ihl = (p[0] & 0x0F) as usize * 4;
    if (p[0] >> 4) != 4 || ihl < IPV4_HDR_MIN || p.len() < ihl {
        return None;
    }
    let total = ((p[2] as usize) << 8) | p[3] as usize;
    let payload_len = total.saturating_sub(ihl).min(p.len() - ihl);
    // header checksum must verify
    if checksum(&p[..ihl]) != 0 {
        return None;
    }
    Some(Ipv4Hdr {
        ihl,
        proto: p[9],
        ttl: p[8],
        src: u32::from_be_bytes([p[12], p[13], p[14], p[15]]),
        dst: u32::from_be_bytes([p[16], p[17], p[18], p[19]]),
        payload: &p[ihl..ihl + payload_len],
    })
}

pub const PROTO_ICMP: u8 = 1;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_TCP: u8 = 6;

/// Is this IPv4 address ours?
pub fn is_ours(ip: u32) -> bool {
    ip == OUR_IP
}

// ---------------------------------------------------------------------------
// UDP (v0.9): the userland socket transport
// ---------------------------------------------------------------------------

pub const UDP_HDR: usize = 8;

/// Build a UDP datagram into `buf` (after the IPv4 header). The checksum
/// covers the classic pseudo-header (src/dst ip, proto, udp length) so the
/// peer — slirp or our own demux — can verify it end to end.
/// Returns bytes written (8 + payload.len()).
pub fn udp_put(buf: &mut [u8], src_ip: u32, dst_ip: u32, src_port: u16, dst_port: u16, payload: &[u8]) -> usize {
    let n = UDP_HDR + payload.len();
    buf[0..2].copy_from_slice(&src_port.to_be_bytes());
    buf[2..4].copy_from_slice(&dst_port.to_be_bytes());
    buf[4..6].copy_from_slice(&(n as u16).to_be_bytes());
    buf[6..8].copy_from_slice(&[0, 0]); // checksum placeholder
    buf[UDP_HDR..n].copy_from_slice(payload);
    // pseudo-header fold
    let mut sum = 0u32;
    for w in src_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    for w in dst_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    sum += PROTO_UDP as u32;
    sum += (n as u32) & 0xFFFF;
    let mut i = 0;
    while i + 1 < n {
        sum += ((buf[i] as u32) << 8) | buf[i + 1] as u32;
        i += 2;
    }
    if i < n {
        sum += (buf[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    buf[6..8].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    n
}

pub struct UdpHdr<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub payload: &'a [u8],
}

/// Parse a UDP header + payload. The length field is trusted only within
/// the bounds of the actual bytes; a zero checksum ("not used") is accepted.
pub fn udp_parse(p: &[u8]) -> Option<UdpHdr<'_>> {
    if p.len() < UDP_HDR {
        return None;
    }
    let src_port = ((p[0] as u16) << 8) | p[1] as u16;
    let dst_port = ((p[2] as u16) << 8) | p[3] as u16;
    let dgram_len = (((p[4] as u16) << 8) | p[5] as u16) as usize;
    if dgram_len < UDP_HDR {
        return None;
    }
    let payload_len = (dgram_len - UDP_HDR).min(p.len() - UDP_HDR);
    Some(UdpHdr {
        src_port,
        dst_port,
        payload: &p[UDP_HDR..UDP_HDR + payload_len],
    })
}

// ---------------------------------------------------------------------------
// ICMP echo
// ---------------------------------------------------------------------------

/// Build an ICMP echo (request or reply) after the IPv4 header.
/// Returns bytes written (8 + payload.len()).
pub fn icmp_echo_put(buf: &mut [u8], kind: u8, id: u16, seq: u16, payload: &[u8]) -> usize {
    buf[0] = kind;
    buf[1] = 0; // code
    buf[2..4].copy_from_slice(&[0, 0]); // checksum placeholder
    buf[4..6].copy_from_slice(&id.to_be_bytes());
    buf[6..8].copy_from_slice(&seq.to_be_bytes());
    let n = ICMP_HDR + payload.len();
    buf[ICMP_HDR..n].copy_from_slice(payload);
    let sum = checksum(&buf[..n]);
    buf[2..4].copy_from_slice(&sum.to_be_bytes());
    n
}

pub struct IcmpEcho {
    pub kind: u8,
    pub id: u16,
    pub seq: u16,
    pub payload: [u8; 32],
    pub payload_len: usize,
}

/// Parse an ICMP echo request/reply (the only ICMP kinds we care about).
pub fn icmp_echo_parse(p: &[u8]) -> Option<IcmpEcho> {
    if p.len() < ICMP_HDR {
        return None;
    }
    let kind = p[0];
    if kind != ICMP_ECHO_REQUEST && kind != ICMP_ECHO_REPLY {
        return None;
    }
    if checksum(p) != 0 {
        return None; // bad icmp checksum
    }
    let plen = (p.len() - ICMP_HDR).min(32);
    let mut payload = [0u8; 32];
    payload[..plen].copy_from_slice(&p[ICMP_HDR..ICMP_HDR + plen]);
    Some(IcmpEcho {
        kind,
        id: ((p[4] as u16) << 8) | p[5] as u16,
        seq: ((p[6] as u16) << 8) | p[7] as u16,
        payload,
        payload_len: plen,
    })
}

// ---------------------------------------------------------------------------
// Internet checksum
// ---------------------------------------------------------------------------

pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u32;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += ((data[i] as u32) << 8) | data[i + 1] as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// TCP (v1.3): segments for the ring-3 stream sockets
// ---------------------------------------------------------------------------

pub const TCP_HDR_MIN: usize = 20;

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;

/// Build a TCP segment into `buf` (after the IPv4 header) with the classic
/// pseudo-header checksum. Returns bytes written (header + payload).
#[allow(clippy::too_many_arguments)]
pub fn tcp_put(
    buf: &mut [u8],
    src_ip: u32,
    dst_ip: u32,
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    payload: &[u8],
) -> usize {
    let n = TCP_HDR_MIN + payload.len();
    buf[0..2].copy_from_slice(&src_port.to_be_bytes());
    buf[2..4].copy_from_slice(&dst_port.to_be_bytes());
    buf[4..8].copy_from_slice(&seq.to_be_bytes());
    buf[8..12].copy_from_slice(&ack.to_be_bytes());
    buf[12] = 0x50; // data offset 5 (20 bytes), no options
    buf[13] = flags;
    buf[14..16].copy_from_slice(&window.to_be_bytes());
    buf[16..18].copy_from_slice(&[0, 0]); // checksum placeholder
    buf[18..20].copy_from_slice(&[0, 0]); // urgent pointer
    buf[TCP_HDR_MIN..n].copy_from_slice(payload);
    // pseudo-header fold (same shape as udp_put)
    let mut sum = 0u32;
    for w in src_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    for w in dst_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    sum += PROTO_TCP as u32;
    sum += (n as u32) & 0xFFFF;
    let mut i = 0;
    while i + 1 < n {
        sum += ((buf[i] as u32) << 8) | buf[i + 1] as u32;
        i += 2;
    }
    if i < n {
        sum += (buf[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    buf[16..18].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    n
}

/// Verify a TCP segment checksum (the pseudo-header carries src/dst ip,
/// so the segment bytes alone are not enough).
pub fn tcp_checksum_ok(src_ip: u32, dst_ip: u32, p: &[u8]) -> bool {
    let mut sum = 0u32;
    for w in src_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    for w in dst_ip.to_be_bytes().chunks(2) {
        sum += ((w[0] as u32) << 8) | w[1] as u32;
    }
    sum += PROTO_TCP as u32;
    sum += (p.len() as u32) & 0xFFFF;
    let mut i = 0;
    while i + 1 < p.len() {
        sum += ((p[i] as u32) << 8) | p[i + 1] as u32;
        i += 2;
    }
    if i < p.len() {
        sum += (p[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    (sum as u16) == 0xFFFF
}

pub struct TcpSeg<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    pub payload: &'a [u8],
}

/// Parse a TCP segment. Truncates the payload to the bytes actually
/// present (the IPv4 layer already clamped to the IP total length).
pub fn tcp_parse(p: &[u8]) -> Option<TcpSeg<'_>> {
    if p.len() < TCP_HDR_MIN {
        return None;
    }
    let doff = (p[12] >> 4) as usize * 4;
    if doff < TCP_HDR_MIN || p.len() < doff {
        return None;
    }
    Some(TcpSeg {
        src_port: ((p[0] as u16) << 8) | p[1] as u16,
        dst_port: ((p[2] as u16) << 8) | p[3] as u16,
        seq: u32::from_be_bytes([p[4], p[5], p[6], p[7]]),
        ack: u32::from_be_bytes([p[8], p[9], p[10], p[11]]),
        flags: p[13],
        window: ((p[14] as u16) << 8) | p[15] as u16,
        payload: &p[doff..],
    })
}
