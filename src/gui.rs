//! GLM OS v1.0 — the framebuffer GUI.
//!
//! A kernel-space desktop that takes the screen over from the text console
//! while it runs: PS/2-mouse cursor, a draggable "system monitor" window,
//! click-to-close. Event-driven dirty-region rendering — the desktop
//! pattern is drawn once, the window only on drag, the cursor via a small
//! save/restore region. `Esc` or the close button hands the text screen
//! back through `console::redraw_all_global()`.

use alloc::format;
use alloc::string::String;

use crate::console::{self, Rgb, CONSOLE};
use crate::cpu::keyboard;
use crate::cpu::mouse;
use crate::cpu::pit;
use crate::sched;

// ---------------- palette ----------------

const DESK0: Rgb = Rgb(17, 20, 38);
const DESK1: Rgb = Rgb(11, 13, 26);
const TASKBAR_BG: Rgb = Rgb(8, 9, 16);
const TASKBAR_EDGE: Rgb = Rgb(64, 70, 96);
const WIN_BG: Rgb = Rgb(24, 27, 36);
const WIN_EDGE: Rgb = Rgb(80, 86, 108);
const SHADOW: Rgb = Rgb(4, 4, 7);
const TITLE_BG: Rgb = Rgb(40, 96, 210);
const TITLE_FG: Rgb = Rgb(238, 240, 246);
const TEXT_FG: Rgb = Rgb(196, 203, 216);
const ACCENT: Rgb = Rgb(94, 226, 141);
const WARN: Rgb = Rgb(255, 220, 93);
const BTN_BG: Rgb = Rgb(58, 62, 76);
const BTN_EDGE: Rgb = Rgb(120, 126, 148);
const BTN_FG: Rgb = Rgb(235, 235, 235);
const CLOSE_BG: Rgb = Rgb(200, 58, 58);

// ---------------- geometry ----------------

const WIN_W: usize = 380;
const WIN_H: usize = 244;
const TITLE_H: usize = 22;
const TASKBAR_H: usize = 24;
const LINE_H: usize = 14; // scale-1 text line pitch
const MARGIN: usize = 12;

const CUR_W: usize = 12;
const CUR_H: usize = 18;
const SAVE_W: usize = 16;
const SAVE_H: usize = 24;

/// 12x18 arrow, 'W' = white fill, 'K' = dark outline, '.' = transparent.
const CURSOR: [&str; CUR_H] = [
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

// ---------------- state ----------------

struct Ui {
    w: usize,
    h: usize,
    cur_x: i32,
    cur_y: i32,
    win_x: i32,
    win_y: i32,
    save: [u32; SAVE_W * SAVE_H],
    dragging: bool,
    drag_dx: i32,
    drag_dy: i32,
    prev_left: bool,
    moves: u64,
    clicks: u64,
    drags: u64,
    stats_at: u64,
    live: bool,
}

impl Ui {
    fn taskbar_y(&self) -> i32 {
        self.h as i32 - TASKBAR_H as i32
    }

    fn clamp_cur(&mut self) {
        self.cur_x = self.cur_x.clamp(0, self.w as i32 - 2);
        self.cur_y = self.clamp_cur_y();
    }

    fn clamp_cur_y(&mut self) -> i32 {
        self.cur_y = self.cur_y.clamp(0, self.h as i32 - 2);
        self.cur_y
    }

    fn clamp_win(&mut self) {
        self.win_x = self.win_x.clamp(0, self.w as i32 - WIN_W as i32 - 2);
        self.win_y = self.win_y.clamp(0, self.taskbar_y() - WIN_H as i32 - 2);
    }
}

// ---------------- entry ----------------

/// Take the screen over, run the desktop until Esc / close, then hand the
/// text console back. Runs in the context of the shell task; the scheduler
/// keeps everyone else alive through our SYS_SLEEP frames.
pub fn run() {
    console::GUI_ACTIVE.store(true, core::sync::atomic::Ordering::Relaxed);
    crate::klog!("gui: enter");

    // fresh input slate: whatever was typed before `gui` stays in the line
    keyboard::drain();

    let mut ui = {
        let mut g = CONSOLE.lock();
        let Some(c) = g.as_mut() else {
            console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
            crate::klog!("gui: no framebuffer console, abort");
            return;
        };
        let (w, h) = (c.width(), c.height());
        let win_x = ((w - WIN_W) / 2) as i32;
        let win_y = (((h - TASKBAR_H - WIN_H) / 2).saturating_sub(24)) as i32;
        Ui {
            w,
            h,
            cur_x: (w / 2) as i32,
            cur_y: (h / 2) as i32,
            win_x,
            win_y,
            save: [0; SAVE_W * SAVE_H],
            dragging: false,
            drag_dx: 0,
            drag_dy: 0,
            prev_left: false,
            moves: 0,
            clicks: 0,
            drags: 0,
            stats_at: 0,
            live: true,
        }
    };
    ui.clamp_win();

    // --- initial scene: desktop, taskbar, window, cursor ---
    {
        let mut g = CONSOLE.lock();
        if let Some(c) = g.as_mut() {
            draw_desktop(c, &ui);
            draw_window(c, &ui);
            save_under(c, &mut ui);
            draw_cursor(c, &ui);
        }
    }

    let mut exit_reason: &str = "esc";
    loop {
        // ---- motion ----
        let (dx, dy, btns) = mouse::take();
        let left = btns & 1 != 0;
        if dx != 0 || dy != 0 {
            ui.moves += 1;
            ui.cur_x += dx;
            ui.cur_y += dy;
            ui.clamp_cur();
            if ui.dragging {
                let (old_x, old_y) = (ui.win_x, ui.win_y);
                ui.win_x = ui.cur_x - ui.drag_dx;
                ui.win_y = ui.cur_y - ui.drag_dy;
                ui.clamp_win();
                ui.drags += 1;
                let mut g = CONSOLE.lock();
                if let Some(c) = g.as_mut() {
                    erase_window(c, &ui, old_x, old_y);
                    draw_window(c, &ui);
                    save_under(c, &mut ui);
                    draw_cursor(c, &ui);
                }
            } else {
                let mut g = CONSOLE.lock();
                if let Some(c) = g.as_mut() {
                    restore_under(c, &ui);
                    save_under(c, &mut ui);
                    draw_cursor(c, &ui);
                }
            }
        }

        // ---- button edges ----
        if left && !ui.prev_left {
            ui.clicks += 1;
            if hit_close(&ui) || hit_button(&ui) {
                exit_reason = if hit_close(&ui) { "close-x" } else { "close-btn" };
                break;
            }
            if hit_title(&ui) {
                ui.dragging = true;
                ui.drag_dx = ui.cur_x - ui.win_x;
                ui.drag_dy = ui.cur_y - ui.win_y;
            }
        }
        if !left && ui.prev_left && ui.dragging {
            ui.dragging = false;
        }
        ui.prev_left = left;

        // ---- keyboard: esc exits ----
        let mut esc = false;
        while let Some(k) = keyboard::pop() {
            if k == 0x1B {
                esc = true;
            }
        }
        if esc {
            break;
        }

        // ---- periodic stats refresh ----
        let now = pit::uptime_ms();
        if now.saturating_sub(ui.stats_at) >= 500 {
            ui.stats_at = now;
            ui.live = !ui.live;
            let mut g = CONSOLE.lock();
            if let Some(c) = g.as_mut() {
                draw_stats(c, &ui);
                // the stats fill may have painted over the cursor sprite
                // (the window sits at screen center) -> re-overlay it
                restore_under(c, &ui);
                save_under(c, &mut ui);
                draw_cursor(c, &ui);
            }
        }

        // one frame: sleep = yield the cpu, irqs keep accumulating motion
        sched::ksyscall(sched::SYS_SLEEP, 16, 0, 0);
    }

    let (cx, cy) = (ui.cur_x, ui.cur_y);
    let (wx, wy) = (ui.win_x, ui.win_y);
    let (moves, clicks, drags) = (ui.moves, ui.clicks, ui.drags);
    let pkts = mouse::packets();

    console::GUI_ACTIVE.store(false, core::sync::atomic::Ordering::Relaxed);
    console::redraw_all_global();
    crate::klog!(
        "gui: exit reason={} cursor=({},{}) window=({},{}) moves={} clicks={} drags={} packets={}",
        exit_reason,
        cx,
        cy,
        wx,
        wy,
        moves,
        clicks,
        drags,
        pkts
    );
}

// ---------------- hit testing ----------------

fn hit_title(ui: &Ui) -> bool {
    let x = ui.cur_x;
    let y = ui.cur_y;
    x >= ui.win_x
        && x < ui.win_x + WIN_W as i32 - 24
        && y >= ui.win_y
        && y < ui.win_y + TITLE_H as i32
}

fn hit_close(ui: &Ui) -> bool {
    let x = ui.cur_x;
    let y = ui.cur_y;
    x >= ui.win_x + WIN_W as i32 - 24
        && x < ui.win_x + WIN_W as i32 - 6
        && y >= ui.win_y + 4
        && y < ui.win_y + 4 + 14
}

fn hit_button(ui: &Ui) -> bool {
    let x = ui.cur_x;
    let y = ui.cur_y;
    let bx = ui.win_x + WIN_W as i32 - 88;
    let by = ui.win_y + WIN_H as i32 - 28;
    x >= bx && x < bx + 72 && y >= by && y < by + 18
}

// ---------------- drawing ----------------

fn desk_px(ui: &Ui, x: usize, y: usize) -> Rgb {
    if y as i32 >= ui.taskbar_y() {
        TASKBAR_BG
    } else if ((x >> 3) + (y >> 3)) & 1 == 0 {
        DESK0
    } else {
        DESK1
    }
}

fn draw_desktop(c: &console::Console, ui: &Ui) {
    for y in 0..ui.h {
        for x in 0..ui.w {
            c.px(x, y, &desk_px(ui, x, y));
        }
    }
    // taskbar top edge
    let ty = ui.taskbar_y() as usize;
    for x in 0..ui.w {
        c.px(x, ty, &TASKBAR_EDGE);
    }
    // taskbar caption
    draw_str(c, "  GLM OS 1.0 desktop", 4, ty + 8, &TEXT_FG, &TASKBAR_BG, 1);
    let hint = "mouse: ps/2 | drag the title bar | esc quits";
    let hx = ui.w.saturating_sub(hint.len() * 8 + 8);
    draw_str(c, hint, hx, ty + 8, &TASKBAR_EDGE, &TASKBAR_BG, 1);
}

fn draw_window(c: &console::Console, ui: &Ui) {
    let (wx, wy) = (ui.win_x as usize, ui.win_y as usize);

    // drop shadow (offset 4px right+down)
    fill_rect(c, wx + 4, wy + 4, WIN_W, WIN_H, &SHADOW);
    // body + border
    fill_rect(c, wx, wy, WIN_W, WIN_H, &WIN_BG);
    fill_rect(c, wx, wy, WIN_W, 1, &WIN_EDGE);
    fill_rect(c, wx, wy + WIN_H - 1, WIN_W, 1, &WIN_EDGE);
    fill_rect(c, wx, wy, 1, WIN_H, &WIN_EDGE);
    fill_rect(c, wx + WIN_W - 1, wy, 1, WIN_H, &WIN_EDGE);
    // title bar
    fill_rect(c, wx + 1, wy + 1, WIN_W - 2, TITLE_H - 1, &TITLE_BG);
    draw_str(
        c,
        "GLM OS 1.0 - system monitor",
        wx + 8,
        wy + 7,
        &TITLE_FG,
        &TITLE_BG,
        1,
    );
    // close box
    let (bx, by) = (wx + WIN_W - 24, wy + 4);
    fill_rect(c, bx, by, 18, 14, &CLOSE_BG);
    draw_str(c, "x", bx + 6, by + 3, &TITLE_FG, &CLOSE_BG, 1);

    draw_stats(c, ui);
}

fn erase_window(c: &console::Console, ui: &Ui, old_x: i32, old_y: i32) {
    // repaint the pattern over the window's PREVIOUS footprint (+ shadow);
    // the caller redraws the window at its new position right after, so
    // overlapping rects resolve correctly
    let (wx, wy) = (old_x as usize, old_y as usize);
    for y in wy..(wy + WIN_H + 4) {
        for x in wx..(wx + WIN_W + 4) {
            c.px(x, y, &desk_px(ui, x, y));
        }
    }
}

fn draw_stats(c: &console::Console, ui: &Ui) {
    let (wx, wy) = (ui.win_x as usize, ui.win_y as usize);
    let cx0 = wx + MARGIN;
    let cy0 = wy + TITLE_H + 8;

    // content background
    fill_rect(
        c,
        wx + 2,
        wy + TITLE_H + 1,
        WIN_W - 4,
        WIN_H - TITLE_H - 2,
        &WIN_BG,
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
        format!("uptime:  {}h {}m {}s", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60),
        format!("tasks:   {} alive   ctx switches: {}", tasks_alive, sched::switches()),
        format!("memory:  {}/{} frames used", fs.used, fs.total),
        match heap {
            Some(h) => format!(
                "heap:    {} KiB in use ({} allocs)",
                h.allocated / 1024,
                h.allocs
            ),
            None => String::from("heap:    offline"),
        },
        format!("mouse:   {} packets   resync {}", mouse::packets(), mouse::resyncs()),
        format!("cursor:  {}, {}", ui.cur_x, ui.cur_y),
        format!("net:     rx {} tx {} drop {} (irq {})", rx, tx, drop, irq_n),
        String::from("ring 3 + smp + networking + now a desktop"),
    ];

    for (i, line) in lines.iter().enumerate() {
        let fg = if i == 7 { ACCENT } else { TEXT_FG };
        draw_str(c, line, cx0, cy0 + i * LINE_H, &fg, &WIN_BG, 1);
    }

    // live dot: blinks on the 500 ms stats tick
    let dot = if ui.live { ACCENT } else { WIN_BG };
    fill_rect(c, wx + WIN_W - 14, wy + TITLE_H + 10, 6, 6, &dot);

    // [ close ] button
    let (bx, by) = (wx + WIN_W - 88, wy + WIN_H - 28);
    fill_rect(c, bx, by, 72, 18, &BTN_BG);
    fill_rect(c, bx, by, 72, 1, &BTN_EDGE);
    fill_rect(c, bx, by + 17, 72, 1, &BTN_EDGE);
    fill_rect(c, bx, by, 1, 18, &BTN_EDGE);
    fill_rect(c, bx + 71, by, 1, 18, &BTN_EDGE);
    draw_str(c, "close", bx + 18, by + 5, &BTN_FG, &BTN_BG, 1);

    // hint line
    draw_str(
        c,
        "esc exits - drag title to move",
        wx + MARGIN,
        wy + WIN_H - 25,
        &WARN,
        &WIN_BG,
        1,
    );
}

fn fill_rect(c: &console::Console, x0: usize, y0: usize, w: usize, h: usize, col: &Rgb) {
    for y in y0..(y0 + h) {
        for x in x0..(x0 + w) {
            c.px(x, y, col);
        }
    }
}

fn draw_str(c: &console::Console, s: &str, x: usize, y: usize, fg: &Rgb, bg: &Rgb, scale: usize) {
    for (i, &b) in s.as_bytes().iter().enumerate() {
        c.draw_char_px(b, x + i * 8 * scale, y, fg, bg, scale);
    }
}

// ---------------- cursor ----------------

fn save_under(c: &console::Console, ui: &mut Ui) {
    let (x0, y0) = (ui.cur_x as usize, ui.cur_y as usize);
    for gy in 0..SAVE_H {
        for gx in 0..SAVE_W {
            ui.save[gy * SAVE_W + gx] = c.grab(x0 + gx, y0 + gy);
        }
    }
}

fn restore_under(c: &console::Console, ui: &Ui) {
    let (x0, y0) = (ui.cur_x as usize, ui.cur_y as usize);
    for gy in 0..SAVE_H {
        for gx in 0..SAVE_W {
            let p = ui.save[gy * SAVE_W + gx];
            if p != 0 {
                c.px_packed(x0 + gx, y0 + gy, p);
            }
        }
    }
}

fn draw_cursor(c: &console::Console, ui: &Ui) {
    let (x0, y0) = (ui.cur_x as usize, ui.cur_y as usize);
    for (row, spec) in CURSOR.iter().enumerate() {
        for (col, ch) in spec.bytes().enumerate() {
            match ch {
                b'W' => c.px(x0 + col, y0 + row, &Rgb(245, 245, 245)),
                b'K' => c.px(x0 + col, y0 + row, &Rgb(12, 12, 16)),
                _ => {}
            }
        }
    }
}
