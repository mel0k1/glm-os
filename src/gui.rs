//! GLM OS v1.2 — the framebuffer GUI: compositor, resizable windows, ring 3.
//!
//! Architecture (v1.2):
//!   * everything renders into an off-screen back buffer (one heap-allocated
//!     frame in the framebuffer's native packed-pixel format);
//!   * only dirty rectangles are blitted to the visible framebuffer
//!     (`Console::blit_from`), so mid-frame tearing is gone;
//!   * the mouse cursor is stamped on top of the front buffer AFTER each
//!     blit -- the scene buffer never contains the cursor, so the pointer
//!     can neither flicker nor smear and needs no save/restore arrays;
//!   * windows are dynamic now: the two kernel windows (system monitor,
//!     about) plus any number of USER windows owned by ring-3 tasks, each
//!     with its own backing store for the content area;
//!   * every window has a resize grip in its bottom-right corner: press,
//!     drag, release -- content redraws around the new geometry;
//!   * ring-3 apps draw through `int 0x80` GUI syscalls (open / rect /
//!     text / event / geo / close) into their window's backing store; the
//!     compositor blends that store like any other window. Input (clicks,
//!     keys, close, resize) is queued per window and polled by the app;
//!   * the bottom taskbar behaves like a real desktop's: a start button
//!     with a popup menu (launch windows, run the ring-3 demo, reboot,
//!     halt), task buttons for every open window and a tray with a
//!     net-activity led plus an uptime clock;
//!   * `Esc` or closing every window hands the text screen back through
//!     `console::redraw_all_global()`.
//!
//! Locking: one global GUI_LOCK guards the whole desktop state (windows,
//! z-order, dirty list, back buffer). The desktop loop takes it once per
//! frame; user syscalls take it per call. Order: GUI_LOCK -> CONSOLE ->
//! SERIAL. SCHED_LOCK is never taken while holding GUI_LOCK except from
//! the start-menu spawn action, which is the documented GUI_LOCK ->
//! SCHED_LOCK direction (the task-exit hook releases SCHED_LOCK first).

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::console::{self, font, Rgb, CONSOLE};
use crate::cpu::keyboard;
use crate::cpu::mouse;
use crate::cpu::pit;
use crate::sched;
use crate::sync::Spinlock;

// ---------------- palette (Rgb is the source of truth) ----------------

const DESK_TOP: Rgb = Rgb(26, 30, 56);
const DESK_BOT: Rgb = Rgb(10, 12, 22);
const MARK: Rgb = Rgb(48, 58, 100); // desktop watermark
const TASKBAR_BG: Rgb = Rgb(10, 11, 18);
const TASKBAR_EDGE: Rgb = Rgb(58, 64, 88);
const WIN_BG: Rgb = Rgb(24, 27, 36);
const WIN_EDGE: Rgb = Rgb(82, 88, 110);
const SHADOW: Rgb = Rgb(4, 4, 7);
const TITLE: Rgb = Rgb(40, 96, 210); // focused title bar
const TITLE_DIM: Rgb = Rgb(62, 68, 90); // unfocused title bar
const TITLE_FG: Rgb = Rgb(238, 240, 246);
const TEXT: Rgb = Rgb(198, 204, 216);
const DIM: Rgb = Rgb(126, 132, 148);
const ACCENT: Rgb = Rgb(94, 226, 141);
const CYAN: Rgb = Rgb(70, 226, 226);
const WARN: Rgb = Rgb(255, 220, 93);
const RED: Rgb = Rgb(235, 90, 90);
const BTN: Rgb = Rgb(54, 58, 72);
const BTN_EDGE: Rgb = Rgb(122, 128, 150);
const BTN_FG: Rgb = Rgb(235, 235, 235);
const CLOSE_BG: Rgb = Rgb(200, 58, 58);
const MENU_BG: Rgb = Rgb(18, 20, 30);
const MENU_EDGE: Rgb = Rgb(70, 76, 100);
const HOVER: Rgb = Rgb(44, 52, 80);
const TB_IDLE: Rgb = Rgb(26, 30, 42); // taskbar button, inactive window
const TB_ACTIVE: Rgb = Rgb(44, 52, 78); // taskbar button, focused window
const WHITE: Rgb = Rgb(245, 245, 245); // cursor fill
const BLACK: Rgb = Rgb(12, 12, 16); // cursor outline
const GRIP: Rgb = Rgb(150, 158, 182); // resize grip diagonal
const TERM_BG: Rgb = Rgb(12, 13, 18); // terminal window background

/// All palette colors pre-packed into the framebuffer's native format
/// (packed once at `gui` entry against the live console).
#[derive(Clone, Copy)]
struct C {
    desk_top: u32,
    desk_bot: u32,
    mark: u32,
    taskbar: u32,
    taskbar_edge: u32,
    win_bg: u32,
    win_edge: u32,
    shadow: u32,
    title: u32,
    title_dim: u32,
    title_fg: u32,
    text: u32,
    dim: u32,
    accent: u32,
    cyan: u32,
    warn: u32,
    red: u32,
    btn: u32,
    btn_edge: u32,
    btn_fg: u32,
    close_bg: u32,
    menu_bg: u32,
    menu_edge: u32,
    hover: u32,
    tb_idle: u32,
    tb_active: u32,
    white: u32,
    black: u32,
    grip: u32,
    term_bg: u32,
    /// v1.4: the full 16-color console palette, packed — terminal windows
    /// render cells tagged with console palette indexes.
    pal: [u32; 16],
}

impl C {
    fn build(con: &console::Console) -> Self {
        C {
            desk_top: con.pack_rgb(&DESK_TOP),
            desk_bot: con.pack_rgb(&DESK_BOT),
            mark: con.pack_rgb(&MARK),
            taskbar: con.pack_rgb(&TASKBAR_BG),
            taskbar_edge: con.pack_rgb(&TASKBAR_EDGE),
            win_bg: con.pack_rgb(&WIN_BG),
            win_edge: con.pack_rgb(&WIN_EDGE),
            shadow: con.pack_rgb(&SHADOW),
            title: con.pack_rgb(&TITLE),
            title_dim: con.pack_rgb(&TITLE_DIM),
            title_fg: con.pack_rgb(&TITLE_FG),
            text: con.pack_rgb(&TEXT),
            dim: con.pack_rgb(&DIM),
            accent: con.pack_rgb(&ACCENT),
            cyan: con.pack_rgb(&CYAN),
            warn: con.pack_rgb(&WARN),
            red: con.pack_rgb(&RED),
            btn: con.pack_rgb(&BTN),
            btn_edge: con.pack_rgb(&BTN_EDGE),
            btn_fg: con.pack_rgb(&BTN_FG),
            close_bg: con.pack_rgb(&CLOSE_BG),
            menu_bg: con.pack_rgb(&MENU_BG),
            menu_edge: con.pack_rgb(&MENU_EDGE),
            hover: con.pack_rgb(&HOVER),
            tb_idle: con.pack_rgb(&TB_IDLE),
            tb_active: con.pack_rgb(&TB_ACTIVE),
            white: con.pack_rgb(&WHITE),
            black: con.pack_rgb(&BLACK),
            grip: con.pack_rgb(&GRIP),
            term_bg: con.pack_rgb(&TERM_BG),
            pal: {
                let mut a = [0u32; 16];
                for (i, col) in console::PALETTE.iter().enumerate() {
                    a[i] = con.pack_rgb(col);
                }
                a
            },
        }
    }
}

// ---------------- geometry ----------------

const MON_W: usize = 380;
const MON_H: usize = 244;
const ABOUT_W: usize = 330;
const ABOUT_H: usize = 190;
const TERM_W: usize = 464; // v1.4: terminal windows (56 cols x 19 rows)
const TERM_H: usize = 304;
const TITLE_H: usize = 22;
const TASKBAR_H: usize = 28;
const START_W: usize = 56;
const TASKBTN_W: usize = 132;
const LINE_H: usize = 14; // scale-1 text line pitch
const MARGIN: usize = 12;
const MIN_WIN_W: i32 = 140;
const MIN_WIN_H: i32 = 90;
const GRIP_SIZE: i32 = 14;

const MENU_W: usize = 190;
const MENU_ITEM_H: usize = 20;
const MENU_ITEMS: [&str; 6] = [
    "terminal",
    "system monitor",
    "about glm os",
    "run ring-3 demo",
    "reboot",
    "halt",
];
/// 4px top pad + 4 launch items + 6px separator + 2 power items + 2px pad
const MENU_H: usize = 4 + 4 * MENU_ITEM_H + 6 + 2 * MENU_ITEM_H + 2;

const SAVE_W: usize = 16;
const SAVE_H: usize = 24;

const MONITOR: usize = 0;
const ABOUT: usize = 1;
const MAX_WINS: usize = 10;

/// 12x18 arrow, 'W' = white fill, 'K' = dark outline, '.' = transparent.
const CURSOR: [&str; 18] = [
    "W...........",
    "WW..........",
    "WKW.........",
    "WKKW........",
    "WKKKW.......",
    "WKKKKW......",
    "WKKKKKW.....",
    "WKKKKKKW....",
    "WKKKKKKKW...",
    "WKKKKKKKKW..",
    "WKKKKKKKKKW.",
    "WKKKKWWWWWW.",
    "WKKWKW......",
    "WKW.WKW.....",
    "KW..WKW.....",
    "W....WKW....",
    "......WK....",
    "............",
];

fn glyph_for(ch: u8) -> [u8; 8] {
    if (font::FONT_FIRST..font::FONT_FIRST + 95).contains(&ch) {
        font::FONT8[(ch - font::FONT_FIRST) as usize]
    } else {
        font::FONT8[(b'?' - font::FONT_FIRST) as usize]
    }
}

// ---------------- rects ----------------

type Rect = (i32, i32, i32, i32); // x, y, w, h

fn point_in(x: i32, y: i32, r: Rect) -> bool {
    x >= r.0 && x < r.0 + r.2 && y >= r.1 && y < r.1 + r.3
}

fn rects_intersect(a: Rect, b: Rect) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn intersect_screen(r: Rect, w: usize, h: usize) -> Rect {
    let (mut x, mut y, mut ww, mut hh) = r;
    if x < 0 {
        ww += x;
        x = 0;
    }
    if y < 0 {
        hh += y;
        y = 0;
    }
    let (sw, sh) = (w as i32, h as i32);
    if x >= sw || y >= sh || ww <= 0 || hh <= 0 {
        return (0, 0, 0, 0);
    }
    if x + ww > sw {
        ww = sw - x;
    }
    if y + hh > sh {
        hh = sh - y;
    }
    if ww <= 0 || hh <= 0 {
        return (0, 0, 0, 0);
    }
    (x, y, ww, hh)
}

fn push_rect(v: &mut Vec<Rect>, r: Rect) {
    if r.2 > 0 && r.3 > 0 {
        v.push(r);
    }
}

// ---------------- painter (draws into the back buffer) ----------------

struct Painter<'a> {
    buf: &'a mut [u32],
    w: usize,
    h: usize,
    clip: Rect,
}

impl<'a> Painter<'a> {
    fn new(buf: &'a mut [u32], w: usize, h: usize, clip: Rect) -> Self {
        Painter { buf, w, h, clip }
    }

    #[inline]
    fn px(&mut self, x: i32, y: i32, col: u32) {
        let (cx, cy, cw, ch) = self.clip;
        if x < cx || x >= cx + cw || y < cy || y >= cy + ch {
            return;
        }
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        self.buf[y as usize * self.w + x as usize] = col;
    }

    /// Horizontal fill with the fast whole-row path (gradient rows).
    fn fill_row(&mut self, y: i32, xa: i32, xb: i32, col: u32) {
        let (cx, cy, cw, ch) = self.clip;
        if y < cy || y >= cy + ch || y < 0 || y >= self.h as i32 {
            return;
        }
        let a = xa.max(cx).max(0);
        let b = xb.min(cx + cw).min(self.w as i32);
        if b <= a {
            return;
        }
        let base = y as usize * self.w;
        self.buf[base + a as usize..base + b as usize].fill(col);
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, w: i32, h: i32, col: u32) {
        for y in y0..y0 + h {
            self.fill_row(y, x0, x0 + w, col);
        }
    }

    /// Blit a row-major `src` (width `sw`) at dst (dx, dy), size bw x bh,
    /// honoring the clip region. Used to blend user window backing stores.
    fn blit(&mut self, src: &[u32], sw: usize, dx: i32, dy: i32, bw: i32, bh: i32) {
        if bw <= 0 || bh <= 0 || sw == 0 {
            return;
        }
        let (cx, cy, cw, ch) = self.clip;
        let x0 = dx.max(cx).max(0);
        let y0 = dy.max(cy).max(0);
        let x1 = (dx + bw).min(cx + cw).min(self.w as i32);
        let y1 = (dy + bh).min(cy + ch).min(self.h as i32);
        if x1 <= x0 || y1 <= y0 {
            return; // fully clipped out: nothing to blend
        }
        for y in y0..y1 {
            let sy = (y - dy) as usize;
            let sx0 = (x0 - dx) as usize;
            let sx1 = (x1 - dx) as usize;
            if sy >= bh as usize || sx1 > sw {
                continue;
            }
            let dst_base = y as usize * self.w;
            let src_off = sy * sw;
            let n = sx1 - sx0;
            if src_off + sx0 + n <= src.len() {
                self.buf[dst_base + x0 as usize..dst_base + x0 as usize + n]
                    .copy_from_slice(&src[src_off + sx0..src_off + sx0 + n]);
            }
        }
    }

    /// 8x8 glyph at an arbitrary pixel origin; `bg = None` keeps the
    /// existing pixels wherever the glyph is unset (transparent text).
    fn char(&mut self, ch: u8, x: i32, y: i32, fg: u32, bg: Option<u32>, scale: usize) {
        let glyph = glyph_for(ch);
        for gy in 0..8usize {
            let bits = glyph[gy];
            for gx in 0..8usize {
                let on = bits & (1 << gx) != 0;
                if !on && bg.is_none() {
                    continue;
                }
                let col = if on { fg } else { bg.unwrap() };
                for dy in 0..scale {
                    for dx in 0..scale {
                        self.px(
                            x + ((gx * scale + dx) as i32),
                            y + ((gy * scale + dy) as i32),
                            col,
                        );
                    }
                }
            }
        }
    }

    fn str8(&mut self, s: &str, x: i32, y: i32, fg: u32, bg: Option<u32>, scale: usize) {
        for (i, &b) in s.as_bytes().iter().enumerate() {
            self.char(b, x + (i * 8 * scale) as i32, y, fg, bg, scale);
        }
    }
}

fn lerp_rgb(a: Rgb, b: Rgb, t: u32, den: u32) -> Rgb {
    let d = den.max(1) as i32;
    let ch = |x: u8, y: u8| -> u8 {
        (x as i32 + ((y as i32 - x as i32) * (t as i32)) / d).clamp(0, 255) as u8
    };
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

// ---------------- windows & desktop state ----------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Monitor,
    About,
    User,
    Term,
}

/// v1.4: terminal window state — a scrollback of closed lines with
/// per-character console-palette colors, the live (unterminated) line, a
/// keystroke queue fed by the compositor and a caret blink flag. Kernel
/// windows keep an empty one.
struct TermState {
    lines: Vec<Vec<(u8, u8)>>, // closed lines: (char, palette fg index)
    cur: Vec<(u8, u8)>,        // current unterminated line
    fg: u8,                    // palette index for newly written chars
    input: VecDeque<u8>,       // keystrokes routed by the compositor
    caret: bool,               // blink phase, rendered at the end of `cur`
    cols: usize,               // wrap width at feed time
    scroll_cap: usize,         // max closed lines kept
}

impl TermState {
    fn empty() -> Self {
        Self {
            lines: Vec::new(),
            cur: Vec::new(),
            fg: crate::console::GLM_GRAY,
            input: VecDeque::new(),
            caret: true,
            cols: 40,
            scroll_cap: 400,
        }
    }

    fn flush_cur(&mut self) {
        let l = core::mem::take(&mut self.cur);
        self.lines.push(l);
        if self.lines.len() > self.scroll_cap {
            self.lines.remove(0);
        }
    }

    fn push_cell(&mut self, ch: u8) {
        self.cur.push((ch, self.fg));
        if self.cur.len() >= self.cols {
            self.flush_cur(); // hard wrap at the feed-time width
        }
    }

    /// Byte-stream writer with console semantics: '\n' closes the line,
    /// 0x08 pops (the shell's "\x08 \x08" erase idiom lands correctly),
    /// '\t' becomes four spaces, anything non-printable becomes '?'.
    fn feed(&mut self, s: &str) {
        for &b in s.as_bytes() {
            match b {
                b'\n' => self.flush_cur(),
                b'\r' => {}
                0x08 => {
                    self.cur.pop();
                }
                b'\t' => {
                    for _ in 0..4 {
                        self.push_cell(b' ');
                    }
                }
                _ => {
                    let ch = if b.is_ascii_graphic() || b == b' ' { b } else { b'?' };
                    self.push_cell(ch);
                }
            }
        }
    }

    fn backspace(&mut self) {
        self.cur.pop();
    }
}

/// One desktop window. Kernel windows (monitor/about) toggle open/closed
/// forever; user windows are created by ring-3 syscalls, carry a backing
/// store for the content area and an input event queue, and die with
/// their owner task.
struct Win {
    id: u32,
    kind: Kind,
    owner: u64, // pid, user windows only
    title: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    open: bool,
    minimized: bool,
    buf: Vec<u32>,       // user window content pixels (row-major, cw x ch)
    ev: VecDeque<u64>,   // packed input events (user windows only)
    term: TermState,     // terminal windows only
}

impl Win {
    fn short(&self) -> &str {
        match self.kind {
            Kind::Monitor => "system monitor",
            Kind::About => "about",
            Kind::User => {
                let t = self.title.as_str();
                if t.is_empty() { "ring-3 app" } else { t }
            }
            Kind::Term => "terminal",
        }
    }

    fn content_rect(&self) -> Rect {
        (
            self.x + 2,
            self.y + TITLE_H as i32 + 1,
            self.w - 4,
            self.h - TITLE_H as i32 - 2,
        )
    }

    fn full_rect(&self) -> Rect {
        (self.x, self.y, self.w + 4, self.h + 4)
    }

    fn body_rect(&self) -> Rect {
        (self.x, self.y, self.w, self.h)
    }

    fn grip_rect(&self) -> Rect {
        (self.x + self.w - GRIP_SIZE, self.y + self.h - GRIP_SIZE, GRIP_SIZE, GRIP_SIZE)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reason {
    Esc,
    AllClosed,
    Reboot,
    Halt,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::Esc => "esc",
            Reason::AllClosed => "all-closed",
            Reason::Reboot => "reboot",
            Reason::Halt => "halt",
        }
    }
}

/// Read-only snapshot of the monitor window's dynamic numbers, computed
/// once per compose() so draw fns never need the whole Desk.
struct MonStats {
    open_wins: usize,
    user_wins: usize,
    fps: u32,
    live: bool,
    cur_x: i32,
    cur_y: i32,
}

/// All mutable desktop state except the back buffer, guarded by GUI_LOCK.
/// `compose` splits Desk into `sc` and `back` so scene reads and buffer
/// writes never fight the borrow checker.
struct Scene {
    w: usize,
    h: usize,
    c: C,
    grad: Vec<u32>,
    wins: Vec<Win>,
    z: Vec<usize>,
    dirty: Vec<Rect>,
    cur_x: i32,
    cur_y: i32,
    prev_left: bool,
    dragging: Option<(usize, i32, i32)>, // (win slot, grab dx, grab dy)
    resizing: Option<usize>,             // win slot
    menu_open: bool,
    menu_hover: Option<usize>,
    next_id: u32,
    moves: u64,
    clicks: u64,
    drags: u64,
    keys: u64,
    stats_at: u64,
    clock_at: u64,
    frames: u32,
    fps: u32,
    fps_at: u64,
    net_prev: u64,
    net_led: bool,
    live: bool,
}

struct Desk {
    sc: Scene,
    back: Vec<u32>,
}

static GUI_LOCK: Spinlock<()> = Spinlock::new(());
static mut DESK: Option<Desk> = None;

fn desk() -> Option<&'static mut Desk> {
    unsafe {
        let p = core::ptr::addr_of_mut!(DESK);
        (*p).as_mut()
    }
}

impl Scene {
    fn taskbar_y(&self) -> i32 {
        self.h as i32 - TASKBAR_H as i32
    }

    fn clamp_cur(&mut self) {
        self.cur_x = self.cur_x.clamp(0, self.w as i32 - 2);
        self.cur_y = self.cur_y.clamp(0, self.h as i32 - 2);
    }

    fn clamp_win(&mut self, i: usize) {
        let (ww, wh) = (self.wins[i].w, self.wins[i].h);
        let xmax = (self.w as i32 - ww - 2).max(0);
        let ymax = (self.taskbar_y() - wh - 2).max(0);
        self.wins[i].x = self.wins[i].x.clamp(0, xmax);
        self.wins[i].y = self.wins[i].y.clamp(0, ymax);
    }

    /// The focused window = the topmost open, non-minimized one.
    fn active_idx(&self) -> Option<usize> {
        for &i in self.z.iter().rev() {
            if self.wins[i].open && !self.wins[i].minimized {
                return Some(i);
            }
        }
        None
    }

    fn any_open(&self) -> bool {
        self.wins.iter().any(|w| w.open)
    }

    fn find_by_id(&self, id: u32) -> Option<usize> {
        self.wins.iter().position(|w| w.id == id)
    }

    /// Push a dirty rect for window `i` (its full footprint incl. shadow).
    fn dirty_win(&mut self, i: usize) {
        push_rect(&mut self.dirty, self.wins[i].full_rect());
    }

    fn mon_stats(&self) -> MonStats {
        MonStats {
            open_wins: self.wins.iter().filter(|w| w.open).count(),
            user_wins: self.wins.iter().filter(|w| w.open && w.kind == Kind::User).count(),
            fps: self.fps,
            live: self.live,
            cur_x: self.cur_x,
            cur_y: self.cur_y,
        }
    }
}

// ---------------- rect helpers over the scene ----------------

fn start_rect(sc: &Scene) -> Rect {
    (6, sc.taskbar_y() + 4, START_W as i32, 20)
}

fn taskbar_rect(sc: &Scene) -> Rect {
    (0, sc.taskbar_y(), sc.w as i32, TASKBAR_H as i32)
}

fn tray_rect(sc: &Scene) -> Rect {
    (sc.w as i32 - 220, sc.taskbar_y(), 220, TASKBAR_H as i32)
}

fn menu_panel_rect(sc: &Scene) -> Rect {
    (
        6,
        sc.taskbar_y() - MENU_H as i32 - 4,
        MENU_W as i32,
        MENU_H as i32,
    )
}

/// Menu footprint incl. the 1px border and 3px drop shadow.
fn menu_full_rect(sc: &Scene) -> Rect {
    let (x, y, _, _) = menu_panel_rect(sc);
    (x, y, MENU_W as i32 + 4, MENU_H as i32 + 4)
}

/// Top y of menu item `k` (0..2 above the separator, 3..4 below).
fn menu_item_y(sc: &Scene, k: usize) -> i32 {
    let my = menu_panel_rect(sc).1;
    if k < 4 {
        // launch block: terminal / monitor / about / ring-3 demo
        my + 4 + (k as i32) * MENU_ITEM_H as i32
    } else {
        // separator + power pair: reboot / halt
        my + 4 + 4 * MENU_ITEM_H as i32 + 6 + ((k - 4) as i32) * MENU_ITEM_H as i32
    }
}

fn menu_item_rect(sc: &Scene, k: usize) -> Rect {
    (
        6 + 2,
        menu_item_y(sc, k),
        MENU_W as i32 - 4,
        MENU_ITEM_H as i32,
    )
}

fn menu_item_at(sc: &Scene) -> Option<usize> {
    for k in 0..MENU_ITEMS.len() {
        if point_in(sc.cur_x, sc.cur_y, menu_item_rect(sc, k)) {
            return Some(k);
        }
    }
    None
}

/// The nth taskbar task-button rect for the nth open window (slot order).
fn task_btn_rect(sc: &Scene, n: usize) -> Rect {
    let bx = 6 + START_W as i32 + 6 + (n * (TASKBTN_W + 4)) as i32;
    (bx, sc.taskbar_y() + 4, TASKBTN_W as i32, 20)
}

/// Slot indices of open windows, in taskbar (slot) order.
fn open_task_slots(sc: &Scene) -> Vec<usize> {
    sc.wins
        .iter()
        .enumerate()
        .filter(|(_, w)| w.open)
        .map(|(i, _)| i)
        .collect()
}

// ---------------- actions ----------------

/// Raise window `i` to the top of the z-order, un-minimize it and mark
/// every affected region (all title bars + taskbar) dirty.
fn focus(sc: &mut Scene, i: usize) {
    if sc.z.last() == Some(&i) && !sc.wins[i].minimized {
        return;
    }
    if let Some(pos) = sc.z.iter().position(|&k| k == i) {
        sc.z.remove(pos);
    }
    sc.z.push(i);
    sc.wins[i].minimized = false;
    for j in 0..sc.wins.len() {
        if sc.wins[j].open && !sc.wins[j].minimized {
            push_rect(&mut sc.dirty, sc.wins[j].full_rect());
        }
    }
    let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
}

fn open_win(sc: &mut Scene, kind_idx: usize) {
    if sc.wins[kind_idx].open {
        // already open: just restore + focus
        sc.wins[kind_idx].minimized = false;
        crate::klog!("gui: restore {} window", sc.wins[kind_idx].short());
        focus(sc, kind_idx);
        return;
    }
    sc.wins[kind_idx].open = true;
    // cascade from the monitor window when it is around, else center
    if kind_idx == ABOUT {
        let (mx, my) = (sc.wins[MONITOR].x, sc.wins[MONITOR].y);
        if sc.wins[MONITOR].open {
            sc.wins[ABOUT].x = mx + 36;
            sc.wins[ABOUT].y = my + 36;
        } else {
            sc.wins[ABOUT].x = (sc.w as i32 - ABOUT_W as i32) / 2;
            sc.wins[ABOUT].y = (sc.taskbar_y() - ABOUT_H as i32) / 2 - 24;
        }
        sc.wins[ABOUT].w = ABOUT_W as i32;
        sc.wins[ABOUT].h = ABOUT_H as i32;
    }
    sc.clamp_win(kind_idx);
    crate::klog!("gui: launch {} window", sc.wins[kind_idx].short());
    focus(sc, kind_idx);
}

fn minimize_win(sc: &mut Scene, i: usize) {
    sc.wins[i].minimized = true;
    crate::klog!("gui: minimize {} window", sc.wins[i].short());
    sc.dirty_win(i);
    // focus the topmost remaining open window, if any
    if let Some(top) = sc.active_idx() {
        focus(sc, top);
    } else {
        let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
    }
}

fn task_btn_click(sc: &mut Scene, i: usize) {
    let active = sc.active_idx() == Some(i);
    if active && !sc.wins[i].minimized {
        minimize_win(sc, i);
    } else {
        crate::klog!("gui: restore {} window", sc.wins[i].short());
        focus(sc, i);
    }
}

/// Queue an event for a user window (slot), dropping the oldest on overflow.
fn push_event(sc: &mut Scene, i: usize, ev: u64) {
    const EV_CAP: usize = 32;
    let q = &mut sc.wins[i].ev;
    if q.len() >= EV_CAP {
        q.pop_front();
    }
    q.push_back(ev);
}

/// Left-button press dispatch. Returns a Reason when the desktop session
/// itself should end (all windows closed / reboot / halt).
fn on_click(sc: &mut Scene) -> Option<Reason> {
    let (x, y) = (sc.cur_x, sc.cur_y);

    // 1) start menu is up: items act, clicks outside dismiss it
    if sc.menu_open {
        if let Some(k) = menu_item_at(sc) {
            sc.menu_open = false;
            sc.menu_hover = None;
            let mr = menu_full_rect(sc);
    push_rect(&mut sc.dirty, mr);
            return match k {
                0 => {
                    // v1.4: spawn a terminal session straight from the desktop
                    crate::klog!("gui: spawning terminal session from start menu");
                    if !crate::term::launch() {
                        crate::klog!("gui: terminal spawn failed (task table full?)");
                    }
                    None
                }
                1 => {
                    open_win(sc, MONITOR);
                    None
                }
                2 => {
                    open_win(sc, ABOUT);
                    None
                }
                3 => {
                    // v1.2: launch the ring-3 GUI demo straight from the desktop
                    crate::klog!("gui: spawning ring-3 gui demo from start menu");
                    match crate::user::task::spawn_user_elf("/BIN/GUIDEMO.ELF") {
                        Ok(pid) => crate::klog!("gui: ring-3 gui demo spawned as pid {}", pid),
                        Err(e) => crate::klog!("gui: gui demo spawn failed: {}", e),
                    }
                    // the spawn printed to the text console underneath us:
                    // recompose the whole screen next frame
                    push_rect(&mut sc.dirty, (0, 0, sc.w as i32, sc.h as i32));
                    None
                }
                4 => Some(Reason::Reboot),
                _ => Some(Reason::Halt),
            };
        }
        if !point_in(x, y, menu_full_rect(sc)) {
            sc.menu_open = false;
            sc.menu_hover = None;
            let mr = menu_full_rect(sc);
    push_rect(&mut sc.dirty, mr);
        }
        return None;
    }

    // 2) taskbar: start button, then task buttons
    if y >= sc.taskbar_y() {
        if point_in(x, y, start_rect(sc)) {
            sc.menu_open = true;
            sc.menu_hover = None;
            crate::klog!("gui: start menu open");
            let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
            let mr = menu_full_rect(sc);
    push_rect(&mut sc.dirty, mr);
            return None;
        }
        for (n, i) in open_task_slots(sc).into_iter().enumerate() {
            if point_in(x, y, task_btn_rect(sc, n)) {
                task_btn_click(sc, i);
                return None;
            }
        }
        return None;
    }

    // 3) windows, topmost first: first click focuses, then buttons/drag act
    for k in (0..sc.z.len()).rev() {
        let i = sc.z[k];
        if !sc.wins[i].open || sc.wins[i].minimized {
            continue;
        }
        if !point_in(x, y, sc.wins[i].body_rect()) {
            continue;
        }
        if sc.active_idx() != Some(i) {
            focus(sc, i);
            return None;
        }
        let (wx, wy, ww, wh) = (sc.wins[i].x, sc.wins[i].y, sc.wins[i].w, sc.wins[i].h);
        let kind = sc.wins[i].kind;
        // close box [x]
        if point_in(x, y, (wx + ww - 24, wy + 4, 18, 14)) {
            sc.wins[i].open = false;
            crate::klog!("gui: close {} window", sc.wins[i].short());
            sc.dirty_win(i);
            if kind == Kind::User {
                // the app decides what to do; the slot stays for its events
                push_event(sc, i, pack_ev(EV_CLOSE, 0, 0));
            }
            if !sc.any_open() {
                return Some(Reason::AllClosed);
            }
            if let Some(top) = sc.active_idx() {
                focus(sc, top);
            } else {
                let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
            }
            return None;
        }
        // minimize box [-]
        if point_in(x, y, (wx + ww - 42, wy + 4, 18, 14)) {
            minimize_win(sc, i);
            return None;
        }
        // resize grip (bottom-right corner)
        if point_in(x, y, sc.wins[i].grip_rect()) {
            sc.resizing = Some(i);
            crate::klog!("gui: resize {} window started", sc.wins[i].short());
            return None;
        }
        // title bar (minus the button strip) starts a drag
        if y < wy + TITLE_H as i32 && x < wx + ww - 42 {
            sc.dragging = Some((i, x - wx, y - wy));
            return None;
        }
        // content click inside an active user window -> event for the app
        if kind == Kind::User {
            let (cx, cy) = (x - (wx + 2), y - (wy + TITLE_H as i32 + 1));
            push_event(sc, i, pack_ev(EV_CLICK, cx.max(0), cy.max(0)));
        }
        return None;
    }
    None
}

// ---------------- scene composition (into the back buffer) ----------------

/// Redraw `rect` of the scene into the back buffer: desktop gradient,
/// watermark, every visible window in z-order, then the taskbar and the
/// start menu on top. Splits Desk into scene + back so the painter can
/// own the back buffer while we read window state freely.
fn compose(d: &mut Desk, r: Rect) {
    let (rx, ry, rw, rh) = intersect_screen(r, d.sc.w, d.sc.h);
    if rw <= 0 || rh <= 0 {
        return;
    }
    let Desk { sc, back } = d;
    let (w, h) = (sc.w, sc.h);
    let c = sc.c;
    let active = sc.active_idx();
    let stats = sc.mon_stats();
    let z = sc.z.clone();
    let menu_open = sc.menu_open;

    let mut p = Painter::new(back, w, h, (rx, ry, rw, rh));
    for y in ry..ry + rh {
        if y >= 0 && (y as usize) < sc.grad.len() {
            let col = sc.grad[y as usize];
            p.fill_row(y, rx, rx + rw, col);
        }
    }
    draw_watermark(&mut p, &c, w, h);
    for &i in z.iter() {
        if sc.wins[i].open && !sc.wins[i].minimized {
            draw_window(&mut p, &c, &sc.wins[i], active == Some(i), &stats);
        }
    }
    // the taskbar always sits on top of the desktop pattern; the start
    // menu (if open) is the topmost layer of all
    draw_taskbar(&mut p, &c, sc);
    if menu_open {
        draw_menu(&mut p, &c, sc);
    }
}

fn draw_watermark(p: &mut Painter, c: &C, w: usize, h: usize) {
    let text = "GLM OS";
    let scale = 4;
    let tw = (text.len() * 8 * scale) as i32;
    let x = (w as i32 - tw) / 2;
    let y = h as i32 / 6; // above the default window position
    p.str8(text, x, y, c.mark, None, scale);
    let sub = "v1.4 - terminal windows on the desktop";
    let sw = (sub.len() * 8) as i32;
    p.str8(sub, (w as i32 - sw) / 2, y + 8 * scale as i32 + 14, c.mark, None, 1);
}

fn draw_window(p: &mut Painter, c: &C, win: &Win, active: bool, stats: &MonStats) {
    let (wx, wy) = (win.x, win.y);
    let (ww, wh) = (win.w, win.h);

    // drop shadow, then body + 1px border
    p.fill_rect(wx + 4, wy + 4, ww, wh, c.shadow);
    p.fill_rect(wx, wy, ww, wh, c.win_bg);
    p.fill_rect(wx, wy, ww, 1, c.win_edge);
    p.fill_rect(wx, wy + wh - 1, ww, 1, c.win_edge);
    p.fill_rect(wx, wy, 1, wh, c.win_edge);
    p.fill_rect(wx + ww - 1, wy, 1, wh, c.win_edge);

    // title bar + label
    let tb = if active { c.title } else { c.title_dim };
    p.fill_rect(wx + 1, wy + 1, ww - 2, TITLE_H as i32 - 1, tb);
    p.str8(&win.title, wx + 8, wy + 7, c.title_fg, Some(tb), 1);

    // minimize [-] and close [x] boxes
    let (mx, my) = (wx + ww - 42, wy + 4);
    p.fill_rect(mx, my, 18, 14, c.btn);
    p.str8("-", mx + 7, my + 3, c.btn_fg, Some(c.btn), 1);
    let (bx, by) = (wx + ww - 24, wy + 4);
    p.fill_rect(bx, by, 18, 14, c.close_bg);
    p.str8("x", bx + 6, by + 3, c.title_fg, Some(c.close_bg), 1);

    // content area
    let cr = win.content_rect();
    match win.kind {
        Kind::User => {
            let (cw, chh) = (cr.2.max(0) as usize, cr.3.max(0) as usize);
            if win.buf.len() == cw * chh && cw > 0 {
                p.blit(&win.buf, cw, cr.0, cr.1, cr.2, cr.3);
            } else {
                p.fill_rect(cr.0, cr.1, cr.2, cr.3, c.win_bg);
            }
        }
        Kind::Monitor => draw_monitor_content(p, c, win, stats),
        Kind::About => draw_about_content(p, c, win),
        Kind::Term => draw_term_content(p, c, win),
    }

    // resize grip: three diagonal steps in the bottom-right corner
    let gx = wx + ww - GRIP_SIZE;
    let gy = wy + wh - GRIP_SIZE;
    for s in 0..3 {
        let o = (s * 4) as i32;
        for k in 0..GRIP_SIZE - o {
            p.px(gx + o + k, gy + GRIP_SIZE - 1 - o - k, c.grip);
        }
    }
}

fn draw_monitor_content(p: &mut Painter, c: &C, win: &Win, stats: &MonStats) {
    let (wx, wy) = (win.x, win.y);
    let (ww, wh) = (win.w, win.h);

    p.fill_rect(
        wx + 2,
        wy + TITLE_H as i32 + 1,
        ww - 4,
        wh - TITLE_H as i32 - 2,
        c.win_bg,
    );

    let ms = pit::uptime_ms();
    let mut tasks_alive = 0usize;
    sched::for_each_task(|t| {
        if t.state != sched::State::Dead {
            tasks_alive += 1;
        }
    });
    let fs = crate::mem::frames::stats();
    let heap = crate::mem::heap::stats();
    let (irq_n, rx, tx, drop, _kicks) = crate::net::e1000::counters();

    let lines: [String; 8] = [
        format!(
            "uptime:  {}h {}m {}s",
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60
        ),
        format!(
            "tasks:   {} alive   ctx switches: {}",
            tasks_alive,
            sched::switches()
        ),
        format!("memory:  {}/{} frames used", fs.used, fs.total),
        match heap {
            Some(h) => format!(
                "heap:    {} KiB in use ({} allocs)",
                h.allocated / 1024,
                h.allocs
            ),
            None => String::from("heap:    offline"),
        },
        format!(
            "mouse:   {} packets   resync {}",
            mouse::packets(),
            mouse::resyncs()
        ),
        format!("net:     rx {} tx {} drop {} (irq {})", rx, tx, drop, irq_n),
        format!(
            "gui:     {} windows ({} ring-3), {} fps",
            stats.open_wins, stats.user_wins, stats.fps
        ),
        format!("cursor:  {}, {}", stats.cur_x, stats.cur_y),
    ];

    for (n, line) in lines.iter().enumerate() {
        let fg = if n == 6 { c.accent } else { c.text };
        p.str8(line, wx + MARGIN as i32, wy + TITLE_H as i32 + 8 + (n * LINE_H) as i32, fg, None, 1);
    }

    // live dot blinks on the 500 ms stats tick
    let dot = if stats.live { c.accent } else { c.win_bg };
    p.fill_rect(wx + ww - 14, wy + TITLE_H as i32 + 10, 6, 6, dot);

    p.str8(
        "esc exits - drag title, grip resizes",
        wx + MARGIN as i32,
        wy + wh - 18,
        c.warn,
        None,
        1,
    );
}

fn draw_about_content(p: &mut Painter, c: &C, win: &Win) {
    let (wx, wy) = (win.x, win.y);
    let (ww, wh) = (win.w, win.h);

    p.fill_rect(
        wx + 2,
        wy + TITLE_H as i32 + 1,
        ww - 4,
        wh - TITLE_H as i32 - 2,
        c.win_bg,
    );

    // logo lockup: "GLM" accent + "OS" white, scale 2 (inside the content area)
    p.str8("GLM", wx + 16, wy + 32, c.accent, None, 2);
    p.str8("OS", wx + 16 + 3 * 16 + 8, wy + 32, c.title_fg, None, 2);
    p.str8(
        "version 1.6.0 - ring-3 file syscalls: userland owns the disk",
        wx + 16,
        wy + 58,
        c.dim,
        None,
        1,
    );

    let body = [
        ("the operating system designed,", c.text),
        ("written and tested by GLM (z.ai)", c.text),
        ("", c.text),
        ("no_std | preemptive smp | ring3", c.text),
        ("udp+tcp+icmp networking | fat32", c.text),
        ("resizable windows + gui syscalls", c.accent),
        ("double buffered framebuffer gui", c.accent),
        ("", c.text),
        ("github.com/mel0k1/glm-os", c.cyan),
    ];
    for (n, (line, fg)) in body.iter().enumerate() {
        p.str8(
            line,
            wx + 16,
            wy + 78 + (n * LINE_H) as i32,
            *fg,
            None,
            1,
        );
    }
}

/// v1.4: render a terminal window — the visible tail of the scrollback,
/// the live input line and the block caret. Lines were wrapped at feed
/// time with the then-current width; cells beyond the visible width are
/// simply clipped.
fn draw_term_content(p: &mut Painter, c: &C, win: &Win) {
    let cr = win.content_rect();
    p.fill_rect(cr.0, cr.1, cr.2, cr.3, c.term_bg);
    if cr.2 < 20 || cr.3 < 20 {
        return;
    }

    let t = &win.term;
    let ox = cr.0 + 6;
    let oy = cr.1 + 5;
    let cols_vis = ((cr.2 - 12) / 8).max(1) as usize;
    let rows = ((cr.3 - 10) as usize / LINE_H).max(1);

    fn line_at(p: &mut Painter, c: &C, line: &[(u8, u8)], ox: i32, y: i32, cols_vis: usize) {
        for (j, &(ch, fg)) in line.iter().enumerate() {
            if j >= cols_vis {
                break;
            }
            let idx = (fg as usize).min(15);
            p.char(ch, ox + (j * 8) as i32, y, c.pal[idx], None, 1);
        }
    }

    // closed lines first (tail only), then the live line at the bottom
    let keep = rows.saturating_sub(1);
    let start = t.lines.len().saturating_sub(keep);
    let mut row = 0usize;
    for line in &t.lines[start..] {
        line_at(p, c, line, ox, oy + (row * LINE_H) as i32, cols_vis);
        row += 1;
    }
    let ly = oy + (row * LINE_H) as i32;
    line_at(p, c, &t.cur, ox, ly, cols_vis);

    // block caret at the end of the live line
    if t.caret && t.cur.len() < cols_vis {
        let cxx = ox + (t.cur.len() * 8) as i32;
        p.fill_rect(cxx, ly + 1, 8, LINE_H as i32 - 5, c.pal[(t.fg as usize).min(15)]);
    }
}

fn draw_taskbar(p: &mut Painter, c: &C, sc: &Scene) {
    let w = sc.w;
    let ty = sc.taskbar_y();

    p.fill_rect(0, ty, w as i32, TASKBAR_H as i32, c.taskbar);
    p.fill_rect(0, ty, w as i32, 1, c.taskbar_edge);

    // start button (pressed look while the menu is open)
    let sbg = if sc.menu_open { c.hover } else { c.btn };
    let (sx, sy) = (6, ty + 4);
    p.fill_rect(sx, sy, START_W as i32, 20, sbg);
    p.fill_rect(sx, sy, START_W as i32, 1, c.btn_edge);
    p.fill_rect(sx, sy + 19, START_W as i32, 1, c.btn_edge);
    p.fill_rect(sx, sy, 1, 20, c.btn_edge);
    p.fill_rect(sx + START_W as i32 - 1, sy, 1, 20, c.btn_edge);
    p.fill_rect(sx + 6, sy + 6, 8, 8, c.accent); // logo square
    p.str8("GLM", sx + 20, sy + 6, c.title_fg, Some(sbg), 1);

    // task buttons, stable order (slot order), open windows only
    for (n, i) in open_task_slots(sc).into_iter().enumerate() {
        let win = &sc.wins[i];
        let (bx, by, bw, bh) = task_btn_rect(sc, n);
        let active = sc.active_idx() == Some(i) && !win.minimized;
        let bg = if active { c.tb_active } else { c.tb_idle };
        p.fill_rect(bx, by, bw, bh, bg);
        if active {
            p.fill_rect(bx, by, bw, 2, c.accent);
        }
        p.fill_rect(bx, by, 1, bh, c.taskbar_edge);
        p.fill_rect(bx + bw - 1, by, 1, bh, c.taskbar_edge);
        p.fill_rect(bx, by + bh - 1, bw, 1, c.taskbar_edge);
        let fg = if win.minimized {
            c.dim
        } else if active {
            c.title_fg
        } else {
            c.text
        };
        // truncate the label to what fits (15 chars in 132px minus padding)
        let label = win.short();
        let cut = label.len().min(15);
        while !label.is_char_boundary(cut) {
            // ascii font only: cannot happen, but stay safe
            break;
        }
        let label = &label[..cut];
        p.str8(label, bx + 8, by + 6, fg, Some(bg), 1);
    }

    // tray: net-activity led + uptime clock + version tag
    let ms = pit::uptime_ms();
    let tray = format!(
        "{:02}:{:02}:{:02}  GLM 1.6",
        (ms / 3_600_000) % 100,
        (ms / 60_000) % 60,
        (ms / 1000) % 60
    );
    let tx = w as i32 - (tray.len() as i32) * 8 - 12;
    let tyi = ty as i32;
    p.fill_rect(tx - 14, tyi + 12, 4, 4, if sc.net_led { c.accent } else { c.btn_edge });
    p.str8(&tray, tx, tyi + 10, c.text, None, 1);
}

fn draw_menu(p: &mut Painter, c: &C, sc: &Scene) {
    let (mx, my, mw, mh) = menu_panel_rect(sc);

    // drop shadow, panel, border
    p.fill_rect(mx + 3, my + 3, mw, mh, c.shadow);
    p.fill_rect(mx, my, mw, mh, c.menu_bg);
    p.fill_rect(mx, my, mw, 1, c.menu_edge);
    p.fill_rect(mx, my + mh - 1, mw, 1, c.menu_edge);
    p.fill_rect(mx, my, 1, mh, c.menu_edge);
    p.fill_rect(mx + mw - 1, my, 1, mh, c.menu_edge);

    let icons = [c.accent, c.cyan, c.title, c.warn, c.text, c.red];
    for k in 0..MENU_ITEMS.len() {
        let iy = menu_item_y(sc, k);
        if k == 4 {
            // separator line between the launch items and the power pair
            p.fill_row(iy - 3, mx + 4, mx + mw - 4, c.menu_edge);
        }
        let hovered = sc.menu_hover == Some(k);
        if hovered {
            p.fill_rect(mx + 2, iy, mw - 4, MENU_ITEM_H as i32, c.hover);
        }
        p.fill_rect(mx + 8, iy + 6, 8, 8, icons[k]);
        let fg = if hovered { c.white } else { c.text };
        p.str8(MENU_ITEMS[k], mx + 24, iy + 6, fg, None, 1);
    }
}

/// Full-screen farewell painted by the start-menu "halt" item. The screen
/// freezes here; the scheduler keeps running with the console muted.
fn draw_halt_screen(d: &mut Desk) {
    let (w, h) = (d.sc.w, d.sc.h);
    let c = d.sc.c;
    let mut p = Painter::new(&mut d.back, w, h, (0, 0, w as i32, h as i32));
    for y in 0..h as i32 {
        if y >= 0 && (y as usize) < d.sc.grad.len() {
            let col = d.sc.grad[y as usize];
            p.fill_row(y, 0, w as i32, col);
        }
    }
    let t1 = "GLM OS 1.6";
    p.str8(
        t1,
        (w as i32 - (t1.len() * 8 * 3) as i32) / 2,
        h as i32 / 2 - 44,
        c.accent,
        None,
        3,
    );
    let t2 = "it is now safe to turn off your computer.";
    p.str8(
        t2,
        (w as i32 - (t2.len() * 8) as i32) / 2,
        h as i32 / 2 + 6,
        c.white,
        None,
        1,
    );
    let t3 = "halted from the start menu - scheduler still alive";
    p.str8(
        t3,
        (w as i32 - (t3.len() * 8) as i32) / 2,
        h as i32 / 2 + 26,
        c.dim,
        None,
        1,
    );
}

// ---------------- cursor (stamped on the front buffer) ----------------

/// Stamp the arrow sprite straight onto the visible framebuffer. Called
/// AFTER every blit that could touch the pointer's pixels -- the back
/// buffer never contains the cursor, so blits erase it for free.
fn stamp_cursor(c: &console::Console, x: i32, y: i32) {
    let (x0, y0) = (x.max(0) as usize, y.max(0) as usize);
    for (row, spec) in CURSOR.iter().enumerate() {
        for (col, ch) in spec.bytes().enumerate() {
            match ch {
                b'W' => c.px(x0 + col, y0 + row, &WHITE),
                b'K' => c.px(x0 + col, y0 + row, &BLACK),
                _ => {}
            }
        }
    }
}

// ---------------- ring-3 GUI syscalls (int 0x80) ----------------

pub const EV_NONE: u64 = 0;
pub const EV_CLOSE: u64 = 1;
pub const EV_CLICK: u64 = 2;
pub const EV_KEY: u64 = 3;
pub const EV_RESIZE: u64 = 4;

/// Pack an event into one u64: type in the high 32 bits, two 16-bit args.
fn pack_ev(t: u64, a: i32, b: i32) -> u64 {
    (t << 32) | ((a.max(0) as u64 & 0xFFFF) << 16) | (b.max(0) as u64 & 0xFFFF)
}

/// Decode helper for the userland library docs: (type, a, b).
#[allow(dead_code)]
pub fn unpack_ev(v: u64) -> (u64, u64, u64) {
    ((v >> 32) & 0xFFFF, (v >> 16) & 0xFFFF, v & 0xFFFF)
}

fn pack_rgb_col(rgb: u32) -> u32 {
    let col = Rgb(((rgb >> 16) & 0xFF) as u8, ((rgb >> 8) & 0xFF) as u8, (rgb & 0xFF) as u8);
    let mut g = CONSOLE.lock();
    g.as_ref().map(|c| c.pack_rgb(&col)).unwrap_or(rgb)
}

fn copy_user_bytes(uva: u64, len: usize) -> Option<Vec<u8>> {
    use crate::mem::paging::cr3;
    use crate::mem::vmm::AddressSpace;
    use crate::user::uaccess::read_user_bytes;
    if len == 0 {
        return Some(Vec::new());
    }
    let space = AddressSpace::from_pml4(cr3());
    let mut tmp = alloc::vec![0u8; len];
    if read_user_bytes(&space, uva, &mut tmp).is_err() {
        return None;
    }
    Some(tmp)
}

fn sanitize(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '?' })
        .collect()
}

/// SYS_GUI_OPEN(title_ptr, title_len, x|(y<<16), w|(h<<16)) -> id.
/// x/y < 0 (GUI_AUTO) means "place me automatically" (cascade).
pub fn sys_open(title_ptr: u64, title_len: u64, xy: u64, wh: u64) -> i64 {
    let title = match copy_user_bytes(title_ptr, (title_len as usize).min(24)) {
        Some(b) => sanitize(&b),
        None => return -1,
    };
    let mut x = (xy & 0xFFFF) as i32;
    let mut y = ((xy >> 16) & 0xFFFF) as i32;
    let mut w = (wh & 0xFFFF) as i32;
    let mut h = ((wh >> 16) & 0xFFFF) as i32;

    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return -1 };
    let sc = &mut d.sc;

    // reject nonsense sizes, clamp into the screen (taskbar respected)
    w = w.clamp(MIN_WIN_W, 1200);
    h = h.clamp(MIN_WIN_H, 800);
    w = w.min(sc.w as i32 - 8);
    h = h.min(sc.taskbar_y() - 8);
    if w < MIN_WIN_W || h < MIN_WIN_H {
        return -1;
    }
    if x < 0 || y < 0 {
        // auto placement (GUI_AUTO = -1): cascade from the monitor, else center
        if sc.wins[MONITOR].open {
            x = sc.wins[MONITOR].x + 48;
            y = sc.wins[MONITOR].y + 48;
        } else {
            x = (sc.w as i32 - w) / 2;
            y = (sc.taskbar_y() - h) / 2 - 24;
        }
    }

    // find a free closed user slot, else append a new window
    let slot = sc
        .wins
        .iter()
        .position(|win| win.kind == Kind::User && !win.open);
    let slot = match slot {
        Some(s) => s,
        None => {
            if sc.wins.len() >= MAX_WINS {
                return -1;
            }
            sc.wins.push(Win {
                id: 0,
                kind: Kind::User,
                owner: 0,
                title: String::new(),
                x: 0,
                y: 0,
                w,
                h,
                open: false,
                minimized: false,
                buf: Vec::new(),
                ev: VecDeque::new(),
                term: TermState::empty(),
            });
            sc.wins.len() - 1
        }
    };
    sc.next_id += 1;
    let id = sc.next_id;
    let cw = (w - 4).max(1) as usize;
    let ch = (h - TITLE_H as i32 - 2).max(1) as usize;
    let win = &mut sc.wins[slot];
    win.id = id;
    win.owner = sched::current_pid();
    win.title = title;
    win.x = x;
    win.y = y;
    win.w = w;
    win.h = h;
    win.open = true;
    win.minimized = false;
    win.buf.clear();
    win.buf.resize(cw * ch, 0);
    win.ev.clear();
    let wrect = win.full_rect();
    drop(win);
    sc.z.retain(|&k| k != slot);
    sc.z.push(slot);
    push_rect(&mut sc.dirty, wrect);
    let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
    crate::klog!(
        "gui: user window id={} opened by pid {} at ({},{}) {}x{}",
        id,
        sc.wins[slot].owner,
        x,
        y,
        w,
        h
    );
    id as i64
}

/// SYS_GUI_CLOSE(id): the owner retires its own window.
pub fn sys_close(pid: u64, id: u64) -> i64 {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return -1 };
    let sc = &mut d.sc;
    let Some(i) = sc.find_by_id(id as u32) else { return -1 };
    if sc.wins[i].owner != pid || sc.wins[i].kind != Kind::User {
        return -1;
    }
    let was_open = sc.wins[i].open;
    sc.wins[i].open = false;
    sc.wins[i].buf = Vec::new();
    if was_open {
        sc.dirty_win(i);
        crate::klog!("gui: user window id={} closed by pid {}", id, pid);
    }
    0
}

/// SYS_GUI_RECT(id, x|(y<<16), w|(h<<16), rgb): fill a rect in the
/// window-local coordinate system (content-area origin).
pub fn sys_rect(pid: u64, id: u64, xy: u64, wh: u64, rgb: u32) -> i64 {
    // coords are sign-extended i16 (negative = clipped out by the painter)
    let (x, y) = ((xy & 0xFFFF) as u16 as i16 as i32, ((xy >> 16) & 0xFFFF) as u16 as i16 as i32);
    let (w, h) = ((wh & 0xFFFF) as i32, ((wh >> 16) & 0xFFFF) as i32);
    let col = pack_rgb_col(rgb);
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return -1 };
    let sc = &mut d.sc;
    let Some(i) = sc.find_by_id(id as u32) else { return -1 };
    if sc.wins[i].owner != pid || !sc.wins[i].open {
        return -1;
    }
    let cw = (sc.wins[i].w - 4).max(0) as usize;
    let ch = (sc.wins[i].h - TITLE_H as i32 - 2).max(0) as usize;
    if sc.wins[i].buf.len() != cw * ch {
        return -1; // geometry changed; the app must redraw after RESIZE
    }
    let mut p = Painter::new(&mut sc.wins[i].buf, cw, ch, (0, 0, cw as i32, ch as i32));
    p.fill_rect(x, y, w, h, col);
    push_rect(&mut sc.dirty, sc.wins[i].full_rect());
    0
}

/// SYS_GUI_TEXT(id, x|(y<<16), ptr, len, rgb): draw an ASCII string.
pub fn sys_text(pid: u64, id: u64, xy: u64, ptr: u64, len: u64, rgb: u32) -> i64 {
    let (x, y) = ((xy & 0xFFFF) as u16 as i16 as i32, ((xy >> 16) & 0xFFFF) as u16 as i16 as i32);
    let len = (len as usize).min(200);
    let bytes = match copy_user_bytes(ptr, len) {
        Some(b) => b,
        None => return -1,
    };
    let col = pack_rgb_col(rgb);
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return -1 };
    let sc = &mut d.sc;
    let Some(i) = sc.find_by_id(id as u32) else { return -1 };
    if sc.wins[i].owner != pid || !sc.wins[i].open {
        return -1;
    }
    let cw = (sc.wins[i].w - 4).max(0) as usize;
    let ch = (sc.wins[i].h - TITLE_H as i32 - 2).max(0) as usize;
    if sc.wins[i].buf.len() != cw * ch {
        return -1;
    }
    let mut p = Painter::new(&mut sc.wins[i].buf, cw, ch, (0, 0, cw as i32, ch as i32));
    p.str8(&sanitize(&bytes), x, y, col, None, 1);
    push_rect(&mut sc.dirty, sc.wins[i].full_rect());
    len as i64
}

/// SYS_GUI_EVENT(id): pop one packed input event, 0 when the queue is
/// empty (polling model -- apps sleep between polls).
pub fn sys_event(pid: u64, id: u64) -> u64 {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return (-1i64) as u64 };
    let sc = &mut d.sc;
    let Some(i) = sc.find_by_id(id as u32) else {
        return (-1i64) as u64; // window gone: the app should exit
    };
    if sc.wins[i].owner != pid {
        return (-1i64) as u64;
    }
    sc.wins[i].ev.pop_front().unwrap_or(EV_NONE)
}

/// SYS_GUI_GEO(id): current geometry packed as (w << 16) | h, -1 if gone.
pub fn sys_geo(pid: u64, id: u64) -> i64 {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return -1 };
    let sc = &d.sc;
    let Some(i) = sc.find_by_id(id as u32) else { return -1 };
    if sc.wins[i].owner != pid || !sc.wins[i].open {
        return -1;
    }
    ((sc.wins[i].w as i64) << 16) | sc.wins[i].h as i64
}

/// Scheduler exit hook: a dead task takes its windows with it.
/// Called from sched::exit_current AFTER SCHED_LOCK has been released
/// (lock order: never SCHED_LOCK -> GUI_LOCK).
pub fn on_task_exit(pid: u64) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let mut dirtied = false;
    for i in 0..sc.wins.len() {
        if sc.wins[i].kind == Kind::User && sc.wins[i].open && sc.wins[i].owner == pid {
            crate::klog!("gui: user window id={} died with pid {}", sc.wins[i].id, pid);
            sc.wins[i].open = false;
            sc.wins[i].buf = Vec::new();
            sc.dirty_win(i);
            dirtied = true;
        }
        // v1.4: terminal windows die with their session task too
        if sc.wins[i].kind == Kind::Term && sc.wins[i].open && sc.wins[i].owner == pid {
            crate::klog!(
                "gui: terminal window id={} died with session pid {}",
                sc.wins[i].id,
                pid
            );
            sc.wins[i].open = false;
            sc.wins[i].term.input.clear();
            sc.dirty_win(i);
            dirtied = true;
        }
    }
    if dirtied {
        let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
    }
}

// ---------------- v1.4: terminal window API ----------------
//
// The session task (term.rs) and the console redirect live outside the
// GUI; these entry points are the only way they touch desktop state.
// Every function takes GUI_LOCK and finds the window by id.

/// Open a fresh terminal window owned by kernel session task `pid`.
/// Returns the window id used as the session's output-redirect target.
pub fn term_open_for(pid: u64) -> Option<u32> {
    let _g = GUI_LOCK.lock();
    let d = desk()?;
    let sc = &mut d.sc;

    // reuse a closed terminal slot, else append a new window
    let slot = sc
        .wins
        .iter()
        .position(|w| w.kind == Kind::Term && !w.open);
    let slot = match slot {
        Some(s) => s,
        None => {
            if sc.wins.len() >= MAX_WINS {
                return None;
            }
            sc.wins.push(Win {
                id: 0,
                kind: Kind::Term,
                owner: 0,
                title: String::from("terminal"),
                x: 0,
                y: 0,
                w: TERM_W as i32,
                h: TERM_H as i32,
                open: false,
                minimized: false,
                buf: Vec::new(),
                ev: VecDeque::new(),
                term: TermState::empty(),
            });
            sc.wins.len() - 1
        }
    };

    // cascade placement from the monitor window, else centered
    let (mut x, mut y) = if sc.wins[MONITOR].open {
        (sc.wins[MONITOR].x + 44, sc.wins[MONITOR].y + 40)
    } else {
        (
            (sc.w as i32 - TERM_W as i32) / 2,
            (sc.taskbar_y() - TERM_H as i32) / 2 - 20,
        )
    };
    let n_open = sc
        .wins
        .iter()
        .filter(|w| w.kind == Kind::Term && w.open)
        .count() as i32;
    x += n_open * 26;
    y += n_open * 26;

    sc.next_id += 1;
    let id = sc.next_id;
    let wrect = {
        let w = &mut sc.wins[slot];
        w.id = id;
        w.owner = pid;
        w.x = x;
        w.y = y;
        w.w = TERM_W as i32;
        w.h = TERM_H as i32;
        w.open = true;
        w.minimized = false;
        w.title = String::from("terminal");
        w.term = TermState::empty();
        w.term.cols = ((w.w - 16) / 8).max(12) as usize;
        w.ev.clear();
        w.full_rect()
    };
    sc.z.retain(|&k| k != slot);
    sc.z.push(slot);
    push_rect(&mut sc.dirty, wrect);
    let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
    crate::klog!(
        "gui: terminal window id={} opened for pid {} at ({},{}) {}x{}",
        id,
        pid,
        x,
        y,
        TERM_W,
        TERM_H
    );
    Some(id)
}

/// Common lookup: the open terminal window with this id, mutable.
fn term_slot(sc: &mut Scene, id: u32) -> Option<usize> {
    let i = sc.find_by_id(id)?;
    if sc.wins[i].kind != Kind::Term || !sc.wins[i].open {
        return None;
    }
    Some(i)
}

/// Console redirect sink: append bytes to the terminal's scrollback.
pub fn term_feed(id: u32, s: &str) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    sc.wins[i].term.feed(s);
    sc.dirty_win(i);
}

/// Console redirect color: set the palette index used for new characters.
pub fn term_set_color(id: u32, color: u8) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    sc.wins[i].term.fg = color.min(15);
}

/// `clear` from a terminal session: wipe scrollback + live line.
pub fn term_clear(id: u32) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    sc.wins[i].term.lines.clear();
    sc.wins[i].term.cur.clear();
    sc.dirty_win(i);
}

/// Pop one keystroke routed by the compositor (session poll).
pub fn term_pop_input(id: u32) -> Option<u8> {
    let _g = GUI_LOCK.lock();
    let d = desk()?;
    let sc = &mut d.sc;
    let i = term_slot(sc, id)?;
    sc.wins[i].term.input.pop_front()
}

/// Is this terminal window still open? A closed one tells the session
/// task to exit ([x] clicked, or the desktop session ended).
pub fn term_closed(id: u32) -> bool {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return true };
    let sc = &mut d.sc;
    match sc.find_by_id(id) {
        Some(i) => sc.wins[i].kind != Kind::Term || !sc.wins[i].open,
        None => true,
    }
}

/// Close from the session itself (`exit` command).
pub fn term_close(id: u32) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    crate::klog!("gui: terminal window id={} closed by session", id);
    sc.wins[i].open = false;
    sc.wins[i].term.input.clear();
    sc.dirty_win(i);
    let tr = taskbar_rect(sc);
    push_rect(&mut sc.dirty, tr);
}

/// Erase the last character of the live line (session backspace).
pub fn term_backspace(id: u32) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    sc.wins[i].term.backspace();
    sc.dirty_win(i);
}

/// Toggle the caret blink (session tick) and repaint the window.
pub fn term_caret_tick(id: u32) {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    let sc = &mut d.sc;
    let Some(i) = term_slot(sc, id) else { return };
    sc.wins[i].term.caret = !sc.wins[i].term.caret;
    sc.dirty_win(i);
}

/// Open/restore/focus the system monitor from a terminal session
/// (the `gui` command must NOT re-enter the compositor recursively).
pub fn desktop_open_monitor() {
    let _g = GUI_LOCK.lock();
    let Some(d) = desk() else { return };
    open_win(&mut d.sc, MONITOR);
}

// ---------------- desktop session (the compositor loop) ----------------

/// Take the screen over, run the desktop until Esc / all windows closed /
/// reboot / halt, then hand the text console back. Runs in the context of
/// the shell task; the scheduler keeps everyone else alive through our
/// SYS_SLEEP frames.
pub fn run() {
    console::GUI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
    keyboard::drain();

    // pack the palette + build the gradient against the live console, and
    // reserve the back buffer (one full frame) up front
    let (w, h, c, grad, mut back) = {
        let mut g = CONSOLE.lock();
        let Some(con) = g.as_ref() else {
            console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
            crate::klog!("gui: no framebuffer console, abort");
            return;
        };
        let (w, h) = (con.width(), con.height());
        let c = C::build(con);
        let mut grad: Vec<u32> = Vec::new();
        let den = (h.max(2) - 1) as u32;
        for y in 0..h {
            grad.push(con.pack_rgb(&lerp_rgb(DESK_TOP, DESK_BOT, y as u32, den)));
        }
        let mut back: Vec<u32> = Vec::new();
        let ok = back.try_reserve_exact(w * h).is_ok();
        if ok {
            back.resize(w * h, 0);
        }
        (w, h, c, grad, if ok { back } else { Vec::new() })
    };
    if back.is_empty() {
        console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
        crate::klog!("gui: back buffer alloc failed ({}x{}), abort", w, h);
        return;
    }
    crate::klog!(
        "gui: enter (double buffered, {}x{}, back buffer {} KiB)",
        w,
        h,
        back.len() * 4 / 1024
    );

    let sc = Scene {
        w,
        h,
        c,
        grad,
        wins: alloc::vec![
            Win {
                id: 0,
                kind: Kind::Monitor,
                owner: 0,
                title: String::from("GLM OS 1.6 - system monitor"),
                x: ((w - MON_W) / 2) as i32,
                y: (((h - TASKBAR_H - MON_H) / 2).saturating_sub(24)) as i32,
                w: MON_W as i32,
                h: MON_H as i32,
                open: true,
                minimized: false,
                buf: Vec::new(),
                ev: VecDeque::new(),
                term: TermState::empty(),
            },
            Win {
                id: 0,
                kind: Kind::About,
                owner: 0,
                title: String::from("GLM OS 1.6 - about"),
                x: 0,
                y: 0,
                w: ABOUT_W as i32,
                h: ABOUT_H as i32,
                open: false,
                minimized: false,
                buf: Vec::new(),
                ev: VecDeque::new(),
                term: TermState::empty(),
            },
        ],
        z: alloc::vec![MONITOR, ABOUT],
        dirty: Vec::new(),
        cur_x: (w / 2) as i32,
        cur_y: (h / 2) as i32,
        prev_left: false,
        dragging: None,
        resizing: None,
        menu_open: false,
        menu_hover: None,
        next_id: 0,
        moves: 0,
        clicks: 0,
        drags: 0,
        keys: 0,
        stats_at: 0,
        clock_at: 0,
        frames: 0,
        fps: 0,
        fps_at: 0,
        net_prev: 0,
        net_led: false,
        live: true,
    };
    let mut d = Desk { sc, back };
    d.sc.clamp_win(MONITOR);
    compose(&mut d, (0, 0, w as i32, h as i32));
    {
        let _g = GUI_LOCK.lock();
        unsafe { core::ptr::addr_of_mut!(DESK).write(Some(d)) };
    }
    // initial blit + cursor
    {
        let _g = GUI_LOCK.lock();
        let d = desk().unwrap();
        let mut g = CONSOLE.lock();
        if let Some(con) = g.as_ref() {
            con.blit_from(&d.back, w, 0, 0, 0, 0, w, h);
            stamp_cursor(con, d.sc.cur_x, d.sc.cur_y);
        }
    }

    // ---- the compositor loop: one iteration = one frame ----
    let mut exit: Option<Reason> = None;
    while exit.is_none() {
        exit = {
            let _g = GUI_LOCK.lock();
            let d = desk().unwrap();

            // ---- phase A: input + state (mutable scene borrow) ----
            let (click_reason, quit, moved, ocx, ocy) = {
                let sc = &mut d.sc;
                let (dx, dy, btns) = mouse::take();
                let left = btns & 1 != 0;
                let (ocx, ocy) = (sc.cur_x, sc.cur_y);
                let moved = dx != 0 || dy != 0;
                if moved {
                    sc.moves += 1;
                    sc.cur_x += dx;
                    sc.cur_y += dy;
                    sc.clamp_cur();
                    if let Some((wi, gdx, gdy)) = sc.dragging {
                        let old = sc.wins[wi].full_rect();
                        sc.wins[wi].x = sc.cur_x - gdx;
                        sc.wins[wi].y = sc.cur_y - gdy;
                        sc.clamp_win(wi);
                        sc.drags += 1;
                        push_rect(&mut sc.dirty, old);
                        push_rect(&mut sc.dirty, sc.wins[wi].full_rect());
                    }
                    if let Some(ri) = sc.resizing {
                        let old = sc.wins[ri].full_rect();
                        let sw = sc.w as i32;
                        let tby = sc.taskbar_y();
                        let win = &mut sc.wins[ri];
                        let nw = (sc.cur_x - win.x + 1).clamp(MIN_WIN_W, sw - win.x - 2);
                        let nh = (sc.cur_y - win.y + 1).clamp(MIN_WIN_H, tby - win.y - 2);
                        if nw != win.w || nh != win.h {
                            win.w = nw;
                            win.h = nh;
                            if win.kind == Kind::User {
                                // backing store follows the new content size;
                                // the app repaints on the RESIZE event
                                let cw = (nw - 4).max(1) as usize;
                                let ch = (nh - TITLE_H as i32 - 2).max(1) as usize;
                                win.buf.clear();
                                win.buf.resize(cw * ch, 0);
                            }
                        }
                        push_rect(&mut sc.dirty, old);
                        push_rect(&mut sc.dirty, sc.wins[ri].full_rect());
                    }
                }
                let click_reason = if left && !sc.prev_left {
                    sc.clicks += 1;
                    on_click(sc)
                } else {
                    None
                };
                if !left && sc.prev_left {
                    // button release ends drag/resize (klog once, per gesture)
                    if let Some(ri) = sc.resizing.take() {
                        let (w0, h0) = (sc.wins[ri].w, sc.wins[ri].h);
                        crate::klog!(
                            "gui: resized {} window to {}x{}",
                            sc.wins[ri].short(),
                            w0,
                            h0
                        );
                        if sc.wins[ri].kind == Kind::User {
                            push_event(sc, ri, pack_ev(EV_RESIZE, w0, h0));
                        }
                    }
                    sc.dragging = None;
                }
                sc.prev_left = left;

                // ---- keyboard: esc closes the menu first, then exits;
                //      other keys go to the focused user or terminal window ----
                let mut quit = false;
                while let Some(k) = keyboard::pop() {
                    if k == 0x1B {
                        if sc.menu_open {
                            sc.menu_open = false;
                            sc.menu_hover = None;
                            let mr = menu_full_rect(sc);
    push_rect(&mut sc.dirty, mr);
                        } else {
                            quit = true;
                            break;
                        }
                    } else if let Some(ai) = sc.active_idx() {
                        if sc.wins[ai].open {
                            match sc.wins[ai].kind {
                                Kind::User => {
                                    sc.keys += 1;
                                    push_event(sc, ai, pack_ev(EV_KEY, k as i32, 0));
                                }
                                Kind::Term => {
                                    // v1.4: keystrokes for the terminal session
                                    const TERM_IN_CAP: usize = 128;
                                    let inp = &mut sc.wins[ai].term.input;
                                    if inp.len() < TERM_IN_CAP {
                                        inp.push_back(k);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // ---- menu hover tracking ----
                if sc.menu_open {
                    let hov = menu_item_at(sc);
                    if hov != sc.menu_hover {
                        sc.menu_hover = hov;
                        let mr = menu_full_rect(sc);
    push_rect(&mut sc.dirty, mr);
                    }
                }

                // ---- periodic ticks ----
                let now = pit::uptime_ms();
                if now.saturating_sub(sc.stats_at) >= 500 {
                    sc.stats_at = now;
                    sc.live = !sc.live;
                    if sc.wins[MONITOR].open && !sc.wins[MONITOR].minimized {
                        push_rect(&mut sc.dirty, sc.wins[MONITOR].content_rect());
                    }
                }
                if now.saturating_sub(sc.clock_at) >= 1000 {
                    sc.clock_at = now;
                    let (_irq, rx, tx, _drop, _k) = crate::net::e1000::counters();
                    sc.net_led = rx + tx != sc.net_prev;
                    sc.net_prev = rx + tx;
                    let trr = tray_rect(sc);
                    push_rect(&mut sc.dirty, trr);
                }
                if now.saturating_sub(sc.fps_at) >= 1000 {
                    sc.fps = (sc.frames as u64 * 1000 / (now - sc.fps_at).max(1)) as u32;
                    sc.frames = 0;
                    sc.fps_at = now;
                }
                (click_reason, quit, moved, ocx, ocy)
            };

            // ---- phase B: render (compose + blit + cursor stamp) ----
            let cur_rect = (d.sc.cur_x, d.sc.cur_y, SAVE_W as i32, SAVE_H as i32);
            let cursor_redraw =
                moved || d.sc.dirty.iter().any(|r| rects_intersect(*r, cur_rect));
            if !d.sc.dirty.is_empty() {
                let rects: Vec<Rect> = d.sc.dirty.clone();
                for r in &rects {
                    compose(d, *r);
                }
            }
            {
                let mut g = CONSOLE.lock();
                if let Some(con) = g.as_ref() {
                    if !d.sc.dirty.is_empty() {
                        for r in &d.sc.dirty {
                            let (rx, ry, rw, rh) = intersect_screen(*r, w, h);
                            if rw > 0 && rh > 0 {
                                con.blit_from(
                                    &d.back,
                                    w,
                                    rx as usize,
                                    ry as usize,
                                    rx as usize,
                                    ry as usize,
                                    rw as usize,
                                    rh as usize,
                                );
                            }
                        }
                    }
                    if moved {
                        // erase the old sprite by blitting the clean scene
                        let (ox, oy) = (ocx.max(0) as usize, ocy.max(0) as usize);
                        con.blit_from(&d.back, w, ox, oy, ox, oy, SAVE_W, SAVE_H);
                    }
                    if cursor_redraw {
                        stamp_cursor(con, d.sc.cur_x, d.sc.cur_y);
                    }
                }
            }
            d.sc.dirty.clear();
            d.sc.frames += 1;
            click_reason.or(quit.then_some(Reason::Esc))
        };
        // one frame: sleep = yield the cpu, irqs keep accumulating motion;
        // user GUI syscalls run while we are parked
        sched::ksyscall(sched::SYS_SLEEP, 16, 0, 0);
    }

    match exit.unwrap_or(Reason::Esc) {
        Reason::Esc | Reason::AllClosed => {
            let reason = exit.unwrap();
            // grab the session stats before the desktop disappears
            let (cx, cy, moves, clicks, drags, keys, frames, fps) = {
                let _g = GUI_LOCK.lock();
                let d = desk().unwrap();
                (
                    d.sc.cur_x, d.sc.cur_y, d.sc.moves, d.sc.clicks, d.sc.drags,
                    d.sc.keys, d.sc.frames, d.sc.fps,
                )
            };
            teardown(reason);
            crate::klog!(
                "gui: cursor=({},{}) moves={} clicks={} drags={} keys={} packets={} frames={} fps={}",
                cx, cy, moves, clicks, drags, keys, mouse::packets(), frames, fps
            );
        }
        Reason::Reboot => {
            teardown(Reason::Reboot);
            crate::klog!("gui: reboot requested from the start menu");
            crate::cpu::reboot();
        }
        Reason::Halt => {
            // freeze on the farewell screen; GUI_ACTIVE stays up so kstat
            // cannot scribble over it. Never returns.
            {
                let _g = GUI_LOCK.lock();
                let d = desk().unwrap();
                draw_halt_screen(d);
                let mut g = CONSOLE.lock();
                if let Some(con) = g.as_ref() {
                    con.blit_from(&d.back, w, 0, 0, 0, 0, w, h);
                }
            }
            crate::klog!("gui: exit reason=halt - screen frozen");
            crate::klog!("gui: halted from the start menu - scheduler still alive");
            loop {
                sched::ksyscall(sched::SYS_SLEEP, 100, 0, 0);
            }
        }
    }
}

fn teardown(reason: Reason) {
    // remove the desktop from under the syscalls, hand the screen back
    let _g = GUI_LOCK.lock();
    unsafe {
        let p = core::ptr::addr_of_mut!(DESK);
        *p = None;
    }
    drop(_g);
    console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
    console::redraw_all_global();
    crate::klog!("gui: exit reason={}", reason.as_str());
}


