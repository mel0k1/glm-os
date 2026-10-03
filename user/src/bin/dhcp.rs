//! dhcp — a real DHCP client, RFC 2131, entirely in ring 3 (v2.6).
//!
//!   DHCP.ELF            (no arguments; the NIC is the only network there is)
//!
//! Until today the OS knew exactly one address: 10.0.2.15, carved into the
//! kernel at boot. That is a lie every real OS stopped telling long ago —
//! a machine LEARNS its address. This program performs the classic dance:
//!
//!   1. drop the kernel config to 0.0.0.0 (net_setconf — the new v2.6
//!      syscall; the RFC discovery state, "I have no address"),
//!   2. DHCPDISCOVER -> 255.255.255.255:67 from UDP port 68 (the kernel's
//!      v2.6 broadcast fast path maps it to the Ethernet broadcast MAC
//!      without any ARP),
//!   3. DHCPOFFER <- the server proposes yiaddr (+ options: mask, router,
//!      DNS, lease, server id),
//!   4. DHCPREQUEST -> broadcast, echoing the offered address (option 50)
//!      and the server id (option 54),
//!   5. DHCPACK <- the lease is real: apply ip/mask/gw/dns via net_setconf
//!      and print the report.
//!
//! Both RX packets arrive while the machine is still unconfigured, so the
//! kernel accepts link-broadcast IPv4 (255.255.255.255) as its own — the
//! second half of the v2.6 kernel work. Every RX is validated: BOOTREPLY
//! op, matching xid, magic cookie, message type.
//!
//! Exit codes: 0 leased | 1 no nic (offline) | 2 bind failed
//!             | 3 no offer | 4 no ack | 5 setconf rejected.

#![no_std]
#![no_main]

use glm_user::{
    exit, fmt_u64, net_bind, net_close, net_info, net_recvfrom_timeout, net_sendto, net_setconf,
    uptime_ms, write, SrcAddr,
};

const DHCP_PORT_S: u16 = 67;
const DHCP_PORT_C: u16 = 68;
const TIMEOUT_MS: u64 = 2500;
const TRIES: usize = 3;

// RFC 2131 packet layout (fixed part)
const OP_BOOTREQUEST: u8 = 1;
const OP_BOOTREPLY: u8 = 2;
const HTYPE_ETHER: u8 = 1;
const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63]; // magic at offset 236

// option codes
const OPT_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_HOSTNAME: u8 = 12;
const OPT_REQ_IP: u8 = 50;
const OPT_LEASE: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;

// message types
const MT_DISCOVER: u8 = 1;
const MT_OFFER: u8 = 2;
const MT_REQUEST: u8 = 3;
const MT_ACK: u8 = 5;
const MT_NAK: u8 = 6;

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

fn ip_str(ip: u32, b: &mut [u8; 20]) -> &str {
    let mut n = 0;
    for i in (0..4).rev() {
        let oct = ((ip >> (i * 8)) & 0xFF) as u64;
        // copy the digits out first: fmt_u64 borrows `b` for its scratch
        let digits = fmt_u64(oct, b);
        let dl = digits.len();
        let mut own = [0u8; 3];
        own[..dl].copy_from_slice(digits);
        b[n..n + dl].copy_from_slice(&own[..dl]);
        n += dl;
        if i != 0 {
            b[n] = b'.';
            n += 1;
        }
    }
    core::str::from_utf8(&b[..n]).unwrap_or("?")
}

/// One shared scratch for the tiny formatters (the strings are consumed
/// before the next call).
struct Fmt {
    a: [u8; 20],
    b: [u8; 20],
    c: [u8; 20],
    d: [u8; 20],
    e: [u8; 20],
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("[dhcp     ] panic\n");
    exit(101)
}

/// Parse DHCP options starting at offset 240: find the message type and
/// every option the lease report needs. Returns (msg_type, mask, router,
/// dns, lease_s, server_id); missing options come back as 0.
fn parse_options(p: &[u8]) -> (u8, u32, u32, u32, u32, u32) {
    let (mut mt, mut mask, mut router, mut dns, mut lease, mut sid) = (0u8, 0u32, 0u32, 0u32, 0u32, 0u32);
    let mut i = 240usize;
    while i < p.len() {
        let code = p[i];
        if code == OPT_END {
            break;
        }
        if code == 0 {
            i += 1; // pad
            continue;
        }
        if i + 1 >= p.len() {
            break;
        }
        let len = p[i + 1] as usize;
        if i + 2 + len > p.len() {
            break;
        }
        let v = &p[i + 2..i + 2 + len];
        match code {
            OPT_MSG_TYPE if len >= 1 => mt = v[0],
            OPT_MASK if len == 4 => mask = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            OPT_ROUTER if len >= 4 => router = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            OPT_DNS if len >= 4 => dns = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            OPT_LEASE if len == 4 => lease = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            OPT_SERVER_ID if len == 4 => sid = u32::from_be_bytes([v[0], v[1], v[2], v[3]]),
            _ => {}
        }
        i += 2 + len;
    }
    (mt, mask, router, dns, lease, sid)
}

/// Build a BOOTREQUEST: fixed 236 bytes + cookie + options.
/// `req_ip`/`server_id` are only used by REQUEST (0 = skip the option).
fn build_request(buf: &mut [u8; 300], xid: u32, mt: u8, req_ip: u32, server_id: u32, mac48: u64) -> usize {
    for b in buf.iter_mut() {
        *b = 0;
    }
    buf[0] = OP_BOOTREQUEST;
    buf[1] = HTYPE_ETHER;
    buf[2] = 6; // hlen
    buf[3] = 0; // hops
    buf[4..8].copy_from_slice(&xid.to_be_bytes());
    // flags = 0x8000 (BROADCAST): we cannot receive unicast IP packets
    // while unconfigured, so tell the server to answer to 255.255.255.255
    buf[10] = 0x80;
    // chaddr: 16 bytes at offset 28, we have 6
    buf[28] = (mac48 >> 40) as u8;
    buf[29] = (mac48 >> 32) as u8;
    buf[30] = (mac48 >> 24) as u8;
    buf[31] = (mac48 >> 16) as u8;
    buf[32] = (mac48 >> 8) as u8;
    buf[33] = mac48 as u8;
    buf[236..240].copy_from_slice(&COOKIE);
    // options
    let mut o = 240usize;
    buf[o] = OPT_MSG_TYPE;
    buf[o + 1] = 1;
    buf[o + 2] = mt;
    o += 3;
    if req_ip != 0 {
        buf[o] = OPT_REQ_IP;
        buf[o + 1] = 4;
        buf[o + 2..o + 6].copy_from_slice(&req_ip.to_be_bytes());
        o += 6;
    }
    if server_id != 0 {
        buf[o] = OPT_SERVER_ID;
        buf[o + 1] = 4;
        buf[o + 2..o + 6].copy_from_slice(&server_id.to_be_bytes());
        o += 6;
    }
    // client id (hardware type + mac) — polite, slirp tolerates it
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&buf[28..34]);
    buf[o] = OPT_CLIENT_ID;
    buf[o + 1] = 7;
    buf[o + 2] = HTYPE_ETHER;
    buf[o + 3..o + 9].copy_from_slice(&chaddr);
    o += 9;
    // hostname: "glm-os" — the lease shows up in the server's table with a name
    buf[o] = OPT_HOSTNAME;
    buf[o + 1] = 6;
    buf[o + 2..o + 8].copy_from_slice(b"glm-os");
    o += 8;
    // parameter request list: mask, router, dns, lease
    buf[o] = 55;
    buf[o + 1] = 4;
    buf[o + 2] = OPT_MASK;
    buf[o + 3] = OPT_ROUTER;
    buf[o + 4] = OPT_DNS;
    buf[o + 5] = OPT_LEASE;
    o += 6;
    buf[o] = OPT_END;
    o += 1;
    o
}

/// Validate a BOOTREPLY: op, xid, cookie, then the message type option.
/// Returns (msg_type, yiaddr, mask, router, dns, lease, server_id) or None.
fn parse_reply(p: &[u8], xid: u32) -> Option<(u8, u32, u32, u32, u32, u32, u32)> {
    if p.len() < 240 || p[0] != OP_BOOTREPLY {
        return None;
    }
    let rx_xid = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
    if rx_xid != xid {
        return None;
    }
    if p[236..240] != COOKIE {
        return None;
    }
    let (mt, mask, router, dns, lease, sid) = parse_options(p);
    let yiaddr = u32::from_be_bytes([p[16], p[17], p[18], p[19]]);
    Some((mt, yiaddr, mask, router, dns, lease, sid))
}

#[no_mangle]
pub extern "C" fn _start(_argc: i64, _argv: *const *const u8) -> ! {
    let mut f = Fmt {
        a: [0; 20],
        b: [0; 20],
        c: [0; 20],
        d: [0; 20],
        e: [0; 20],
    };
    let mut ib = [0u8; 20];
    let mut jb = [0u8; 20];
    let mut kb = [0u8; 20];
    let mut lb = [0u8; 20];

    // ---- go unconfigured (RFC 2131 INIT state) ----------------------------
    let old_ip = net_info(0) as u32;
    if net_setconf(0, 0, 0, 0) < 0 {
        write("[dhcp     ] kernel refused setconf - exit 5\n");
        exit(5);
    }
    write("[dhcp     ] dropped to 0.0.0.0, discovering (was ");
    write(ip_str(old_ip, &mut ib));
    write(")\n");

    // ---- bind the client port ---------------------------------------------
    let id = net_bind(DHCP_PORT_C);
    if id < 0 {
        write("[dhcp     ] bind port 68 failed - exit 2\n");
        exit(2);
    }

    let mac48 = net_info(4) as u64; // our MAC, packed in the low 48 bits
    // xid: wall-clock-mixed uptime — collision-free enough for one client
    let mut xid: u32 = (glm_user::clock_time() as u32) ^ (uptime_ms() as u32) ^ 0x474C_4D4F;
    if xid == 0 {
        xid = 0x474C_4D4F;
    }

    let mut tx = [0u8; 300];
    let mut rx = [0u8; 512];

    // ---- DISCOVER -> OFFER -------------------------------------------------
    let mut offered: Option<(u32, u32, u32, u32, u32)> = None; // yi, mask, router, dns, sid
    let mut attempt = 1;
    while attempt <= TRIES && offered.is_none() {
        let n = build_request(&mut tx, xid, MT_DISCOVER, 0, 0, mac48);
        if net_sendto(id, 0xFFFF_FFFF, DHCP_PORT_S, &tx[..n]) < 0 {
            write("[dhcp     ] sendto broadcast failed (no nic?) - exit 1\n");
            net_close(id);
            exit(1);
        }
        write("[dhcp     ] DISCOVER xid=");
        write(num(xid as u64, &mut f.a));
        write(" -> 255.255.255.255:67 (try ");
        write(num(attempt as u64, &mut f.b));
        write(")\n");
        let mut src = SrcAddr::new();
        let r = net_recvfrom_timeout(id, &mut rx, &mut src, TIMEOUT_MS);
        if let Some((mt, yi, mask, router, dns, _lease, sid)) = parse_reply(&rx[..r.max(0) as usize], xid) {
            if mt == MT_OFFER && yi != 0 {
                write("[dhcp     ] OFFER from ");
                write(ip_str(src.ip(), &mut jb));
                write(": ");
                write(ip_str(yi, &mut kb));
                write("\n");
                offered = Some((yi, mask, router, dns, sid));
                break;
            }
            if mt == MT_NAK {
                write("[dhcp     ] server said NAK - exit 3\n");
                net_close(id);
                exit(3);
            }
        }
        attempt += 1;
    }

    let Some((yi, _mask_o, _router_o, _dns_o, server_id)) = offered else {
        net_close(id);
        write("[dhcp     ] no OFFER after ");
        write(num(TRIES as u64, &mut f.c));
        write(" tries - exit 3\n");
        exit(3);
    };

    // ---- REQUEST -> ACK (state SELECTING, broadcast per RFC) ----------------
    let mut acked: Option<(u32, u32, u32, u32, u32)> = None; // yi, mask, router, dns, lease
    let mut attempt = 1;
    while attempt <= TRIES && acked.is_none() {
        let n = build_request(&mut tx, xid, MT_REQUEST, yi, server_id, mac48);
        if net_sendto(id, 0xFFFF_FFFF, DHCP_PORT_S, &tx[..n]) < 0 {
            write("[dhcp     ] request sendto failed - exit 1\n");
            net_close(id);
            exit(1);
        }
        write("[dhcp     ] REQUEST ");
        write(ip_str(yi, &mut ib));
        write(" sid=");
        write(ip_str(server_id, &mut jb));
        write(" (try ");
        write(num(attempt as u64, &mut f.b));
        write(")\n");
        let mut src = SrcAddr::new();
        let r = net_recvfrom_timeout(id, &mut rx, &mut src, TIMEOUT_MS);
        if let Some((mt, yi2, mask, router, dns, lease, _sid)) = parse_reply(&rx[..r.max(0) as usize], xid) {
            if mt == MT_ACK && yi2 == yi {
                write("[dhcp     ] ACK from ");
                write(ip_str(src.ip(), &mut kb));
                write("\n");
                acked = Some((yi2, mask, router, dns, lease));
                break;
            }
            if mt == MT_NAK {
                write("[dhcp     ] server said NAK to REQUEST - exit 4\n");
                net_close(id);
                exit(4);
            }
        }
        attempt += 1;
    }
    net_close(id);

    let Some((ip, mask, router, dns, lease)) = acked else {
        write("[dhcp     ] no ACK after ");
        write(num(TRIES as u64, &mut f.d));
        write(" tries - exit 4\n");
        exit(4);
    };

    // ---- apply the lease ----------------------------------------------------
    if net_setconf(ip, mask, router, dns) < 0 {
        write("[dhcp     ] setconf rejected - exit 5\n");
        exit(5);
    }

    // ---- the report ----------------------------------------------------------
    write("[dhcp     ] leased ");
    write(ip_str(ip, &mut ib));
    write(" mask ");
    write(ip_str(mask, &mut jb));
    write(" gw ");
    write(ip_str(router, &mut kb));
    write(" dns ");
    write(ip_str(dns, &mut lb));
    write("\n");
    if lease != 0 {
        write("[dhcp     ] lease time ");
        write(num(lease as u64, &mut f.e));
        write(" s\n");
    }
    write("[dhcp     ] applied via net_setconf - the address was LEARNED, exit 0\n");
    exit(0)
}
