//! guidemo — a ring-3 GUI application for GLM OS v1.2.
//!
//! Opens a window on the kernel desktop through the int 0x80 GUI syscalls
//! and runs a small animation loop: a ball bouncing inside the window,
//! a frame counter, and a click-to-recolor interaction. Everything the
//! window shows was drawn by THIS unprivileged process: the kernel only
//! composites our backing store onto the desktop.
//!
//! Events handled: EV_CLICK recolors the ball, EV_RESIZE repaints the
//! scene, EV_CLOSE (or a vanished window) ends the loop with exit 0.
//! 'q' quits from the keyboard.

#![no_std]
#![no_main]

use glm_user::{
    gui_ev_parts, exit, fmt_u64, gui_close, gui_event, gui_geo, gui_open, gui_rect, gui_text,
    getpid, sleep_ms, write, EV_CLICK, EV_CLOSE, EV_KEY, EV_RESIZE,
};

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}

// palette (0xRRGGBB; the kernel packs it into the framebuffer format)
const BG: u32 = 0x14181E;
const PANEL: u32 = 0x1B2029;
const ACCENT: u32 = 0x5EE28D;
const CYAN: u32 = 0x46E2E2;
const TEXT: u32 = 0xC6CCD8;
const DIM: u32 = 0x7E8494;
const BALL_COLORS: [u32; 3] = [0xEB5A5A, 0x46E2E2, 0xFFDC5D];
const BALL_R: i32 = 9;

/// Tiny fixed buffer string builder (no heap in ring 3).
struct Sbuf {
    buf: [u8; 48],
    len: usize,
}

fn heapless(a: &str, b: &str) -> Sbuf {
    let mut s = Sbuf { buf: [0; 48], len: 0 };
    for byte in a.bytes().chain(b.bytes()) {
        if s.len < 47 {
            s.buf[s.len] = byte;
            s.len += 1;
        }
    }
    s
}

impl Sbuf {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("?")
    }
}

fn num(v: u64, b: &mut [u8; 20]) -> &str {
    core::str::from_utf8(fmt_u64(v, b)).unwrap_or("?")
}

/// Erase + redraw the ball. The erase rect covers the union of the OLD
/// position (ox, oy) and the NEW one (nx, ny) -- erasing only around the
/// new position leaves a 1px sliver of the old ball behind every frame
/// (classic trail bug). Clamped so it never goes negative.
fn draw_ball(id: i64, ox: i32, oy: i32, nx: i32, ny: i32, col: u32) {
    let x0 = (ox - BALL_R - 2).min(nx - BALL_R - 2).max(0);
    let y0 = (oy - BALL_R - 2).min(ny - BALL_R - 2).max(0);
    let x1 = (ox + BALL_R + 2).max(nx + BALL_R + 2);
    let y1 = (oy + BALL_R + 2).max(ny + BALL_R + 2);
    gui_rect(id, x0, y0, x1 - x0, y1 - y0, BG);
    gui_rect(id, nx - BALL_R, ny - BALL_R, 2 * BALL_R, 2 * BALL_R, col);
}

/// Repaint everything for the current window size (start + every resize).
fn draw_scene(id: i64, w: i32, h: i32, pid: u64, frames: u64, ball: usize) {
    let mut b = [0u8; 20];
    // background + header band
    gui_rect(id, 0, 0, w, h, BG);
    gui_rect(id, 0, 0, w, 14, PANEL);
    gui_text(id, 4, 3, "ring 3 demo - glm-user", ACCENT);
    // static copy
    gui_text(id, 8, 22, "hello from ring 3!", TEXT);
    gui_text(id, 8, 36, "drawn via int 0x80 gui syscalls", DIM);
    gui_text(id, 8, 52, heapless("pid: ", num(pid, &mut b)).as_str(), CYAN);
    gui_text(id, 8, 66, heapless("frames: ", num(frames, &mut b)).as_str(), CYAN);
    gui_text(id, 8, 80, "click = recolor, 'q' = quit", DIM);
    gui_text(id, 8, h - 14, "kernel composites this window", DIM);
    draw_ball(id, 60, 104, 60, 104, BALL_COLORS[ball]);
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    let mut b = [0u8; 20];
    let pid = getpid();

    // the desktop must be running (gui command), else this fails fast
    let id = gui_open("ring 3 demo", 120, 120, 250, 180);
    if id < 0 {
        write("[guidemo ] no gui desktop - run 'gui' first, exit 1\n");
        exit(1);
    }
    let (mut w, mut h) = (250, 180);
    let mut ball = 0usize;
    let mut frames: u64 = 0;
    draw_scene(id, w, h, pid, frames, ball);

    let (mut bx, mut by) = (60, 104);
    let (mut vx, mut vy) = (3, 2);

    loop {
        sleep_ms(30);
        frames += 1;

        // drain the input queue
        loop {
            let ev = gui_event(id);
            if ev == u64::MAX {
                // window/desktop gone (kernel closed it or gui exited)
                exit(0);
            }
            let (t, a, _arg) = gui_ev_parts(ev);
            if t == 0 {
                break; // queue empty
            }
            if t == EV_CLOSE {
                // silently retire: console writes would scribble the desktop
                let _ = gui_close(id);
                exit(0);
            }
            if t == EV_CLICK {
                ball = (ball + 1) % BALL_COLORS.len();
            }
            if t == EV_KEY && a == b'q' as u64 {
                let _ = gui_close(id);
                exit(0);
            }
            if t == EV_RESIZE {
                // the backing store was reset: full repaint at the new size
                if let Some((nw, nh)) = gui_geo(id) {
                    w = nw;
                    h = nh;
                }
                draw_scene(id, w, h, pid, frames, ball);
            }
        }

        // animate the ball inside the CONTENT area (window minus the 4px
        // frame and the 22px title bar: content w-4 x h-24)
        let cw = w - 4;
        let ch = h - 24;
        let (px, py) = (bx, by);
        bx += vx;
        by += vy;
        if bx - BALL_R < 0 {
            bx = BALL_R;
            vx = -vx;
        }
        if bx + BALL_R > cw {
            bx = cw - BALL_R;
            vx = -vx;
        }
        if by - BALL_R < 96 {
            // keep the ball below the text block so it never erases it
            by = 96 + BALL_R;
            vy = -vy;
        }
        if by + BALL_R > ch {
            by = ch - BALL_R;
            vy = -vy;
        }
        draw_ball(id, px, py, bx, by, BALL_COLORS[ball]);

        // frame counter refresh (only the digits line)
        gui_text(id, 8, 66, heapless("frames: ", num(frames, &mut b)).as_str(), CYAN);
    }
}
