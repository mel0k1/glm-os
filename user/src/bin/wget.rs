//! wget — fetch a URL over DNS + HTTP and drop the body on the disk (v2.0).
//!
//!   wget http://host[:port][/path] [OUT.HTM]
//!
//! Pure ring 3 end to end: names go through glm_user::dns (UDP sockets),
//! the document through glm_user::http (TCP syscalls), the body through
//! the v1.6 file syscalls. The kernel never parses a single HTTP byte.
//!
//! Exit codes:
//!   0  saved the body            1  usage
//!   2  DNS failed                3  TCP connect failed
//!   4  request send failed       5  response never arrived / malformed
//!   6  non-200 final status      7  too many redirects
//!   8  could not save the file

#![no_std]
#![no_main]

use glm_user::dns;
use glm_user::http;
use glm_user::{exit, file_close, file_open, file_write, fmt_u64, write, O_CREATE};

const HOPS: usize = 4; // redirect budget
static mut RBUF: [u8; 65536] = [0; 65536]; // BSS: 64 KiB is way over the 16-page stack

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}

/// The parts of a URL we care about.
struct Url {
    host: [u8; 128],
    host_len: usize,
    port: u16,
    path: [u8; 256],
    path_len: usize,
}

/// Parse "http://host[:port][/path]" (the only scheme we speak).
fn parse_url(u: &[u8]) -> Option<Url> {
    const SCHEME: &[u8] = b"http://";
    if u.len() < SCHEME.len() || !u[..7].eq_ignore_ascii_case(SCHEME) {
        return None;
    }
    let rest = &u[7..];
    let slash = rest.iter().position(|&c| c == b'/').unwrap_or(rest.len());
    let hostport = &rest[..slash];
    let (host_b, port) = match hostport.iter().rposition(|&c| c == b':') {
        Some(c) => {
            let mut p: u16 = 0;
            let ps = &hostport[c + 1..];
            if ps.is_empty() || ps.len() > 5 || !ps.iter().all(|b| b.is_ascii_digit()) {
                return None;
            }
            for &d in ps {
                p = p * 10 + (d - b'0') as u16;
            }
            (&hostport[..c], p)
        }
        None => (hostport, 80),
    };
    if host_b.is_empty() || host_b.len() > 128 {
        return None;
    }
    let mut url = Url {
        host: [0; 128],
        host_len: host_b.len(),
        port,
        path: [0; 256],
        path_len: 0,
    };
    url.host[..host_b.len()].copy_from_slice(host_b);
    let path = if slash < rest.len() {
        &rest[slash..]
    } else {
        &b"/"[..]
    };
    if path.len() > 256 {
        return None;
    }
    url.path[..path.len()].copy_from_slice(path);
    url.path_len = path.len();
    Some(url)
}

/// Byte slice of the host (for &str-style handling).
fn host_as_str(u: &Url) -> &str {
    core::str::from_utf8(&u.host[..u.host_len]).unwrap_or("?")
}

fn path_as_str(u: &Url) -> &str {
    core::str::from_utf8(&u.path[..u.path_len]).unwrap_or("/")
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    let mut nb = [0u8; 20];
    if argc < 2 {
        write("[wget ] usage: wget http://host[:port][/path] [OUT.HTM]\n");
        write("[wget ] exit 1\n");
        exit(1);
    }

    // ---- argv -> owned byte buffers (the argv arena dies at exit) -------
    let mut urlb = [0u8; 384];
    let mut outb = [0u8; 64];
    let urlb_len = copy_arg(unsafe { *argv.add(1) }, &mut urlb);
    let out_len = if argc > 2 {
        copy_arg(unsafe { *argv.add(2) }, &mut outb)
    } else {
        let d = b"PAGE.HTM";
        outb[..d.len()].copy_from_slice(d);
        d.len()
    };
    let out = core::str::from_utf8(&outb[..out_len]).unwrap_or("PAGE.HTM");

    let Some(mut url) = parse_url(&urlb[..urlb_len]) else {
        write("[wget ] only http:// URLs are supported (no TLS) - exit 1\n");
        exit(1);
    };
    write("[wget ] url http://");
    write(host_as_str(&url));
    write(":");
    write(num(url.port as u64, &mut nb));
    write(path_as_str(&url));
    write(" -> ");
    write(out);
    write("\n");

    let mut buf = unsafe { &mut *core::ptr::addr_of_mut!(RBUF) };

    // ---- resolve (dotted quads skip the wire) ---------------------------
    let host = host_as_str(&url);
    let ip = match dns::resolve(host) {
        Ok(ip) => ip,
        Err(e) => {
            write("[wget ] dns: ");
            write(dns::err_str(e));
            write(" - exit 2\n");
            exit(2);
        }
    };
    let mut ipbuf = [0u8; 16];
    write("[wget ] dns: ");
    write(dns::ip_to_str(ip, &mut ipbuf));
    write("\n");

    // ---- fetch, following redirects -------------------------------------
    let mut hops = 0usize;
    let mut cur_ip = ip;
    loop {
        match http::get(cur_ip, url.port, host_as_str(&url), path_as_str(&url), buf) {
            Ok(resp) => {
                write("[wget ] status ");
                write(num(resp.status as u64, &mut nb));
                write(", ");
                write(num(resp.total as u64, &mut nb));
                write(" bytes total, body at +");
                write(num(resp.body as u64, &mut nb));
                write("\n");

                if resp.status == 200 {
                    let fd = file_open(out, O_CREATE);
                    if fd < 0 {
                        write("[wget ] open failed - exit 8\n");
                        exit(8);
                    }
                    let n = file_write(fd, &buf[resp.body..resp.body + resp.body_len]);
                    let _ = file_close(fd);
                    if n < 0 {
                        write("[wget ] write failed - exit 8\n");
                        exit(8);
                    }
                    write("[wget ] saved ");
                    write(num(n as u64, &mut nb));
                    write(" bytes to ");
                    write(out);
                    write("\n[wget ] exit 0\n");
                    exit(0);
                }

                if resp.status == 301 || resp.status == 302 || resp.status == 303 || resp.status == 307 || resp.status == 308 {
                    if hops + 1 >= HOPS {
                        write("[wget ] too many redirects - exit 7\n");
                        exit(7);
                    }
                    let loc = http::header_value(&buf[..resp.body], b"location:");
                    if loc.is_none() {
                        write("[wget ] redirect without location - exit 7\n");
                        exit(7);
                    }
                    let loc = loc.unwrap();
                    let mut lbuf = [0u8; 384];
                    if loc.len() > lbuf.len() {
                        write("[wget ] location too long - exit 7\n");
                        exit(7);
                    }
                    lbuf[..loc.len()].copy_from_slice(loc);
                    hops += 1;
                    write("[wget ] redirect -> ");
                    write(core::str::from_utf8(&lbuf[..loc.len()]).unwrap_or("?"));
                    write("\n");
                    if !follow_location(&lbuf[..loc.len()], &mut url) {
                        write("[wget ] bad location - exit 7\n");
                        exit(7);
                    }
                    // re-resolve the (possibly new) host
                    let h = host_as_str(&url);
                    match dns::resolve(h) {
                        Ok(v) => cur_ip = v,
                        Err(e) => {
                            write("[wget ] dns on redirect: ");
                            write(dns::err_str(e));
                            write(" - exit 2\n");
                            exit(2);
                        }
                    }
                    continue;
                }

                write("[wget ] non-200 status - exit 6\n");
                exit(6);
            }
            Err(http::E_CONNECT) => {
                write("[wget ] connect failed - exit 3\n");
                exit(3);
            }
            Err(http::E_SEND) => {
                write("[wget ] send failed - exit 4\n");
                exit(4);
            }
            Err(_) => {
                write("[wget ] response never arrived - exit 5\n");
                exit(5);
            }
        }
    }
}

/// Copy one argv C string into `dst`. Returns the length (capped).
fn copy_arg(p: *const u8, dst: &mut [u8]) -> usize {
    let mut n = 0usize;
    unsafe {
        while n < dst.len() {
            let c = *p.add(n);
            if c == 0 {
                break;
            }
            dst[n] = c;
            n += 1;
        }
    }
    n
}

/// Apply a Location header to `url`. Absolute "http://..." URLs are
/// re-parsed; relative paths swap the path only. Returns false on junk.
fn follow_location(loc: &[u8], url: &mut Url) -> bool {
    if let Some(nu) = parse_url(loc) {
        *url = nu;
        return true;
    }
    // relative: must start with '/'
    if loc.first() != Some(&b'/') || loc.len() > 256 {
        return false;
    }
    url.path = [0; 256];
    url.path[..loc.len()].copy_from_slice(loc);
    url.path_len = loc.len();
    true
}
