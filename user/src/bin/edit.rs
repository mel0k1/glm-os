//! edit — v2.2: a ring-3 text editor for GLM OS.
//!
//! The second heap-backed desktop application (after the v2.1 file
//! manager) and the first one that EDITS: documents live entirely in
//! malloc'd memory — one variable-length buffer per line, the line
//! table itself realloc'd on demand. Nothing here is a fixed BSS array:
//! a 10-line note and a 500-line file allocate exactly what they need.
//!
//! Editing model (no arrow keys in the v0.1 keyboard map — the mouse
//! positions the caret instead):
//!   * click a cell        -> caret moves there
//!   * printable keys      -> insert at the caret
//!   * backspace (0x08)    -> delete before the caret / join lines
//!   * enter (0x0A)        -> split the line at the caret
//!   * [SAVE] button (or 's') -> write through the v1.6 file syscalls
//!   * [x] / EV_CLOSE      -> exit (unsaved changes are not prompted)
//!
//! The document renders into the window through the v1.2 GUI syscalls;
//! the caret blinks by uptime (redrawn every ~500 ms without any kernel
//! support). Save path: argv[1], or /HOME/UNTITLED.TXT when launched
//! without arguments (the start menu does exactly that).

#![no_std]
#![no_main]

use glm_user::{
    cstr_into, exit, file_close, file_open, file_read, file_write, fmt_u64, gui_close, gui_event,
    gui_flush, gui_geo, gui_open, gui_rect, gui_text, heap, sleep_ms, write, gui_ev_parts, arg_str,
    EV_CLICK, EV_CLOSE, EV_KEY, EV_RESIZE, O_CREATE, O_RDWR,
};

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(101)
}

// palette
const BG: u32 = 0x14181E;
const TOOLBAR: u32 = 0x2A3140;
const BTN: u32 = 0x3A4254;
const SAVE_BTN: u32 = 0x5EE28D;
const BTN_TXT: u32 = 0xE6EDF3;
const TXT: u32 = 0xC6CCD8;
const DIM: u32 = 0x7E8494;
const CYAN: u32 = 0x46E2E2;
const STATUS: u32 = 0x1B2029;
const CARET: u32 = 0x5EE28D;

const WIN_X: i32 = 190;
const WIN_Y: i32 = 110;
const WIN_W: i32 = 470;
const WIN_H: i32 = 310;

const TOOL_H: i32 = 20;
const TOP: i32 = 24;
const ROW_H: i32 = 14;
const STATUS_H: i32 = 16;
const CELL: i32 = 8;

const MAX_LINE: u64 = 16 * 1024; // sanity cap per line
const DEF_SAVE: &str = "/HOME/UNTITLED.TXT";

// v2.5: extended key codes riding EV_KEY (must match the kernel's
// keyboard driver decode: set-1 scancodes behind the 0xE0 prefix)
const K_UP: u8 = 0x80;
const K_DOWN: u8 = 0x81;
const K_LEFT: u8 = 0x82;
const K_RIGHT: u8 = 0x83;
const K_HOME: u8 = 0x84;
const K_END: u8 = 0x85;
const K_DEL: u8 = 0x86;
const K_PGUP: u8 = 0x87;
const K_PGDN: u8 = 0x88;

/// One document line: a malloc'd, capacity-doubled byte buffer.
/// A plain 3-word POD — copies are copies of the handles, never of the
/// bytes, which is exactly the semantics the raw table needs.
#[derive(Clone, Copy)]
struct Line {
    buf: *mut u8,
    cap: u64,
    len: u64,
}

struct Doc {
    lines: *mut Line, // malloc'd array, capacity = doc_cap
    doc_cap: usize,
    nlines: usize,
    path: [u8; 96],
    path_len: usize,
    modified: bool,
    // caret
    line: usize,
    col: usize,
    scroll: usize,
    // window
    id: i64,
    w: i32,
    h: i32,
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

impl Doc {
    fn path_str(&self) -> &str {
        core::str::from_utf8(&self.path[..self.path_len]).unwrap_or(DEF_SAVE)
    }
}

/// Ensure the line can hold `need` bytes (capacity doubling).
fn line_reserve(l: &mut Line, need: u64) -> bool {
    let need = if need == 0 { 1 } else { need };
    if need <= l.cap {
        return true;
    }
    let mut cap = if l.cap == 0 { 32 } else { l.cap };
    while cap < need {
        cap *= 2;
        if cap > MAX_LINE {
            return false;
        }
    }
    let nb = unsafe { heap::realloc(l.buf, cap) };
    if nb.is_null() {
        return false;
    }
    l.buf = nb;
    l.cap = cap;
    true
}

fn line_free(l: Line) {
    if !l.buf.is_null() {
        unsafe { heap::free(l.buf) };
    }
}

fn doc_reserve(d: &mut Doc, need: usize) -> bool {
    if need <= d.doc_cap {
        return true;
    }
    let mut cap = if d.doc_cap == 0 { 16 } else { d.doc_cap };
    while cap < need {
        cap *= 2;
    }
    let nb = unsafe { heap::realloc(d.lines as *mut u8, (cap as u64) * 24) } as *mut Line;
    if nb.is_null() {
        return false;
    }
    d.lines = nb;
    d.doc_cap = cap;
    true
}

/// Append an empty line at the end of the table.
fn doc_push_line(d: &mut Doc) -> bool {
    if !doc_reserve(d, d.nlines + 1) {
        return false;
    }
    unsafe {
        *d.lines.add(d.nlines) = Line {
            buf: core::ptr::null_mut(),
            cap: 0,
            len: 0,
        };
    }
    d.nlines += 1;
    true
}

/// Remove line `idx` from the table (freeing its buffer).
fn doc_remove_line(d: &mut Doc, idx: usize) {
    if idx >= d.nlines {
        return;
    }
    unsafe {
        let l = *d.lines.add(idx);
        line_free(l);
        core::ptr::copy(
            d.lines.add(idx + 1),
            d.lines.add(idx),
            d.nlines - idx - 1,
        );
    }
    d.nlines -= 1;
}

fn line_at(d: &Doc, idx: usize) -> Line {
    unsafe { *d.lines.add(idx) }
}

/// Load a file into the document (replacing whatever was there).
fn load_file(d: &mut Doc, path: &str) -> bool {
    // free the old document
    for i in 0..d.nlines {
        unsafe { line_free(*d.lines.add(i)) };
    }
    d.nlines = 0;
    d.line = 0;
    d.col = 0;
    d.scroll = 0;
    d.modified = false;

    let fd = file_open(path, O_RDWR);
    if fd < 0 {
        // new empty document
        return doc_push_line(d);
    }
    let mut chunk = [0u8; 512];
    let mut pending: Line = Line { buf: core::ptr::null_mut(), cap: 0, len: 0 };
    loop {
        let n = file_read(fd, &mut chunk);
        if n <= 0 {
            break;
        }
        for i in 0..n as usize {
            let b = chunk[i];
            if b == b'\n' {
                if !doc_push_line_with(d, &mut pending) {
                    file_close(fd);
                    return false;
                }
                pending = Line { buf: core::ptr::null_mut(), cap: 0, len: 0 };
            } else {
                if pending.len + 1 > MAX_LINE {
                    continue;
                }
                let need = pending.len + 1;
                if !line_reserve(&mut pending, need) {
                    continue;
                }
                unsafe {
                    *pending.buf.add(pending.len as usize) = b;
                }
                pending.len += 1;
            }
        }
    }
    file_close(fd);
    // the last line (no trailing newline) still counts
    if !doc_push_line_with(d, &mut pending) {
        return false;
    }
    true
}

/// Push `l` as the next line of the table.
fn doc_push_line_with(d: &mut Doc, l: &mut Line) -> bool {
    if !doc_reserve(d, d.nlines + 1) {
        line_free(*l);
        return false;
    }
    unsafe {
        let taken = *l;
        *d.lines.add(d.nlines) = taken;
    }
    d.nlines += 1;
    *l = Line { buf: core::ptr::null_mut(), cap: 0, len: 0 };
    true
}

/// Save the document through the v1.6 file syscalls, line by line.
fn save_file(d: &mut Doc) -> bool {
    let path = d.path_str();
    let fd = file_open(path, O_CREATE | O_RDWR);
    if fd < 0 {
        return false;
    }
    let mut ok = true;
    let mut nl = [b'\n'];
    for i in 0..d.nlines {
        let l = line_at(d, i);
        if l.len > 0 {
            let bytes = unsafe { core::slice::from_raw_parts(l.buf, l.len as usize) };
            if file_write(fd, bytes) != l.len as i64 {
                ok = false;
                break;
            }
        }
        if i + 1 < d.nlines && file_write(fd, &nl) != 1 {
            ok = false;
            break;
        }
    }
    file_close(fd);
    if ok {
        d.modified = false;
    }
    ok
}

fn total_bytes(d: &Doc) -> u64 {
    let mut t = 0u64;
    for i in 0..d.nlines {
        t += line_at(d, i).len + 1; // + newline
    }
    if t > 0 {
        t - 1
    } else {
        0
    }
}

fn visible_rows(w: i32, h: i32) -> usize {
    let ch = h - 24;
    let avail = ch - TOP - STATUS_H;
    if avail < ROW_H {
        1
    } else {
        (avail / ROW_H) as usize
    }
}

fn draw(d: &mut Doc, caret_on: bool) {
    let id = d.id;
    let (w, h) = (d.w, d.h);
    let cw = w - 4;
    let ch = h - 24;
    gui_rect(id, 0, 0, cw, ch, BG);
    // toolbar: [SAVE] path MOD
    gui_rect(id, 0, 0, cw, TOOL_H, TOOLBAR);
    gui_rect(id, 4, 2, 44, 16, SAVE_BTN);
    gui_text(id, 12, 6, "SAVE", 0x0E1218);
    gui_text(id, 56, 6, d.path_str(), if d.modified { CYAN } else { DIM });
    if d.modified {
        gui_text(id, 56 + (d.path_len as i32 + 1) * CELL + 8, 6, "* modified", CYAN);
    }

    // text lines with scroll
    let vis = visible_rows(w, h);
    let mut y = TOP;
    for row in 0..vis {
        let idx = d.scroll + row;
        if idx >= d.nlines {
            break;
        }
        let l = line_at(d, idx);
        if l.len > 0 {
            let bytes = unsafe { core::slice::from_raw_parts(l.buf, l.len as usize) };
            // clamp to what fits horizontally
            let cols = ((cw - 16) / CELL) as usize;
            let n = bytes.len().min(cols);
            let s = core::str::from_utf8(&bytes[..n]).unwrap_or("");
            gui_text(id, 8, y + 3, s, TXT);
        }
        y += ROW_H;
    }

    // caret (blink handled by the caller)
    if caret_on && d.line >= d.scroll && d.line < d.scroll + vis {
        let cy = TOP + ((d.line - d.scroll) as i32) * ROW_H;
        let cx = 8 + (d.col as i32) * CELL;
        gui_rect(id, cx, cy + 2, CELL - 2, ROW_H - 3, CARET);
    }

    // status bar
    gui_rect(id, 0, ch - STATUS_H, cw, STATUS_H, STATUS);
    let mut b1 = [0u8; 20];
    let mut b2 = [0u8; 20];
    let mut b3 = [0u8; 20];
    let mut off = 6;
    gui_text(id, off, ch - STATUS_H + 4, "LN ", DIM);
    off += 3 * CELL;
    gui_text(id, off, ch - STATUS_H + 4, num(d.line as u64 + 1, &mut b1), CYAN);
    off += (b1.len() as i32) * CELL + CELL;
    gui_text(id, off, ch - STATUS_H + 4, "COL ", DIM);
    off += 4 * CELL;
    gui_text(id, off, ch - STATUS_H + 4, num(d.col as u64, &mut b2), CYAN);
    off += (b2.len() as i32) * CELL + 2 * CELL;
    gui_text(id, off, ch - STATUS_H + 4, "lines ", DIM);
    off += 6 * CELL;
    gui_text(id, off, ch - STATUS_H + 4, num(d.nlines as u64, &mut b3), TXT);
    let mut b4 = [0u8; 20];
    let mut b5 = [0u8; 20];
    gui_text(id, cw - 8 - 5 * CELL, ch - STATUS_H + 4, "heap ", DIM);
    match heap::sbrk(0) {
        Some(br) if br > 0x2000_0000 => {
            gui_text(id, cw - 8, ch - STATUS_H + 4, num(br - 0x2000_0000, &mut b4), DIM);
        }
        _ => {
            gui_text(id, cw - 8, ch - STATUS_H + 4, num(0, &mut b5), DIM);
        }
    }

    // v2.3: present the frame. All rect/text calls above painted into the
    // window's back buffer; this swap makes the complete frame visible at
    // once (no half-drawn intermediate states on the desktop).
    gui_flush(id);
}

/// Keep the caret inside the document and the scroll around it.
fn clamp_caret(d: &mut Doc, vis: usize) {
    if d.nlines == 0 {
        d.line = 0;
        d.col = 0;
        return;
    }
    if d.line >= d.nlines {
        d.line = d.nlines - 1;
    }
    let l = line_at(d, d.line);
    if d.col > l.len as usize {
        d.col = l.len as usize;
    }
    if d.line < d.scroll {
        d.scroll = d.line;
    }
    if vis > 0 && d.line >= d.scroll + vis {
        d.scroll = d.line + 1 - vis;
    }
}

fn insert_char(d: &mut Doc, ch: u8) {
    if !doc_reserve(d, d.nlines + 1) {
        return;
    }
    unsafe {
        let mut l = *d.lines.add(d.line);
        let need = l.len + 1;
        if !line_reserve(&mut l, need) {
            *d.lines.add(d.line) = l;
            return;
        }
        // shift the tail right, then write the char
        let mut i = l.len;
        while i > d.col as u64 {
            *l.buf.add(i as usize) = *l.buf.add((i - 1) as usize);
            i -= 1;
        }
        *l.buf.add(d.col) = ch;
        l.len += 1;
        *d.lines.add(d.line) = l;
    }
    d.col += 1;
    d.modified = true;
}

fn delete_back(d: &mut Doc) {
    if d.col > 0 {
        unsafe {
            let mut l = *d.lines.add(d.line);
            if l.len == 0 || d.col as u64 > l.len {
                return;
            }
            let mut i = d.col as u64 - 1;
            while i < l.len - 1 {
                *l.buf.add(i as usize) = *l.buf.add((i + 1) as usize);
                i += 1;
            }
            l.len -= 1;
            *d.lines.add(d.line) = l;
        }
        d.col -= 1;
        d.modified = true;
    } else if d.line > 0 {
        // join with the previous line
        let prev_idx = d.line - 1;
        unsafe {
            let mut prev = *d.lines.add(prev_idx);
            let cur = *d.lines.add(d.line);
            let need = prev.len + cur.len;
            if !line_reserve(&mut prev, need) {
                *d.lines.add(prev_idx) = prev;
                return;
            }
            core::ptr::copy(cur.buf, prev.buf.add(prev.len as usize), cur.len as usize);
            prev.len += cur.len;
            *d.lines.add(prev_idx) = prev;
        }
        doc_remove_line(d, d.line);
        d.line = prev_idx;
        d.modified = true;
    }
}

/// v2.5: forward delete (Delete key) — removes the char at the caret,
/// or pulls the next line up when the caret sits at end of line.
fn delete_forward(d: &mut Doc) {
    let l = line_at(d, d.line);
    if (d.col as u64) < l.len {
        unsafe {
            let mut l = *d.lines.add(d.line);
            // close the gap from the caret on (the tail shifts left)
            let mut i = d.col as u64;
            while i < l.len - 1 {
                *l.buf.add(i as usize) = *l.buf.add((i + 1) as usize);
                i += 1;
            }
            l.len -= 1;
            *d.lines.add(d.line) = l;
        }
        d.modified = true;
    } else if d.line + 1 < d.nlines {
        // join with the NEXT line: append its bytes to this one
        unsafe {
            let mut cur = *d.lines.add(d.line);
            let next = *d.lines.add(d.line + 1);
            let need = cur.len + next.len;
            if !line_reserve(&mut cur, need) {
                *d.lines.add(d.line) = cur;
                return;
            }
            core::ptr::copy(next.buf, cur.buf.add(cur.len as usize), next.len as usize);
            cur.len += next.len;
            *d.lines.add(d.line) = cur;
        }
        doc_remove_line(d, d.line + 1);
        d.modified = true;
    }
}

/// v2.5: navigation keys — arrows, Home/End, PgUp/PgDn. Pure caret
/// movement: clamping and scroll-follow happen in clamp_caret.
fn nav_key(d: &mut Doc, k: u8, vis: usize) {
    match k {
        K_LEFT => {
            if d.col > 0 {
                d.col -= 1;
            } else if d.line > 0 {
                // wrap to the end of the previous line
                d.line -= 1;
                d.col = line_at(d, d.line).len as usize;
            }
        }
        K_RIGHT => {
            let l = line_at(d, d.line);
            if (d.col as u64) < l.len {
                d.col += 1;
            } else if d.line + 1 < d.nlines {
                // wrap to the start of the next line
                d.line += 1;
                d.col = 0;
            }
        }
        K_UP => {
            if d.line > 0 {
                d.line -= 1;
            }
        }
        K_DOWN => {
            if d.line + 1 < d.nlines {
                d.line += 1;
            }
        }
        K_HOME => d.col = 0,
        K_END => d.col = line_at(d, d.line).len as usize,
        K_PGUP => {
            let page = vis.saturating_sub(1).max(1);
            d.line = d.line.saturating_sub(page);
        }
        K_PGDN => {
            let page = vis.saturating_sub(1).max(1);
            d.line = (d.line + page).min(d.nlines.saturating_sub(1));
        }
        _ => {}
    }
}

fn split_line(d: &mut Doc) {
    if !doc_reserve(d, d.nlines + 1) {
        return;
    }
    unsafe {
        let l = *d.lines.add(d.line);
        let tail_len = l.len - d.col as u64;
        // a fresh line for the tail
        let mut nl = Line { buf: core::ptr::null_mut(), cap: 0, len: tail_len };
        if !line_reserve(&mut nl, tail_len.max(1)) {
            return;
        }
        if tail_len > 0 {
            core::ptr::copy(l.buf.add(d.col), nl.buf, tail_len as usize);
        }
        // shrink the current line in place
        let mut head = l;
        head.len = d.col as u64;
        // move the table to open a slot at d.line + 1
        core::ptr::copy(
            d.lines.add(d.line + 1),
            d.lines.add(d.line + 2),
            d.nlines - d.line - 1,
        );
        *d.lines.add(d.line) = head;
        *d.lines.add(d.line + 1) = nl;
    }
    d.nlines += 1;
    d.line += 1;
    d.col = 0;
    d.modified = true;
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    // resolve the save path from argv[1] (or the default)
    let mut pbuf = [0u8; 96];
    let path: &str = if argc >= 2 {
        arg_str(cstr_into(unsafe { *argv.add(1) }, &mut pbuf))
    } else {
        DEF_SAVE
    };
    if path.is_empty() || path.len() > 90 {
        write("[edit ] bad path - exit 3\n");
        exit(3);
    }

    let listbuf = heap::malloc(64); // tiny warmup allocation (exercises sbrk)
    if listbuf.is_null() {
        write("[edit ] heap malloc failed - exit 2\n");
        exit(2);
    }

    let id = gui_open("text editor", WIN_X, WIN_Y, WIN_W, WIN_H);
    if id < 0 {
        write("[edit ] no gui desktop - run 'gui' first, exit 1\n");
        exit(1);
    }
    let (w, h) = gui_geo(id).unwrap_or((WIN_W, WIN_H));

    let mut d = Doc {
        lines: core::ptr::null_mut(),
        doc_cap: 0,
        nlines: 0,
        path: [0; 96],
        path_len: path.len(),
        modified: false,
        line: 0,
        col: 0,
        scroll: 0,
        id,
        w,
        h,
    };
    d.path[..path.len()].copy_from_slice(path.as_bytes());

    if !load_file(&mut d, path) {
        write("[edit ] load failed - exit 4\n");
        let _ = gui_close(id);
        exit(4);
    }
    let _ = listbuf;
    draw(&mut d, true);

    let mut last_blink: u64 = 0;
    let mut caret_on = true;

    loop {
        sleep_ms(25);

        // caret blink (uptime-driven, no kernel support needed)
        let up = glm_user::uptime_ms() / 450;
        if up != last_blink {
            last_blink = up;
            caret_on = !caret_on;
            draw(&mut d, caret_on);
        }

        loop {
            let ev = gui_event(id);
            if ev == u64::MAX {
                // window/desktop gone: free the document and retire
                for i in 0..d.nlines {
                    unsafe { line_free(*d.lines.add(i)) };
                }
                if !d.lines.is_null() {
                    unsafe { heap::free(d.lines as *mut u8) };
                }
                unsafe { heap::free(listbuf) };
                exit(0);
            }
            let (t, a, b) = gui_ev_parts(ev);
            if t == 0 {
                break;
            }
            if t == EV_CLOSE {
                let _ = gui_close(id);
                for i in 0..d.nlines {
                    unsafe { line_free(*d.lines.add(i)) };
                }
                if !d.lines.is_null() {
                    unsafe { heap::free(d.lines as *mut u8) };
                }
                unsafe { heap::free(listbuf) };
                exit(0);
            }
            if t == EV_RESIZE {
                if let Some((nw, nh)) = gui_geo(id) {
                    d.w = nw;
                    d.h = nh;
                }
                let vis = visible_rows(d.w, d.h); clamp_caret(&mut d, vis);
                draw(&mut d, true);
            }
            if t == EV_CLICK {
                let cx = a as i16 as i32;
                let cy = b as i16 as i32;
                // [SAVE] button
                if cy >= 2 && cy < 18 && cx >= 4 && cx < 48 {
                    save_file(&mut d);
                    draw(&mut d, true);
                } else if cy >= TOP && cy < d.h - 24 - STATUS_H {
                    // click into the text: position the caret
                    let row = ((cy - TOP) / ROW_H) as usize;
                    let col = (((cx - 8) / CELL).max(0)) as usize;
                    let idx = d.scroll + row;
                    if idx < d.nlines {
                        d.line = idx;
                        let l = line_at(&d, idx);
                        let maxcol = l.len as usize;
                        d.col = if col > maxcol { maxcol } else { col };
                    }
                    draw(&mut d, true);
                }
            }
            if t == EV_KEY {
                let ch = a as u8;
                match ch {
                    0x08 => {
                        delete_back(&mut d);
                        let vis = visible_rows(d.w, d.h); clamp_caret(&mut d, vis);
                        draw(&mut d, true);
                    }
                    0x0A => {
                        split_line(&mut d);
                        let vis = visible_rows(d.w, d.h); clamp_caret(&mut d, vis);
                        draw(&mut d, true);
                    }
                    // v2.5: navigation keys (arrows / Home / End / PgUp / PgDn)
                    K_UP | K_DOWN | K_LEFT | K_RIGHT | K_HOME | K_END | K_PGUP | K_PGDN => {
                        let vis = visible_rows(d.w, d.h);
                        nav_key(&mut d, ch, vis);
                        clamp_caret(&mut d, vis);
                        draw(&mut d, true);
                    }
                    // v2.5: forward delete
                    K_DEL => {
                        delete_forward(&mut d);
                        let vis = visible_rows(d.w, d.h); clamp_caret(&mut d, vis);
                        draw(&mut d, true);
                    }
                    0x20..=0x7E => {
                        insert_char(&mut d, ch);
                        draw(&mut d, true);
                    }
                    _ => {}
                }
            }
        }
    }
}
