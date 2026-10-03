//! httpd — a web server entirely in ring 3 (v2.4).
//!
//!   HTTPD.ELF [port] [max_connections]   (defaults: 8080, 0 = forever)
//!
//! GLM OS spoke HTTP as a client since v2.0 (WGET). This closes the loop:
//! the OS now SERVES its own persistent disk to the world. Every layer is
//! a previous version's building block:
//!
//!   * v1.3 TCP syscalls   — listen/accept/recv/send/close,
//!   * v1.6 file syscalls  — open/seek/read of files on the FAT32 disk,
//!   * v2.1 heap (malloc)  — a per-file body buffer sized EXACTLY to the
//!                           file (two-pass: seek(END) for the size, then
//!                           malloc + seek(0) + read), nothing fixed in BSS,
//!   * v1.7 argv           — port and connection budget from the command line.
//!
//! Protocol: honest HTTP/1.0 with `Connection: close` — read the request
//! head to "\r\n\r\n" (2 KiB cap), answer, close. GET only (405 otherwise);
//! paths containing ".." are refused with 403 before the filesystem ever
//! sees them; "/" maps to /INDEX.HTM; missing files get a small 404 page.
//! Names are uppercased to the FAT32 8.3 convention.
//!
//! The kernel's sched task-exit klog plus the host-side curl checks make
//! the whole thing machine-verifiable without OCR.
//!
//! Exit codes: 0 served all requested connections | 1 usage | 2 listen
//! failed | 3 accept failed.

#![no_std]
#![no_main]

use glm_user::heap::malloc;
use glm_user::{
    exit, file_close, file_open, file_read, file_seek, fmt_u64, tcp_accept, tcp_close, tcp_listen,
    tcp_recv, tcp_send, write,
};

const HEAD_CAP: usize = 2048;

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    write("[httpd    ] panic\n");
    exit(101)
}

/// Content-Type for the few extensions the disk actually carries.
fn content_type(path: &[u8]) -> &'static str {
    let ext = match path.iter().rposition(|&c| c == b'.') {
        Some(d) => &path[d + 1..],
        None => &[],
    };
    if ext.eq_ignore_ascii_case(b"HTM") || ext.eq_ignore_ascii_case(b"HTML") {
        "text/html"
    } else if ext.eq_ignore_ascii_case(b"TXT") {
        "text/plain"
    } else {
        "application/octet-stream"
    }
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    // ---- argv ------------------------------------------------------------
    let mut ab = [0u8; 64];
    let mut ab2 = [0u8; 64];
    let mut b = [0u8; 20];
    let port: u16 = if argc > 1 {
        let p = unsafe { *argv.add(1) } as *const u8;
        match glm_user::arg_str(glm_user::cstr_into(p, &mut ab)).parse_u16() {
            Some(v) => v,
            None => {
                write("[httpd    ] bad port - exit 1\n");
                exit(1);
            }
        }
    } else {
        8080
    };
    let max_conn: u64 = if argc > 2 {
        let p = unsafe { *argv.add(2) } as *const u8;
        let s = glm_user::arg_str(glm_user::cstr_into(p, &mut ab2));
        let mut v: u64 = 0;
        let ok = !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
        if ok {
            for d in s.bytes() {
                v = v * 10 + (d - b'0') as u64;
            }
        }
        if !ok {
            write("[httpd    ] bad connection budget - exit 1\n");
            exit(1);
        }
        v
    } else {
        0 // serve forever
    };

    let lid = match tcp_listen(port) {
        id if id >= 0 => id,
        _ => {
            write("[httpd    ] listen failed - exit 2\n");
            exit(2);
        }
    };
    write("[httpd    ] ring-3 web server on port ");
    write(num(port as u64, &mut b));
    if max_conn > 0 {
        write(" (");
        write(num(max_conn, &mut b));
        write(" connections, then exit 0)");
    } else {
        write(" (serving until killed)");
    }
    write("\n");

    let mut served: u64 = 0;
    let mut head = [0u8; HEAD_CAP];
    loop {
        if max_conn > 0 && served >= max_conn {
            write("[httpd    ] connection budget spent - exit 0\n");
            exit(0);
        }
        let id = tcp_accept(lid);
        if id < 0 {
            write("[httpd    ] accept failed - exit 3\n");
            exit(3);
        }
        serve_one(id, &mut head);
        tcp_close(id);
        served += 1;
    }
}

/// Read the request head, parse the path, answer, and log the outcome.
fn serve_one(id: i64, head: &mut [u8; HEAD_CAP]) {
    let mut b = [0u8; 20];

    // ---- read until end of the request head --------------------------------
    let mut filled = 0usize;
    while filled < HEAD_CAP {
        let want = (HEAD_CAP - filled).min(512);
        let n = tcp_recv(id, &mut head[filled..filled + want]);
        if n <= 0 {
            break; // EOF or error: work with what we have
        }
        filled += n as usize;
        // header end present?
        if find_head_end(&head[..filled]).is_some() {
            break;
        }
    }

    // ---- parse the request line --------------------------------------------
    let line_end = head[..filled]
        .iter()
        .position(|&c| c == b'\n')
        .unwrap_or(filled.min(HEAD_CAP));
    let line = &head[..line_end];
    let line = if line_end > 0 && line[line_end - 1] == b'\r' {
        &line[..line_end - 1]
    } else {
        line
    };
    // "GET <path> HTTP/1.x"
    let sp1 = line.iter().position(|&c| c == b' ');
    let rest = match sp1 {
        Some(p1) => &line[p1 + 1..],
        None => &line[..0],
    };
    let sp2 = rest.iter().position(|&c| c == b' ');
    let (method, raw_path) = match (sp1, sp2) {
        (Some(_), Some(p2)) => (&line[..sp1.unwrap()], &rest[..p2]),
        (Some(_), None) => (&line[..sp1.unwrap()], rest),
        _ => (&line[..0], &line[..0]),
    };
    let is_get = method.eq_ignore_ascii_case(b"GET");
    let raw_path = strip_query(raw_path);

    // ---- guard rails, then serve -------------------------------------------
    if !is_get {
        send_text(id, 405, "Method Not Allowed", BODY_405);
        log_req(b"GET", b"(non-get)", 405, BODY_405.len());
        return;
    }
    if path_has_dotdot(raw_path) {
        send_text(id, 403, "Forbidden", BODY_403);
        log_req(b"GET", raw_path, 403, BODY_403.len());
        return;
    }

    // "/" -> /INDEX.HTM; uppercase to the FAT 8.3 convention
    let mut path = [0u8; 128];
    let path: &[u8] = if raw_path.is_empty() || raw_path == b"/" {
        b"/INDEX.HTM"
    } else {
        let n = raw_path.len().min(path.len());
        path[..n].copy_from_slice(&raw_path[..n]);
        for c in path[..n].iter_mut() {
            c.make_ascii_uppercase();
        }
        &path[..n]
    };

    // ---- the v1.6/v2.1 file path -------------------------------------------
    let name = core::str::from_utf8(&path[1..]).unwrap_or("INDEX.HTM");
    let fd = file_open(name, 0); // O_RDONLY
    if fd < 0 {
        send_text(id, 404, "Not Found", BODY_404);
        log_req(b"GET", &path, 404, BODY_404.len());
        return;
    }

    // two-pass body: size via seek(END), exact malloc, then read
    let size = file_seek(fd, 0, 2); // whence 2 = END
    if size < 0 {
        file_close(fd);
        send_text(id, 500, "Internal Error", BODY_500);
        log_req(b"GET", &path, 500, BODY_500.len());
        return;
    }
    let size = size as u64 as usize;
    let body = malloc(size as u64); // malloc(0) allocates 1 byte itself
    if body.is_null() {
        file_close(fd);
        send_text(id, 500, "Internal Error", BODY_500);
        log_req(b"GET", &path, 500, BODY_500.len());
        return;
    }
    file_seek(fd, 0, 0); // rewind
    let mut got = 0usize;
    while got < size {
        let n = file_read(fd, unsafe {
            core::slice::from_raw_parts_mut(body.add(got), size - got)
        });
        if n <= 0 {
            break;
        }
        got += n as usize;
    }
    file_close(fd);

    // ---- the response -------------------------------------------------------
    let mut hdr = [0u8; 192];
    let status = b"HTTP/1.0 200 OK\r\n";
    let ct = content_type(&path);
    let mut h = 0usize;
    h = put(&mut hdr, h, status);
    h = put(&mut hdr, h, b"Server: GLM-OS-httpd/2.4 (ring3)\r\n");
    h = put(&mut hdr, h, b"Content-Type: ");
    h = put(&mut hdr, h, ct.as_bytes());
    h = put(&mut hdr, h, b"\r\n");
    h = put(&mut hdr, h, b"Content-Length: ");
    let cl = num(got as u64, &mut b);
    h = put(&mut hdr, h, cl.as_bytes());
    h = put(&mut hdr, h, b"\r\nConnection: close\r\n\r\n");
    let _ = tcp_send(id, &hdr[..h]);
    if got > 0 {
        let body_slice = unsafe { core::slice::from_raw_parts(body, got) };
        tcp_send(id, body_slice);
    }

    log_req(b"GET", &path, 200, got);
}

// ---- small no_std helpers ---------------------------------------------------

const BODY_405: &[u8] = b"GLM OS httpd: GET only\n";
const BODY_403: &[u8] = b"GLM OS httpd: path traversal refused\n";
const BODY_404: &[u8] = b"GLM OS httpd: no such file on the disk\n";
const BODY_500: &[u8] = b"GLM OS httpd: internal error\n";

fn put(dst: &mut [u8], at: usize, src: &[u8]) -> usize {
    let n = src.len().min(dst.len().saturating_sub(at));
    dst[at..at + n].copy_from_slice(&src[..n]);
    at + n
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn strip_query(p: &[u8]) -> &[u8] {
    match p.iter().position(|&c| c == b'?') {
        Some(q) => &p[..q],
        None => p,
    }
}

fn path_has_dotdot(p: &[u8]) -> bool {
    p.windows(2).any(|w| w == b"..")
}

/// One honest log line to the console/terminal; the CONTENT stays
/// host-verifiable (curl checks bytes), this is for the human.
fn log_req(_m: &[u8], path: &[u8], status: u64, bytes: usize) {
    let mut b = [0u8; 20];
    write("[httpd    ] GET ");
    write(core::str::from_utf8(path).unwrap_or("?"));
    write(" -> ");
    write(num(status, &mut b));
    write(" (");
    write(num(bytes as u64, &mut b));
    write(" bytes)\n");
}

/// Tiny response for every non-200 outcome (403/404/405/500).
fn send_text(id: i64, status: u64, reason: &str, body: &[u8]) {
    let mut b = [0u8; 20];
    let mut hdr = [0u8; 160];
    let mut h = 0usize;
    h = put(&mut hdr, h, b"HTTP/1.0 ");
    let st = num(status, &mut b);
    h = put(&mut hdr, h, st.as_bytes());
    h = put(&mut hdr, h, b" ");
    h = put(&mut hdr, h, reason.as_bytes());
    h = put(&mut hdr, h, b"\r\nServer: GLM-OS-httpd/2.4 (ring3)\r\n");
    h = put(&mut hdr, h, b"Content-Type: text/plain\r\nContent-Length: ");
    let cl = num(body.len() as u64, &mut b);
    h = put(&mut hdr, h, cl.as_bytes());
    h = put(&mut hdr, h, b"\r\nConnection: close\r\n\r\n");
    let _ = tcp_send(id, &hdr[..h]);
    let _ = tcp_send(id, body);
}

trait ParseU16 {
    fn parse_u16(&self) -> Option<u16>;
}
impl ParseU16 for str {
    fn parse_u16(&self) -> Option<u16> {
        if self.is_empty() || self.len() > 5 || !self.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let mut v: u32 = 0;
        for d in self.bytes() {
            v = v * 10 + (d - b'0') as u32;
        }
        if v <= u16::MAX as u32 {
            Some(v as u16)
        } else {
            None
        }
    }
}
