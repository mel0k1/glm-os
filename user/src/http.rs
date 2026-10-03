//! http — a plain HTTP/1.0 GET over the v1.3 TCP syscalls (v2.0).
//!
//! The response is drained until EOF (Connection: close semantics),
//! then split into headers and body. `content-length` is honoured as a
//! clamp when present. TLS is out of scope: this fetcher speaks
//! `http://`, not `https://`.

use crate::{tcp_close, tcp_connect, tcp_recv, tcp_send};

/// One parsed response: everything the caller needs to find the body
/// inside the buffer it handed to get().
pub struct HttpResp {
    pub status: u16,
    /// Offset of the first body byte inside the caller's buffer.
    pub body: usize,
    /// Bytes of body actually received (clamped by content-length).
    pub body_len: usize,
    /// Total bytes received (headers + body), for diagnostics.
    pub total: usize,
}

/// Error codes:
pub const E_CONNECT: i64 = -1; // tcp_connect failed
pub const E_SEND: i64 = -2; // the request did not go out in full
pub const E_EOF: i64 = -3; // EOF / error before a full header block
pub const E_MALFORMED: i64 = -4; // not an HTTP status line

/// Perform one GET. `buf` receives the raw response; it bounds what we
/// keep (a bigger body is silently truncated at `buf.len()`).
pub fn get(ip: u32, port: u16, host: &str, path: &str, buf: &mut [u8]) -> Result<HttpResp, i64> {
    let id = tcp_connect(ip, port);
    if id < 0 {
        return Err(E_CONNECT);
    }

    // ---- the request ----------------------------------------------------
    let mut req = [0u8; 512];
    let mut r = 0usize;
    for part in [
        &b"GET "[..],
        path.as_bytes(),
        b" HTTP/1.0\r\nHost: ",
        host.as_bytes(),
        b"\r\nUser-Agent: glm-os/2.0 (ring 3)\r\nConnection: close\r\n\r\n",
    ] {
        if r + part.len() > req.len() {
            let _ = tcp_close(id);
            return Err(E_SEND); // path absurdly long
        }
        req[r..r + part.len()].copy_from_slice(part);
        r += part.len();
    }
    let mut sent = 0usize;
    while sent < r {
        let n = tcp_send(id, &req[sent..r]);
        if n <= 0 {
            let _ = tcp_close(id);
            return Err(E_SEND);
        }
        sent += n as usize;
    }

    // ---- drain the response until EOF -----------------------------------
    let mut total = 0usize;
    while total < buf.len() {
        let n = tcp_recv(id, &mut buf[total..]);
        if n < 0 {
            let _ = tcp_close(id);
            return Err(E_EOF);
        }
        if n == 0 {
            break; // clean EOF: the whole response is in
        }
        total += n as usize;
    }
    let _ = tcp_close(id);
    if total == 0 {
        return Err(E_EOF);
    }

    // ---- split headers / body -------------------------------------------
    let hdr_end = match find(&buf[..total], b"\r\n\r\n") {
        Some(h) => h,
        None => return Err(E_EOF), // no header terminator: not HTTP
    };
    let body_at = hdr_end + 4;

    // status line: "HTTP/1.x NNN ..." — digits at 9..=11
    if total < 12 || &buf[0..5] != b"HTTP/" || !buf[9].is_ascii_digit() {
        return Err(E_MALFORMED);
    }
    if !buf[10].is_ascii_digit() || !buf[11].is_ascii_digit() {
        return Err(E_MALFORMED);
    }
    let status = ((buf[9] - b'0') as u16) * 100
        + ((buf[10] - b'0') as u16) * 10
        + ((buf[11] - b'0') as u16);

    // content-length as an honest clamp
    let mut body_len = total - body_at;
    if let Some(cl) = header_value(&buf[..hdr_end], b"content-length:") {
        if let Ok(v) = parse_usize(&cl) {
            body_len = body_len.min(v);
        }
    }

    Ok(HttpResp {
        status,
        body: body_at,
        body_len,
        total,
    })
}

/// Case-insensitive header lookup INSIDE a header block (call with the
/// [0..hdr_end] slice). Copies the value (whitespace-trimmed) into `out`
/// and returns its length, or None. Only the first match is returned.
pub fn header_value<'a>(hdrs: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut i = 0usize;
    while i + name.len() < hdrs.len() {
        if hdrs[i] == b'\r' || hdrs[i] == b'\n' {
            i += 1;
            continue;
        }
        // start of a line: try to match the (lowercased) name here
        let line_end = hdrs[i..]
            .iter()
            .position(|&c| c == b'\r')
            .unwrap_or(hdrs.len() - i);
        let line = &hdrs[i..i + line_end];
        if line.len() > name.len()
            && line[..name.len()]
                .iter()
                .zip(name.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            let mut v = &line[name.len()..];
            while let Some(f) = v.first() {
                if *f == b' ' || *f == b'\t' {
                    v = &v[1..];
                } else {
                    break;
                }
            }
            return Some(v);
        }
        i += line_end + 2; // skip past the CR (and the LF)
    }
    None
}

/// Byte-exact needle search (returns the offset of the FIRST match).
fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

fn parse_usize(s: &[u8]) -> Result<usize, ()> {
    let mut v: usize = 0;
    for &c in s {
        if !c.is_ascii_digit() {
            return Err(());
        }
        v = v.saturating_mul(10).saturating_add((c - b'0') as usize);
    }
    Ok(v)
}
