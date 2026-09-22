//! GLM OS v1.1 — the framebuffer GUI, now double buffered.
//!
//! Architecture (v1.1):
//!   * everything renders into an off-screen back buffer (one heap-allocated
//!     frame in the framebuffer's native packed-pixel format);
//!   * only dirty rectangles are blitted to the visible framebuffer
//!     (`Console::blit_from`), so mid-frame tearing is gone;
//!   * the mouse cursor is stamped on top of the front buffer AFTER each
//!     blit -- the scene buffer never contains the cursor, so the pointer
//!     can neither flicker nor smear and needs no save/restore arrays;
//!   * the bottom taskbar behaves like a real desktop's: a start button
//!     with a popup menu (launch windows / reboot / halt), task buttons
//!     for every open window (click = focus or minimize toggle) and a
//!     tray with a net-activity led plus an uptime clock;
//!   * two window kinds ship: the system monitor and an about card; both
//!     drag by the title bar, minimize to the taskbar and close; windows
//!     have focus + z-order (the focused one draws last, on top);
//!   * `Esc` or closing every window hands the text screen back through
//!     `console::redraw_all_global()`; the start menu can also reboot or
//!     halt the machine straight from the desktop.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::console::{self, font, Rgb, CONSOLE};
use crate::cpu::keyboard;
use crate::cpu::mouse;
use crate::cpu::pit;
use crate::sched;

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
        }
    }
}

// ---------------- geometry ----------------

const MON_W: usize = 380;
const MON_H: usize = 244;
const ABOUT_W: usize = 330;
const ABOUT_H: usize = 190;
const TITLE_H: usize = 22;
const TASKBAR_H: usize = 28;
const START_W: usize = 56;
const TASKBTN_W: usize = 132;
const LINE_H: usize = 14; // scale-1 text line pitch
const MARGIN: usize = 12;

const MENU_W: usize = 176;
const MENU_ITEM_H: usize = 20;
/// 4px top pad + 2 items + 6px separator + 2 items + 2px bottom pad
const MENU_H: usize = 4 + 2 * MENU_ITEM_H + 6 + 2 * MENU_ITEM_H + 2;
const MENU_ITEMS: [&str; 4] = ["system monitor", "about glm os", "reboot", "halt"];

const SAVE_W: usize = 16;
const SAVE_H: usize = 24;

const MONITOR: usize = 0;
const ABOUT: usize = 1;

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

// ---------------- windows & ui state ----------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum WinKind {
    Monitor,
    About,
}

struct Win {
    kind: WinKind,
    x: i32,
    y: i32,
    open: bool,
    minimized: bool,
}

impl Win {
    fn w(&self) -> i32 {
        match self.kind {
            WinKind::Monitor => MON_W as i32,
            WinKind::About => ABOUT_W as i32,
        }
    }

    fn h(&self) -> i32 {
        match self.kind {
            WinKind::Monitor => MON_H as i32,
            WinKind::About => ABOUT_H as i32,
        }
    }

    fn title(&self) -> &'static str {
        match self.kind {
            WinKind::Monitor => "GLM OS 1.1 - system monitor",
            WinKind::About => "GLM OS 1.1 - about",
        }
    }

    fn short(&self) -> &'static str {
        match self.kind {
            WinKind::Monitor => "system monitor",
            WinKind::About => "about",
        }
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

struct Ui {
    w: usize,
    h: usize,
    cur_x: i32,
    cur_y: i32,
    wins: [Win; 2],
    z: [usize; 2], // z[0] = bottom, z[1] = top
    dragging: Option<(usize, i32, i32)>, // (win idx, grab dx, grab dy)
    prev_left: bool,
    menu_open: bool,
    menu_hover: Option<usize>,
    moves: u64,
    clicks: u64,
    drags: u64,
    stats_at: u64,
    clock_at: u64,
    frames: u32,
    fps: u32,
    fps_at: u64,
    net_prev: u64,
    net_led: bool,
    live: bool,
}

impl Ui {
    fn taskbar_y(&self) -> i32 {
        self.h as i32 - TASKBAR_H as i32
    }

    fn clamp_cur(&mut self) {
        self.cur_x = self.cur_x.clamp(0, self.w as i32 - 2);
        self.cur_y = self.cur_y.clamp(0, self.h as i32 - 2);
    }

    fn clamp_win(&mut self, i: usize) {
        let (ww, wh) = (self.wins[i].w(), self.wins[i].h());
        let xmax = (self.w as i32 - ww - 2).max(0);
        let ymax = (self.taskbar_y() - wh - 2).max(0);
        self.wins[i].x = self.wins[i].x.clamp(0, xmax);
        self.wins[i].y = self.wins[i].y.clamp(0, ymax);
    }

    /// The focused window = the topmost open one in z-order.
    fn active_idx(&self) -> Option<usize> {
        for k in (0..2).rev() {
            let i = self.z[k];
            if self.wins[i].open {
                return Some(i);
            }
        }
        None
    }

    /// The other open window, if any (fixed 2-window model).
    fn other_open(&self, i: usize) -> Option<usize> {
        let j = 1 - i;
        if self.wins[j].open {
            Some(j)
        } else {
            None
        }
    }
}

// ---------------- rect helpers over the ui ----------------

fn win_body_rect(ui: &Ui, i: usize) -> Rect {
    (ui.wins[i].x, ui.wins[i].y, ui.wins[i].w(), ui.wins[i].h())
}

/// Window footprint including the 4px drop shadow (right + down).
fn win_full_rect(ui: &Ui, i: usize) -> Rect {
    (
        ui.wins[i].x,
        ui.wins[i].y,
        ui.wins[i].w() + 4,
        ui.wins[i].h() + 4,
    )
}

fn win_content_rect(ui: &Ui, i: usize) -> Rect {
    (
        ui.wins[i].x + 2,
        ui.wins[i].y + TITLE_H as i32 + 1,
        ui.wins[i].w() - 4,
        ui.wins[i].h() - TITLE_H as i32 - 2,
    )
}

fn start_rect(ui: &Ui) -> Rect {
    (6, ui.taskbar_y() + 4, START_W as i32, 20)
}

fn taskbar_rect(ui: &Ui) -> Rect {
    (0, ui.taskbar_y(), ui.w as i32, TASKBAR_H as i32)
}

fn tray_rect(ui: &Ui) -> Rect {
    (ui.w as i32 - 220, ui.taskbar_y(), 220, TASKBAR_H as i32)
}

fn menu_panel_rect(ui: &Ui) -> Rect {
    (
        6,
        ui.taskbar_y() - MENU_H as i32 - 4,
        MENU_W as i32,
        MENU_H as i32,
    )
}

/// Menu footprint incl. the 1px border and 3px drop shadow.
fn menu_full_rect(ui: &Ui) -> Rect {
    let (x, y, _, _) = menu_panel_rect(ui);
    (x, y, MENU_W as i32 + 4, MENU_H as i32 + 4)
}

/// Top y of menu item `k` (0 and 1 above the separator, 2 and 3 below).
fn menu_item_y(ui: &Ui, k: usize) -> i32 {
    let my = menu_panel_rect(ui).1;
    if k < 2 {
        my + 4 + (k as i32) * MENU_ITEM_H as i32
    } else {
        my + 4 + 2 * MENU_ITEM_H as i32 + 6 + ((k - 2) as i32) * MENU_ITEM_H as i32
    }
}

fn menu_item_rect(ui: &Ui, k: usize) -> Rect {
    (
        6 + 2,
        menu_item_y(ui, k),
        MENU_W as i32 - 4,
        MENU_ITEM_H as i32,
    )
}

fn menu_item_at(ui: &Ui) -> Option<usize> {
    for k in 0..4 {
        if point_in(ui.cur_x, ui.cur_y, menu_item_rect(ui, k)) {
            return Some(k);
        }
    }
    None
}

/// The nth taskbar task-button rect for open window `i`.
fn task_btn_rect(ui: &Ui, i: usize) -> Rect {
    let mut bx = 6 + START_W as i32 + 6;
    for j in 0..i {
        if ui.wins[j].open {
            bx += (TASKBTN_W + 4) as i32;
        }
    }
    (bx, ui.taskbar_y() + 4, TASKBTN_W as i32, 20)
}

// ---------------- actions ----------------

/// Raise window `i` to the top of the z-order, un-minimize it and mark
/// every affected region (both title bars + taskbar) dirty.
fn focus(ui: &mut Ui, dirty: &mut Vec<Rect>, i: usize) {
    if ui.z[1] == i && !ui.wins[i].minimized {
        return;
    }
    let other = if ui.z[0] == i { ui.z[1] } else { ui.z[0] };
    ui.z = [other, i];
    ui.wins[i].minimized = false;
    for j in 0..2 {
        if ui.wins[j].open && !ui.wins[j].minimized {
            push_rect(dirty, win_full_rect(ui, j));
        }
    }
    push_rect(dirty, taskbar_rect(ui));
}

fn open_win(ui: &mut Ui, dirty: &mut Vec<Rect>, kind_idx: usize) {
    if ui.wins[kind_idx].open {
        // already open: just restore + focus
        ui.wins[kind_idx].minimized = false;
        crate::klog!("gui: restore {} window", ui.wins[kind_idx].short());
        focus(ui, dirty, kind_idx);
        return;
    }
    ui.wins[kind_idx].open = true;
    // cascade from the monitor window when it is around, else center
    if kind_idx == ABOUT {
        let (mx, my) = (ui.wins[MONITOR].x, ui.wins[MONITOR].y);
        if ui.wins[MONITOR].open {
            ui.wins[ABOUT].x = mx + 36;
            ui.wins[ABOUT].y = my + 36;
        } else {
            ui.wins[ABOUT].x = (ui.w as i32 - ABOUT_W as i32) / 2;
            ui.wins[ABOUT].y = (ui.taskbar_y() - ABOUT_H as i32) / 2 - 24;
        }
    }
    ui.clamp_win(kind_idx);
    crate::klog!("gui: launch {} window", ui.wins[kind_idx].short());
    focus(ui, dirty, kind_idx);
}

fn minimize_win(ui: &mut Ui, dirty: &mut Vec<Rect>, i: usize) {
    ui.wins[i].minimized = true;
    crate::klog!("gui: minimize {} window", ui.wins[i].short());
    push_rect(dirty, win_full_rect(ui, i));
    if let Some(o) = ui.other_open(i) {
        focus(ui, dirty, o);
    } else {
        push_rect(dirty, taskbar_rect(ui));
    }
}

fn task_btn_click(ui: &mut Ui, dirty: &mut Vec<Rect>, i: usize) {
    let active = ui.active_idx() == Some(i);
    if active && !ui.wins[i].minimized {
        minimize_win(ui, dirty, i);
    } else {
        crate::klog!("gui: restore {} window", ui.wins[i].short());
        focus(ui, dirty, i);
    }
}

/// Left-button press dispatch. Returns a Reason when the desktop session
/// itself should end (all windows closed / reboot / halt).
fn on_click(ui: &mut Ui, dirty: &mut Vec<Rect>) -> Option<Reason> {
    let (x, y) = (ui.cur_x, ui.cur_y);

    // 1) start menu is up: items act, clicks outside dismiss it
    if ui.menu_open {
        if let Some(k) = menu_item_at(ui) {
            ui.menu_open = false;
            ui.menu_hover = None;
            push_rect(dirty, menu_full_rect(ui));
            return match k {
                0 => {
                    open_win(ui, dirty, MONITOR);
                    None
                }
                1 => {
                    open_win(ui, dirty, ABOUT);
                    None
                }
                2 => Some(Reason::Reboot),
                _ => Some(Reason::Halt),
            };
        }
        if !point_in(x, y, menu_full_rect(ui)) {
            ui.menu_open = false;
            ui.menu_hover = None;
            push_rect(dirty, menu_full_rect(ui));
        }
        return None;
    }

    // 2) taskbar: start button, then task buttons
    if y >= ui.taskbar_y() {
        if point_in(x, y, start_rect(ui)) {
            ui.menu_open = true;
            ui.menu_hover = None;
            crate::klog!("gui: start menu open");
            push_rect(dirty, taskbar_rect(ui));
            push_rect(dirty, menu_full_rect(ui));
            return None;
        }
        for i in 0..2 {
            if ui.wins[i].open && point_in(x, y, task_btn_rect(ui, i)) {
                task_btn_click(ui, dirty, i);
                return None;
            }
        }
        return None;
    }

    // 3) windows, topmost first: first click focuses, then buttons/drag act
    for k in (0..2).rev() {
        let i = ui.z[k];
        if !ui.wins[i].open || ui.wins[i].minimized {
            continue;
        }
        if !point_in(x, y, win_body_rect(ui, i)) {
            continue;
        }
        if ui.active_idx() != Some(i) {
            focus(ui, dirty, i);
            return None;
        }
        let (wx, wy, ww) = (ui.wins[i].x, ui.wins[i].y, ui.wins[i].w());
        // close box [x]
        if point_in(x, y, (wx + ww - 24, wy + 4, 18, 14)) {
            ui.wins[i].open = false;
            crate::klog!("gui: close {} window", ui.wins[i].short());
            push_rect(dirty, win_full_rect(ui, i));
            if let Some(o) = ui.other_open(i) {
                focus(ui, dirty, o);
            } else {
                push_rect(dirty, taskbar_rect(ui));
            }
            if ui.wins.iter().all(|w| !w.open) {
                return Some(Reason::AllClosed);
            }
            return None;
        }
        // minimize box [-]
        if point_in(x, y, (wx + ww - 42, wy + 4, 18, 14)) {
            minimize_win(ui, dirty, i);
            return None;
        }
        // title bar (minus the button strip) starts a drag
        if y < wy + TITLE_H as i32 && x < wx + ww - 42 {
            ui.dragging = Some((i, x - wx, y - wy));
            return None;
        }
        return None;
    }
    None
}

// ---------------- scene composition (into the back buffer) ----------------

/// Redraw `rect` of the scene into the back buffer: desktop gradient,
/// watermark, every visible window in z-order, then the start menu on top.
/// Called once for the full screen at entry, then per dirty rectangle.
fn compose(back: &mut [u32], grad: &[u32], w: usize, h: usize, c: &C, ui: &Ui, r: Rect) {
    let (rx, ry, rw, rh) = intersect_screen(r, w, h);
    if rw <= 0 || rh <= 0 {
        return;
    }
    let mut p = Painter::new(back, w, h, (rx, ry, rw, rh));
    for y in ry..ry + rh {
        if y >= 0 && (y as usize) < grad.len() {
            let col = grad[y as usize];
            p.fill_row(y, rx, rx + rw, col);
        }
    }
    draw_watermark(&mut p, c, w, h);
    let (z0, z1) = (ui.z[0], ui.z[1]);
    for i in [z0, z1] {
        if ui.wins[i].open && !ui.wins[i].minimized {
            draw_window(&mut p, c, ui, i);
        }
    }
    // the taskbar always sits on top of the desktop pattern; the start
    // menu (if open) is the topmost layer of all
    draw_taskbar(&mut p, c, ui);
    if ui.menu_open {
        draw_menu(&mut p, c, ui);
    }
}

fn draw_watermark(p: &mut Painter, c: &C, w: usize, h: usize) {
    let text = "GLM OS";
    let scale = 4;
    let tw = (text.len() * 8 * scale) as i32;
    let x = (w as i32 - tw) / 2;
    let y = h as i32 / 6; // above the default window position
    p.str8(text, x, y, c.mark, None, scale);
    let sub = "v1.1 - double buffered desktop";
    let sw = (sub.len() * 8) as i32;
    p.str8(sub, (w as i32 - sw) / 2, y + 8 * scale as i32 + 14, c.mark, None, 1);
}

fn draw_window(p: &mut Painter, c: &C, ui: &Ui, i: usize) {
    let (wx, wy) = (ui.wins[i].x, ui.wins[i].y);
    let (ww, wh) = (ui.wins[i].w(), ui.wins[i].h());
    let active = ui.active_idx() == Some(i);

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
    p.str8(ui.wins[i].title(), wx + 8, wy + 7, c.title_fg, Some(tb), 1);

    // minimize [-] and close [x] boxes
    let (mx, my) = (wx + ww - 42, wy + 4);
    p.fill_rect(mx, my, 18, 14, c.btn);
    p.str8("-", mx + 7, my + 3, c.btn_fg, Some(c.btn), 1);
    let (bx, by) = (wx + ww - 24, wy + 4);
    p.fill_rect(bx, by, 18, 14, c.close_bg);
    p.str8("x", bx + 6, by + 3, c.title_fg, Some(c.close_bg), 1);

    match ui.wins[i].kind {
        WinKind::Monitor => draw_monitor_content(p, c, ui),
        WinKind::About => draw_about_content(p, c, ui),
    }
}

fn draw_monitor_content(p: &mut Painter, c: &C, ui: &Ui) {
    let i = MONITOR;
    let (wx, wy) = (ui.wins[i].x, ui.wins[i].y);
    let (ww, wh) = (ui.wins[i].w(), ui.wins[i].h());

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
        format!("gui:     double buffered, {} fps", ui.fps),
        format!("cursor:  {}, {}", ui.cur_x, ui.cur_y),
    ];

    for (n, line) in lines.iter().enumerate() {
        let fg = if n == 6 { c.accent } else { c.text };
        p.str8(line, wx + MARGIN as i32, wy + TITLE_H as i32 + 8 + (n * LINE_H) as i32, fg, None, 1);
    }

    // live dot blinks on the 500 ms stats tick
    let dot = if ui.live { c.accent } else { c.win_bg };
    p.fill_rect(wx + ww - 14, wy + TITLE_H as i32 + 10, 6, 6, dot);

    p.str8(
        "esc exits - drag title, '-' minimizes",
        wx + MARGIN as i32,
        wy + wh - 18,
        c.warn,
        None,
        1,
    );
}

fn draw_about_content(p: &mut Painter, c: &C, ui: &Ui) {
    let i = ABOUT;
    let (wx, wy) = (ui.wins[i].x, ui.wins[i].y);

    p.fill_rect(
        wx + 2,
        wy + TITLE_H as i32 + 1,
        ABOUT_W as i32 - 4,
        ABOUT_H as i32 - TITLE_H as i32 - 2,
        c.win_bg,
    );

    // logo lockup: "GLM" accent + "OS" white, scale 2 (inside the content area)
    p.str8("GLM", wx + 16, wy + 32, c.accent, None, 2);
    p.str8("OS", wx + 16 + 3 * 16 + 8, wy + 32, c.title_fg, None, 2);
    p.str8(
        "version 1.1.0 - double buffered",
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
        ("udp+icmp networking | fat32", c.text),
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

fn draw_taskbar(p: &mut Painter, c: &C, ui: &Ui) {
    let w = ui.w;
    let ty = ui.taskbar_y();

    p.fill_rect(0, ty, w as i32, TASKBAR_H as i32, c.taskbar);
    p.fill_rect(0, ty, w as i32, 1, c.taskbar_edge);

    // start button (pressed look while the menu is open)
    let sbg = if ui.menu_open { c.hover } else { c.btn };
    let (sx, sy) = (6, ty + 4);
    p.fill_rect(sx, sy, START_W as i32, 20, sbg);
    p.fill_rect(sx, sy, START_W as i32, 1, c.btn_edge);
    p.fill_rect(sx, sy + 19, START_W as i32, 1, c.btn_edge);
    p.fill_rect(sx, sy, 1, 20, c.btn_edge);
    p.fill_rect(sx + START_W as i32 - 1, sy, 1, 20, c.btn_edge);
    p.fill_rect(sx + 6, sy + 6, 8, 8, c.accent); // logo square
    p.str8("GLM", sx + 20, sy + 6, c.title_fg, Some(sbg), 1);

    // task buttons, stable order (win index), open windows only
    for i in 0..2 {
        if !ui.wins[i].open {
            continue;
        }
        let (bx, by, bw, bh) = task_btn_rect(ui, i);
        let active = ui.active_idx() == Some(i) && !ui.wins[i].minimized;
        let bg = if active { c.tb_active } else { c.tb_idle };
        p.fill_rect(bx, by, bw, bh, bg);
        if active {
            p.fill_rect(bx, by, bw, 2, c.accent);
        }
        p.fill_rect(bx, by, 1, bh, c.taskbar_edge);
        p.fill_rect(bx + bw - 1, by, 1, bh, c.taskbar_edge);
        p.fill_rect(bx, by + bh - 1, bw, 1, c.taskbar_edge);
        let fg = if ui.wins[i].minimized {
            c.dim
        } else if active {
            c.title_fg
        } else {
            c.text
        };
        p.str8(ui.wins[i].short(), bx + 8, by + 6, fg, Some(bg), 1);
    }

    // tray: net-activity led + uptime clock + version tag
    let ms = pit::uptime_ms();
    let tray = format!(
        "{:02}:{:02}:{:02}  GLM 1.1",
        (ms / 3_600_000) % 100,
        (ms / 60_000) % 60,
        (ms / 1000) % 60
    );
    let tx = w as i32 - (tray.len() as i32) * 8 - 12;
    let tyi = ty as i32;
    p.fill_rect(tx - 14, tyi + 12, 4, 4, if ui.net_led { c.accent } else { c.btn_edge });
    p.str8(&tray, tx, tyi + 10, c.text, None, 1);
}

fn draw_menu(p: &mut Painter, c: &C, ui: &Ui) {
    let (mx, my, mw, mh) = menu_panel_rect(ui);

    // drop shadow, panel, border
    p.fill_rect(mx + 3, my + 3, mw, mh, c.shadow);
    p.fill_rect(mx, my, mw, mh, c.menu_bg);
    p.fill_rect(mx, my, mw, 1, c.menu_edge);
    p.fill_rect(mx, my + mh - 1, mw, 1, c.menu_edge);
    p.fill_rect(mx, my, 1, mh, c.menu_edge);
    p.fill_rect(mx + mw - 1, my, 1, mh, c.menu_edge);

    let icons = [c.accent, c.cyan, c.warn, c.red];
    for k in 0..4 {
        let iy = menu_item_y(ui, k);
        if k == 2 {
            // separator line between the launch pair and the power pair
            p.fill_row(iy - 3, mx + 4, mx + mw - 4, c.menu_edge);
        }
        let hovered = ui.menu_hover == Some(k);
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
fn draw_halt_screen(back: &mut [u32], grad: &[u32], w: usize, h: usize, c: &C) {
    let mut p = Painter::new(back, w, h, (0, 0, w as i32, h as i32));
    for y in 0..h as i32 {
        if y >= 0 && (y as usize) < grad.len() {
            let col = grad[y as usize];
            p.fill_row(y, 0, w as i32, col);
        }
    }
    let t1 = "GLM OS 1.1";
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

// ---------------- entry ----------------

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

    let mut ui = Ui {
        w,
        h,
        cur_x: (w / 2) as i32,
        cur_y: (h / 2) as i32,
        wins: [
            Win {
                kind: WinKind::Monitor,
                x: ((w - MON_W) / 2) as i32,
                y: (((h - TASKBAR_H - MON_H) / 2).saturating_sub(24)) as i32,
                open: true,
                minimized: false,
            },
            Win {
                kind: WinKind::About,
                x: 0,
                y: 0,
                open: false,
                minimized: false,
            },
        ],
        z: [MONITOR, ABOUT],
        dragging: None,
        prev_left: false,
        menu_open: false,
        menu_hover: None,
        moves: 0,
        clicks: 0,
        drags: 0,
        stats_at: 0,
        clock_at: 0,
        frames: 0,
        fps: 0,
        fps_at: 0,
        net_prev: 0,
        net_led: false,
        live: true,
    };
    ui.clamp_win(MONITOR);

    let mut dirty: Vec<Rect> = Vec::new();

    // --- initial scene: full compose, full blit, cursor stamp ---
    {
        compose(&mut back, &grad, w, h, &c, &ui, (0, 0, w as i32, h as i32));
        let mut g = CONSOLE.lock();
        if let Some(con) = g.as_ref() {
            con.blit_from(&back, w, 0, 0, 0, 0, w, h);
            stamp_cursor(con, ui.cur_x, ui.cur_y);
        }
    }

    let mut exit: Option<Reason> = None;
    while exit.is_none() {
        // ---- input ----
        let (dx, dy, btns) = mouse::take();
        let left = btns & 1 != 0;
        let (ocx, ocy) = (ui.cur_x, ui.cur_y);
        let moved = dx != 0 || dy != 0;
        if moved {
            ui.moves += 1;
            ui.cur_x += dx;
            ui.cur_y += dy;
            ui.clamp_cur();
            if let Some((wi, gdx, gdy)) = ui.dragging {
                let old = win_full_rect(&ui, wi);
                ui.wins[wi].x = ui.cur_x - gdx;
                ui.wins[wi].y = ui.cur_y - gdy;
                ui.clamp_win(wi);
                ui.drags += 1;
                push_rect(&mut dirty, old);
                push_rect(&mut dirty, win_full_rect(&ui, wi));
            }
        }
        if left && !ui.prev_left {
            ui.clicks += 1;
            exit = on_click(&mut ui, &mut dirty);
        }
        if !left && ui.prev_left {
            ui.dragging = None;
        }
        ui.prev_left = left;

        // ---- keyboard: esc closes the menu first, then exits ----
        while let Some(k) = keyboard::pop() {
            if k == 0x1B {
                if ui.menu_open {
                    ui.menu_open = false;
                    ui.menu_hover = None;
                    push_rect(&mut dirty, menu_full_rect(&ui));
                } else {
                    exit = Some(Reason::Esc);
                }
            }
        }

        // ---- menu hover tracking ----
        if ui.menu_open {
            let hov = menu_item_at(&ui);
            if hov != ui.menu_hover {
                ui.menu_hover = hov;
                push_rect(&mut dirty, menu_full_rect(&ui));
            }
        }

        // ---- periodic ticks ----
        let now = pit::uptime_ms();
        if now.saturating_sub(ui.stats_at) >= 500 {
            ui.stats_at = now;
            ui.live = !ui.live;
            if ui.wins[MONITOR].open && !ui.wins[MONITOR].minimized {
                push_rect(&mut dirty, win_content_rect(&ui, MONITOR));
            }
        }
        if now.saturating_sub(ui.clock_at) >= 1000 {
            ui.clock_at = now;
            let (_irq, rx, tx, _drop, _k) = crate::net::e1000::counters();
            ui.net_led = rx + tx != ui.net_prev;
            ui.net_prev = rx + tx;
            push_rect(&mut dirty, tray_rect(&ui));
        }
        if now.saturating_sub(ui.fps_at) >= 1000 {
            ui.fps = (ui.frames as u64 * 1000 / (now - ui.fps_at).max(1)) as u32;
            ui.frames = 0;
            ui.fps_at = now;
        }

        // ---- render: dirty rects into back, blit, cursor stamp ----
        let cur_rect = (ui.cur_x, ui.cur_y, SAVE_W as i32, SAVE_H as i32);
        let cursor_redraw =
            moved || dirty.iter().any(|r| rects_intersect(*r, cur_rect));
        if !dirty.is_empty() {
            for r in &dirty {
                compose(&mut back, &grad, w, h, &c, &ui, *r);
            }
        }
        if !dirty.is_empty() || cursor_redraw {
            let mut g = CONSOLE.lock();
            if let Some(con) = g.as_ref() {
                for r in &dirty {
                    let (rx, ry, rw, rh) = intersect_screen(*r, w, h);
                    if rw > 0 && rh > 0 {
                        con.blit_from(
                            &back,
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
                if moved {
                    // erase the old sprite by blitting the clean scene over it
                    let (ox, oy) = (ocx.max(0) as usize, ocy.max(0) as usize);
                    con.blit_from(&back, w, ox, oy, ox, oy, SAVE_W, SAVE_H);
                }
                if cursor_redraw {
                    stamp_cursor(con, ui.cur_x, ui.cur_y);
                }
            }
            dirty.clear();
        }

        ui.frames += 1;
        // one frame: sleep = yield the cpu, irqs keep accumulating motion
        sched::ksyscall(sched::SYS_SLEEP, 16, 0, 0);
    }

    let reason = exit.unwrap_or(Reason::Esc);
    let (cx, cy) = (ui.cur_x, ui.cur_y);
    let (moves, clicks, drags) = (ui.moves, ui.clicks, ui.drags);
    let (frames, fps) = (ui.frames, ui.fps);
    let pkts = mouse::packets();

    match reason {
        Reason::Reboot => {
            console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
            console::redraw_all_global();
            crate::klog!(
                "gui: exit reason={} cursor=({},{}) moves={} clicks={} drags={} packets={}",
                reason.as_str(),
                cx,
                cy,
                moves,
                clicks,
                drags,
                pkts
            );
            crate::klog!("gui: reboot requested from the start menu");
            crate::cpu::reboot();
        }
        Reason::Halt => {
            // freeze on the farewell screen; GUI_ACTIVE stays up so kstat
            // cannot scribble over it. Never returns.
            draw_halt_screen(&mut back, &grad, w, h, &c);
            {
                let mut g = CONSOLE.lock();
                if let Some(con) = g.as_ref() {
                    con.blit_from(&back, w, 0, 0, 0, 0, w, h);
                }
            }
            crate::klog!(
                "gui: exit reason={} cursor=({},{}) moves={} clicks={} drags={} packets={}",
                reason.as_str(),
                cx,
                cy,
                moves,
                clicks,
                drags,
                pkts
            );
            crate::klog!("gui: halted from the start menu - screen frozen");
            loop {
                sched::ksyscall(sched::SYS_SLEEP, 100, 0, 0);
            }
        }
        Reason::Esc | Reason::AllClosed => {
            console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
            console::redraw_all_global();
            crate::klog!(
                "gui: exit reason={} cursor=({},{}) moves={} clicks={} drags={} packets={} frames={} fps={}",
                reason.as_str(),
                cx,
                cy,
                moves,
                clicks,
                drags,
                pkts,
                frames,
                fps
            );
        }
    }
}
