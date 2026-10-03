//! fmgr — v2.1: a ring-3 GUI file manager for GLM OS.
//!
//! The first desktop application that BROWSES the persistent disk: it
//! lists directories (SYS_FILE_LIST), navigates into subdirectories and
//! back up, opens text files in a built-in viewer (SYS_FILE_OPEN/READ),
//! and repaints through the v1.2 GUI window syscalls. The directory
//! table and the viewer buffer live in MALLOC'd memory (v2.1 heap) —
//! a listing of N entries allocates exactly N records, the viewer
//! allocates exactly the file size — nothing here is a fixed BSS array.
//!
//! Layout (window content coordinates; content = w-4 x h-24):
//!   row 0..20   toolbar: [UP] [REFRESH] <path>          (browse mode)
//!               or [BACK] [^] [v] <file name>           (viewer mode)
//!   row 24..    entry rows, 14 px each: "<D> NAME" cyan for dirs,
//!               "NAME ... SIZE" for files, dirs sorted first
//!   bottom 16   status bar: "N entries | M heap bytes in use"
//!
//! Events: EV_CLICK hits buttons/rows, EV_KEY 'u' = up, 'b' = back,
//! 'r' = refresh, EV_RESIZE repaints at the new size, EV_CLOSE/[x]
//! frees the heap and exits 0.

#![no_std]
#![no_main]

use glm_user::{
    exit, file_list, file_open, file_read, file_close, fmt_u64, gui_close, gui_event, gui_flush,
    gui_geo, gui_open, gui_rect, gui_text, heap, sleep_ms, write, gui_ev_parts,
    EV_CLICK, EV_CLOSE, EV_KEY, EV_RESIZE, O_RDWR,
};

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(101)
}

// palette
const TOOLBAR: u32 = 0x2A3140;
const BTN: u32 = 0x3A4254;
const BTN_TXT: u32 = 0xE6EDF3;
const BG: u32 = 0x14181E;
const STATUS: u32 = 0x1B2029;
const VIEW_BG: u32 = 0x0E1218;
const BACK_BTN: u32 = 0x5EE28D; // distinct green: the test finds the viewer by it
const CYAN: u32 = 0x46E2E2;
const TXT: u32 = 0xC6CCD8;
const DIM: u32 = 0x7E8494;

const WIN_X: i32 = 140;
const WIN_Y: i32 = 90;
const WIN_W: i32 = 400;
const WIN_H: i32 = 280;

const TOOL_H: i32 = 20;
const ROW_H: i32 = 14;
const LIST_TOP: i32 = 24;
const STATUS_H: i32 = 16;

const MAX_ENTRIES: usize = 128;
const MAX_NAME: usize = 13; // FAT 8.3 + NUL margin
const VIEW_MAX: u64 = 16 * 1024;

/// One directory entry (24 bytes), stored in a malloc'd array.
#[repr(C)]
struct Ent {
    kind: u32, // 0 file, 1 dir
    size: u32,
    name: [u8; MAX_NAME],
}

struct Fmgr {
    id: i64,
    w: i32,
    h: i32,
    path: [u8; 96],
    path_len: usize,
    ents: *mut Ent,
    count: usize,
    listbuf: *mut u8, // 4 KiB scratch for file_list
    scroll: usize,
    // viewer state
    viewing: bool,
    vbuf: *mut u8,
    vlen: u64,
    vscroll: usize,
}

impl Fmgr {
    fn path_str(&self) -> &str {
        core::str::from_utf8(&self.path[..self.path_len]).unwrap_or("/")
    }
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

/// path + "/" + name into the fixed path buffer.
fn push_name(f: &mut Fmgr, name: &[u8]) -> bool {
    let mut len = f.path_len;
    if len > 0 && f.path[len - 1] != b'/' {
        if len + 1 >= f.path.len() {
            return false;
        }
        f.path[len] = b'/';
        len += 1;
    }
    if len + name.len() >= f.path.len() {
        return false;
    }
    f.path[len..len + name.len()].copy_from_slice(name);
    f.path_len = len + name.len();
    true
}

/// Strip the last component: "/HOME/DOCS" -> "/HOME", "/HOME" -> "/".
fn pop_name(f: &mut Fmgr) {
    if f.path_len <= 1 {
        f.path_len = 1;
        f.path[0] = b'/';
        return;
    }
    let mut i = f.path_len - 1;
    if i > 0 && f.path[i] == b'/' {
        i -= 1;
    }
    while i > 0 && f.path[i] != b'/' {
        i -= 1;
    }
    f.path_len = if i == 0 { 1 } else { i };
}

/// Re-list the current directory: file_list into the malloc'd scratch,
/// then copy the records into a malloc'd entry array (dirs first,
/// alphabetical — makes row positions deterministic for tests).
fn reload(f: &mut Fmgr) {
    // drop the old table and viewer, if any
    drop_entries(f);
    f.viewing = false;
    if !f.vbuf.is_null() {
        unsafe { heap::free(f.vbuf) };
        f.vbuf = core::ptr::null_mut();
        f.vlen = 0;
    }
    f.scroll = 0;

    let scratch = unsafe { core::slice::from_raw_parts_mut(f.listbuf, 4096) };
    let n = file_list(f.path_str(), scratch, MAX_ENTRIES as usize);
    if n < 0 {
        f.count = 0;
        return;
    }

    // first pass: how many records
    let mut off = 0usize;
    let mut cnt = 0usize;
    for _ in 0..n {
        match glm_user::file_record(scratch, off) {
            Some((_, _, _, next)) => {
                cnt += 1;
                off = next;
            }
            None => break,
        }
    }
    if cnt == 0 {
        f.count = 0;
        return;
    }

    // one malloc for exactly cnt records — the whole point of v2.1
    let ents = heap::malloc((cnt as u64) * 24) as *mut Ent;
    if ents.is_null() {
        f.count = 0;
        return;
    }
    let mut off = 0usize;
    for i in 0..cnt {
        if let Some((kind, name, size, next)) = glm_user::file_record(scratch, off) {
            let e = unsafe { &mut *ents.add(i) };
            e.kind = kind as u32;
            e.size = size;
            for (j, b) in e.name.iter_mut().enumerate() {
                *b = if j < name.len() && j < MAX_NAME { name[j] } else { 0 };
            }
            off = next;
        }
    }

    // sort: dirs first, then names (insertion sort; cnt <= 128)
    for i in 1..cnt {
        let mut j = i;
        while j > 0 {
            let a = unsafe { &*ents.add(j - 1) };
            let b = unsafe { &*ents.add(j) };
            let a_key = (a.kind == 0) as u32; // dirs (1) sort before files (0)? invert:
            let b_key = (b.kind == 0) as u32;
            let swap = if a_key != b_key {
                a_key > b_key
            } else {
                name_of(a) > name_of(b)
            };
            if !swap {
                break;
            }
            unsafe {
                core::ptr::swap(ents.add(j - 1), ents.add(j));
            }
            j -= 1;
        }
    }
    f.ents = ents;
    f.count = cnt;
}

fn name_of(e: &Ent) -> &[u8] {
    let mut n = 0usize;
    while n < MAX_NAME && e.name[n] != 0 {
        n += 1;
    }
    &e.name[..n]
}

fn drop_entries(f: &mut Fmgr) {
    if !f.ents.is_null() {
        unsafe { heap::free(f.ents as *mut u8) };
        f.ents = core::ptr::null_mut();
        f.count = 0;
    }
}

/// Open a file into the viewer: malloc exactly its size, read, show.
fn open_viewer(f: &mut Fmgr, idx: usize) {
    if f.ents.is_null() || idx >= f.count {
        return;
    }
    let (name, size) = unsafe {
        let e = &*f.ents.add(idx);
        (name_of(e), e.size as u64)
    };
    let want = size.min(VIEW_MAX);
    if want == 0 {
        return;
    }
    let buf = heap::malloc(want);
    if buf.is_null() {
        return;
    }
    // full path for the open
    let mut full = [0u8; 128];
    let plen = f.path_len;
    full[..plen].copy_from_slice(&f.path[..plen]);
    let mut flen = plen;
    if flen > 0 && full[flen - 1] != b'/' {
        full[flen] = b'/';
        flen += 1;
    }
    if flen + name.len() >= full.len() {
        unsafe { heap::free(buf) };
        return;
    }
    full[flen..flen + name.len()].copy_from_slice(name);
    flen += name.len();

    let path = core::str::from_utf8(&full[..flen]).unwrap_or("");
    let fd = file_open(path, O_RDWR);
    if fd < 0 {
        unsafe { heap::free(buf) };
        return;
    }
    let got = file_read(fd, unsafe { core::slice::from_raw_parts_mut(buf, want as usize) });
    file_close(fd);
    if got < 0 {
        unsafe { heap::free(buf) };
        return;
    }
    f.vbuf = buf;
    f.vlen = got as u64;
    f.viewing = true;
    f.vscroll = 0;
}

/// Content height available for rows.
fn list_rows(_w: i32, h: i32) -> i32 {
    let ch = h - 24;
    let avail = ch - LIST_TOP - STATUS_H;
    if avail < ROW_H {
        1
    } else {
        avail / ROW_H
    }
}

fn draw(f: &mut Fmgr) {
    let id = f.id;
    let (w, h) = (f.w, f.h);
    let cw = w - 4;
    let ch = h - 24;
    gui_rect(id, 0, 0, cw, ch, if f.viewing { VIEW_BG } else { BG });
    // toolbar
    gui_rect(id, 0, 0, cw, TOOL_H, TOOLBAR);
    if f.viewing {
        gui_rect(id, 4, 2, 44, 16, BACK_BTN);
        gui_text(id, 12, 6, "BACK", 0x0E1218);
        gui_rect(id, 52, 2, 28, 16, BTN);
        gui_text(id, 58, 6, "^", BTN_TXT);
        gui_rect(id, 84, 2, 28, 16, BTN);
        gui_text(id, 90, 6, "v", BTN_TXT);
        gui_text(id, 120, 6, "viewer - v2.1 heap buffer", DIM);
    } else {
        gui_rect(id, 4, 2, 28, 16, BTN);
        gui_text(id, 10, 6, "UP", BTN_TXT);
        gui_rect(id, 36, 2, 64, 16, BTN);
        gui_text(id, 44, 6, "REFRESH", BTN_TXT);
        gui_text(id, 108, 6, f.path_str(), CYAN);
    }

    let vis = list_rows(w, h);
    if f.viewing {
        // render text lines from the malloc'd viewer buffer
        if !f.vbuf.is_null() {
            let data = unsafe { core::slice::from_raw_parts(f.vbuf, f.vlen as usize) };
            let cols = ((cw - 16) / 8).max(8) as usize;
            let mut line_y = LIST_TOP;
            let mut row = 0usize;
            let mut off = 0usize;
            let mut buf = [0u8; 96];
            while off <= data.len() && row < (vis as usize + f.vscroll) {
                let end = data[off..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|p| off + p)
                    .unwrap_or(data.len());
                if row >= f.vscroll {
                    let mut n = 0usize;
                    let mut i = off;
                    while i < end && n < cols {
                        let b = data[i];
                        buf[n] = if b.is_ascii_graphic() || b == b' ' { b } else { b'.' };
                        n += 1;
                        i += 1;
                    }
                    gui_text(id, 8, line_y, core::str::from_utf8(&buf[..n]).unwrap_or(""), TXT);
                    line_y += ROW_H;
                    if line_y > ch - STATUS_H {
                        break;
                    }
                }
                row += 1;
                off = if end < data.len() { end + 1 } else { data.len() + 1 };
            }
        }
    } else if !f.ents.is_null() {
        // directory rows
        let mut y = LIST_TOP;
        let first = f.scroll;
        let last = (f.scroll + vis as usize).min(f.count);
        for i in first..last {
            let e = unsafe { &*f.ents.add(i) };
            let name = name_of(e);
            if e.kind == 1 {
                gui_text(id, 8, y + 3, "<D>", CYAN);
                gui_text(id, 40, y + 3, core::str::from_utf8(name).unwrap_or("?"), TXT);
            } else {
                gui_text(id, 8, y + 3, "-", DIM);
                gui_text(id, 40, y + 3, core::str::from_utf8(name).unwrap_or("?"), TXT);
                let mut b = [0u8; 20];
                let s = num(e.size as u64, &mut b);
                gui_text(id, cw - 8 - (s.len() as i32) * 8, y + 3, s, DIM);
            }
            y += ROW_H;
            if y > ch - STATUS_H {
                break;
            }
        }
    }

    // status bar
    gui_rect(id, 0, ch - STATUS_H, cw, STATUS_H, STATUS);
    let mut b1 = [0u8; 20];
    let mut b2 = [0u8; 20];
    if f.viewing {
        let msg = "viewer: file bytes live in a malloc'd buffer";
        gui_text(id, 6, ch - STATUS_H + 4, msg, DIM);
    } else {
        gui_text(
            id,
            6,
            ch - STATUS_H + 4,
            "entries: ",
            TXT,
        );
        gui_text(id, 6 + 9 * 8, ch - STATUS_H + 4, num(f.count as u64, &mut b1), CYAN);
        gui_text(id, 140, ch - STATUS_H + 4, "heap bytes: ", DIM);
        gui_text(id, 140 + 12 * 8, ch - STATUS_H + 4, num(heap_in_use(), &mut b2), DIM);
    }

    // v2.3: present the frame (per-window double buffering -- the swap
    // publishes everything painted above as one complete image)
    gui_flush(id);
}

/// Sum of the sizes of live mallocs we own (entries + scratch + viewer).
fn heap_in_use() -> u64 {
    // cheap honest number for the status line: scratch is 4 KiB, entries
    // are 24 bytes each, viewer buffer adds vlen — computed by callers
    // is overkill; just report the break delta via sbrk(0).
    match heap::sbrk(0) {
        Some(b) if b > 0x2000_0000 => b - 0x2000_0000,
        _ => 0,
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // heap allocations BEFORE the window: scratch first
    let listbuf = heap::malloc(4096);
    if listbuf.is_null() {
        write("[fmgr ] heap malloc failed - exit 2\n");
        exit(2);
    }

    let id = gui_open("file manager", WIN_X, WIN_Y, WIN_W, WIN_H);
    if id < 0 {
        write("[fmgr ] no gui desktop - run 'gui' first, exit 1\n");
        exit(1);
    }
    let (w, h) = gui_geo(id).unwrap_or((WIN_W, WIN_H));

    let mut f = Fmgr {
        id,
        w,
        h,
        path: [0; 96],
        path_len: 1,
        ents: core::ptr::null_mut(),
        count: 0,
        listbuf,
        scroll: 0,
        viewing: false,
        vbuf: core::ptr::null_mut(),
        vlen: 0,
        vscroll: 0,
    };
    f.path[0] = b'/';
    reload(&mut f);
    draw(&mut f);

    loop {
        sleep_ms(20);
        loop {
            let ev = gui_event(id);
            if ev == u64::MAX {
                // window/desktop gone; free what we own and retire
                drop_entries(&mut f);
                unsafe { heap::free(f.listbuf) };
                if !f.vbuf.is_null() {
                    unsafe { heap::free(f.vbuf) };
                }
                exit(0);
            }
            let (t, a, b) = gui_ev_parts(ev);
            if t == 0 {
                break;
            }
            if t == EV_CLOSE {
                let _ = gui_close(id);
                drop_entries(&mut f);
                unsafe { heap::free(f.listbuf) };
                if !f.vbuf.is_null() {
                    unsafe { heap::free(f.vbuf) };
                }
                exit(0);
            }
            if t == EV_RESIZE {
                if let Some((nw, nh)) = gui_geo(id) {
                    f.w = nw;
                    f.h = nh;
                }
                f.scroll = 0;
                draw(&mut f);
            }
            if t == EV_KEY {
                match a as u8 {
                    b'u' if !f.viewing => {
                        pop_name(&mut f);
                        reload(&mut f);
                        draw(&mut f);
                    }
                    b'r' if !f.viewing => {
                        reload(&mut f);
                        draw(&mut f);
                    }
                    b'b' if f.viewing => {
                        f.viewing = false;
                        if !f.vbuf.is_null() {
                            unsafe { heap::free(f.vbuf) };
                            f.vbuf = core::ptr::null_mut();
                            f.vlen = 0;
                        }
                        draw(&mut f);
                    }
                    _ => {}
                }
            }
            if t == EV_CLICK {
                let cx = a as i16 as i32;
                let cy = b as i16 as i32;
                let cw = f.w - 4;
                let ch = f.h - 24;
                if f.viewing {
                    // BACK / ^ / v
                    if cy >= 2 && cy < 18 {
                        if cx >= 4 && cx < 48 {
                            f.viewing = false;
                            if !f.vbuf.is_null() {
                                unsafe { heap::free(f.vbuf) };
                                f.vbuf = core::ptr::null_mut();
                                f.vlen = 0;
                            }
                            draw(&mut f);
                        } else if cx >= 52 && cx < 80 {
                            if f.vscroll > 0 {
                                f.vscroll -= 1;
                                draw(&mut f);
                            }
                        } else if cx >= 84 && cx < 112 {
                            f.vscroll += 1;
                            draw(&mut f);
                        }
                    }
                } else {
                    // toolbar: UP / REFRESH
                    if cy >= 2 && cy < 18 {
                        if cx >= 4 && cx < 32 {
                            pop_name(&mut f);
                            reload(&mut f);
                            draw(&mut f);
                        } else if cx >= 36 && cx < 100 {
                            reload(&mut f);
                            draw(&mut f);
                        }
                    } else if cy >= LIST_TOP && cy < ch - STATUS_H {
                        // rows
                        let row = ((cy - LIST_TOP) / ROW_H) as usize;
                        let idx = f.scroll + row;
                        if idx < f.count && !f.ents.is_null() {
                            let (kind, name): (u32, [u8; MAX_NAME]) = unsafe {
                                let e = &*f.ents.add(idx);
                                (e.kind, e.name)
                            };
                            if kind == 1 {
                                if push_name(&mut f, &name[..name.iter().position(|&c| c == 0).unwrap_or(MAX_NAME)]) {
                                    reload(&mut f);
                                    draw(&mut f);
                                }
                            } else {
                                open_viewer(&mut f, idx);
                                draw(&mut f);
                            }
                        }
                    }
                }
            }
        }
    }
}
