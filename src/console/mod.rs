//! GLM OS framebuffer text console.
//!
//! Renders an 8x8 bitmap font (scaled 2x for that chunky retro-kernel look)
//! directly into the Limine-provided linear framebuffer. Colors are packed
//! generically from the framebuffer's channel masks.

pub mod font;

use core::cell::UnsafeCell;
use core::fmt;

use crate::sync::Spinlock;

/// RGB triplet, 0..=255 per channel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Classic 16-color palette, tuned slightly brighter for GLM OS.
pub const PALETTE: [Rgb; 16] = [
    Rgb(0, 0, 0),       // 0 black
    Rgb(28, 74, 189),   // 1 blue
    Rgb(46, 184, 114),  // 2 green
    Rgb(38, 189, 189),  // 3 cyan
    Rgb(196, 57, 57),   // 4 red
    Rgb(173, 71, 199),  // 5 magenta
    Rgb(200, 141, 43),  // 6 brown
    Rgb(184, 184, 184), // 7 light gray
    Rgb(90, 90, 90),    // 8 dark gray
    Rgb(84, 134, 255),  // 9 bright blue
    Rgb(94, 226, 141),  // 10 bright green
    Rgb(70, 226, 226),  // 11 bright cyan
    Rgb(255, 99, 99),   // 12 bright red
    Rgb(219, 114, 255), // 13 bright magenta
    Rgb(255, 220, 93),  // 14 yellow
    Rgb(240, 240, 240), // 15 white
];

pub const GLM_CYAN: u8 = 11;
pub const GLM_GREEN: u8 = 10;
pub const GLM_WHITE: u8 = 15;
pub const GLM_GRAY: u8 = 7;
pub const GLM_MAGENTA: u8 = 13;
pub const GLM_YELLOW: u8 = 14;
pub const GLM_RED: u8 = 12;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

// ---------------------------------------------------------------------------
// Text screen shadow grid (v1.0)
//
// Every cell the console draws is mirrored here so the GUI can take the
// screen over and hand a pixel-identical text console back afterwards.
// Static (no heap dependency): the console runs before the heap allocator.
// Accessed ONLY while holding CONSOLE.
// ---------------------------------------------------------------------------

const MAX_COLS: usize = 128;
const MAX_ROWS: usize = 96;

#[derive(Clone, Copy)]
struct Cell {
    ch: u8,
    fg: Rgb,
    bg: Rgb,
}

struct Grid {
    data: [Cell; MAX_COLS * MAX_ROWS],
    cols: usize,
    rows: usize,
    on: bool,
}

struct GridCell(UnsafeCell<Grid>);
unsafe impl Sync for GridCell {}

static GRID: GridCell = GridCell(UnsafeCell::new(Grid {
    data: [Cell { ch: b' ', fg: Rgb(0, 0, 0), bg: Rgb(0, 0, 0) }; MAX_COLS * MAX_ROWS],
    cols: 0,
    rows: 0,
    on: false,
}));

unsafe fn grid_reset(cols: usize, rows: usize, fg: Rgb, bg: Rgb) {
    let g = GRID.0.get();
    (*g).cols = cols;
    (*g).rows = rows;
    (*g).on = cols <= MAX_COLS && rows <= MAX_ROWS;
    let blank = Cell { ch: b' ', fg, bg };
    for i in 0..MAX_COLS * MAX_ROWS {
        (*g).data[i] = blank;
    }
}

unsafe fn grid_store(col: usize, row: usize, ch: u8, fg: Rgb, bg: Rgb) {
    let g = GRID.0.get();
    if (*g).on && col < (*g).cols && row < (*g).rows {
        (*g).data[row * (*g).cols + col] = Cell { ch, fg, bg };
    }
}

unsafe fn grid_scroll(fg: Rgb, bg: Rgb) {
    let g = GRID.0.get();
    if !(*g).on {
        return;
    }
    let cols = (*g).cols;
    let rows = (*g).rows;
    (*g).data.copy_within(cols..cols * rows, 0);
    let blank = Cell { ch: b' ', fg, bg };
    for i in cols * (rows - 1)..cols * rows {
        (*g).data[i] = blank;
    }
}

unsafe fn grid_clear(fg: Rgb, bg: Rgb) {
    let g = GRID.0.get();
    if !(*g).on {
        return;
    }
    let blank = Cell { ch: b' ', fg, bg };
    for i in 0..(*g).cols * (*g).rows {
        (*g).data[i] = blank;
    }
}

pub struct Console {
    fb_base: *mut u32,
    pitch_px: usize, // u32 pixels per scanline
    width: usize,
    height: usize,
    rsh: u32,
    gsh: u32,
    bsh: u32,
    rsz: u32,
    gsz: u32,
    bsz: u32,
    scale: usize,
    pub cols: usize,
    pub rows: usize,
    pub cx: usize,
    pub cy: usize,
    pub fg: Rgb,
    pub bg: Rgb,
}

unsafe impl Send for Console {}

impl Console {
    /// # Safety
    /// `fb_base` must point to a valid framebuffer of `pitch * height` bytes.
    pub unsafe fn new(
        fb_base: *mut u8,
        pitch: usize,
        width: usize,
        height: usize,
        bpp: usize,
        rsh: u8,
        gsh: u8,
        bsh: u8,
        rsz: u8,
        gsz: u8,
        bsz: u8,
    ) -> Self {
        assert_eq!(bpp, 32, "GLM OS v0.1 framebuffer console requires 32bpp");
        let scale = if width / 8 >= 120 { 2 } else { 1 };
        let cell_w = GLYPH_W * scale;
        let cell_h = GLYPH_H * scale + 2; // 2px inter-line gap for readability
        let cols = width / cell_w;
        let rows = height / cell_h;
        Self {
            fb_base: fb_base.cast::<u32>(),
            pitch_px: pitch / 4,
            width,
            height,
            rsh: rsh as u32,
            gsh: gsh as u32,
            bsh: bsh as u32,
            rsz: rsz as u32,
            gsz: gsz as u32,
            bsz: bsz as u32,
            scale,
            cols,
            rows,
            cx: 0,
            cy: 0,
            fg: PALETTE[GLM_GRAY as usize],
            bg: Rgb(6, 7, 10),
        }
    }

    #[inline]
    fn pack(&self, c: &Rgb) -> u32 {
        let shift = |v: u8, sz: u32| -> u32 {
            if sz == 0 || sz >= 8 {
                v as u32
            } else {
                (v as u32) >> (8 - sz)
            }
        };
        (shift(c.0, self.rsz) << self.rsh)
            | (shift(c.1, self.gsz) << self.gsh)
            | (shift(c.2, self.bsz) << self.bsh)
    }

    #[inline]
    unsafe fn put_px(&self, x: usize, y: usize, packed: u32) {
        if x < self.width && y < self.height {
            core::ptr::write_volatile(self.fb_base.add(y * self.pitch_px + x), packed);
        }
    }

    pub fn fill_rect(&self, x0: usize, y0: usize, w: usize, h: usize, c: &Rgb) {
        let packed = self.pack(c);
        for y in y0..(y0 + h) {
            for x in x0..(x0 + w) {
                unsafe { self.put_px(x, y, packed) };
            }
        }
    }

    pub fn cell_w(&self) -> usize {
        GLYPH_W * self.scale
    }

    pub fn cell_h(&self) -> usize {
        GLYPH_H * self.scale + 2
    }

    fn glyph_for(ch: u8) -> [u8; 8] {
        if (font::FONT_FIRST..font::FONT_FIRST + 95).contains(&ch) {
            font::FONT8[(ch - font::FONT_FIRST) as usize]
        } else {
            font::FONT8[(b'?' - font::FONT_FIRST) as usize]
        }
    }

    pub fn draw_char(&self, ch: u8, col: usize, row: usize, fg: &Rgb, bg: &Rgb) {
        let px = col * self.cell_w();
        let py = row * self.cell_h();
        self.draw_char_px(ch, px, py, fg, bg, self.scale);
    }

    /// Glyph at an arbitrary pixel origin with an explicit scale
    /// (v1.0: the GUI renders scale-1 text inside its window).
    pub fn draw_char_px(&self, ch: u8, px: usize, py: usize, fg: &Rgb, bg: &Rgb, scale: usize) {
        let glyph = Self::glyph_for(ch);
        let fgp = self.pack(fg);
        let bgp = self.pack(bg);
        for gy in 0..GLYPH_H {
            let bits = glyph[gy];
            for gx in 0..GLYPH_W {
                let packed = if bits & (1 << gx) != 0 { fgp } else { bgp };
                for dy in 0..scale {
                    for dx in 0..scale {
                        unsafe { self.put_px(px + gx * scale + dx, py + gy * scale + dy, packed) };
                    }
                }
            }
        }
    }

    fn scroll(&mut self) {
        let ch = self.cell_h();
        unsafe {
            let dst = self.fb_base;
            let src = self.fb_base.add(ch * self.pitch_px);
            let count = (self.height - ch) * self.pitch_px;
            core::ptr::copy(src, dst, count);
        }
        let bgp = self.pack(&self.bg);
        for y in (self.height - ch)..self.height {
            for x in 0..self.width {
                unsafe { self.put_px(x, y, bgp) };
            }
        }
        unsafe { grid_scroll(self.fg, self.bg) };
    }

    fn newline(&mut self) {
        self.cx = 0;
        self.cy += 1;
        if self.cy >= self.rows {
            self.scroll();
            self.cy = self.rows - 1;
        }
    }

    pub fn put_char(&mut self, ch: u8) {
        match ch {
            b'\n' => self.newline(),
            b'\r' => self.cx = 0,
            b'\t' => {
                for _ in 0..4 {
                    self.put_char(b' ');
                }
            }
            _ => {
                if self.cx >= self.cols {
                    self.newline();
                }
                self.draw_char(ch, self.cx, self.cy, &self.fg, &self.bg);
                unsafe { grid_store(self.cx, self.cy, ch, self.fg, self.bg) };
                self.cx += 1;
            }
        }
    }

    pub fn write_str(&mut self, s: &str) {
        for &b in s.as_bytes() {
            self.put_char(if b < 128 { b } else { b'?' });
        }
    }

    pub fn set_color(&mut self, color: u8) {
        self.fg = PALETTE[color as usize];
    }

    pub fn set_fg(&mut self, c: Rgb) {
        self.fg = c;
    }

    pub fn clear(&mut self) {
        self.cx = 0;
        self.cy = 0;
        let bgp = self.pack(&self.bg);
        for y in 0..self.height {
            for x in 0..self.width {
                unsafe { self.put_px(x, y, bgp) };
            }
        }
        unsafe { grid_clear(self.fg, self.bg) };
    }

    // ---------------- pixel API (v1.0: the GUI's canvas) ----------------

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn scale(&self) -> usize {
        self.scale
    }

    /// Write one pixel from an Rgb color.
    pub fn px(&self, x: usize, y: usize, c: &Rgb) {
        let p = self.pack(c);
        unsafe { self.put_px(x, y, p) };
    }

    /// v1.1: expose pixel packing so the GUI's back buffer can hold
    /// pre-packed pixels in the framebuffer's native format.
    pub fn pack_rgb(&self, c: &Rgb) -> u32 {
        self.pack(c)
    }

    /// v1.1: copy a rectangle from a linear packed-pixel buffer (the GUI's
    /// back buffer, `src_pitch` pixels wide) into the visible framebuffer.
    /// Clipped against both the source rect and the screen edges; used for
    /// the double-buffered blit and for cursor-erase stamps.
    pub fn blit_from(
        &self,
        src: &[u32],
        src_pitch: usize,
        sx: usize,
        sy: usize,
        dx: usize,
        dy: usize,
        w: usize,
        h: usize,
    ) {
        if src_pitch == 0 || w == 0 || h == 0 {
            return;
        }
        let src_h = src.len() / src_pitch;
        if sx >= src_pitch || sy >= src_h {
            return;
        }
        let w = w.min(src_pitch - sx);
        let h = h.min(src_h - sy);
        if dx >= self.width || dy >= self.height {
            return;
        }
        let w = w.min(self.width - dx);
        let h = h.min(self.height - dy);
        for row in 0..h {
            let s = (sy + row) * src_pitch + sx;
            let d = (dy + row) * self.pitch_px + dx;
            unsafe {
                core::ptr::copy_nonoverlapping(src.as_ptr().add(s), self.fb_base.add(d), w);
            }
        }
    }

    /// Write one pixel from a pre-packed value (cursor save/restore).
    pub fn px_packed(&self, x: usize, y: usize, packed: u32) {
        unsafe { self.put_px(x, y, packed) };
    }

    /// Read one packed pixel back (region save under the cursor sprite).
    pub fn grab(&self, x: usize, y: usize) -> u32 {
        if x < self.width && y < self.height {
            unsafe { core::ptr::read_volatile(self.fb_base.add(y * self.pitch_px + x)) }
        } else {
            0
        }
    }

    /// Re-render the whole text grid to the framebuffer (v1.0: hands the
    /// screen back to the text console after the GUI exits).
    pub fn redraw_all(&mut self) {
        unsafe {
            let g = GRID.0.get();
            if !(*g).on {
                return;
            }
            for row in 0..(*g).rows {
                for col in 0..(*g).cols {
                    let cell = (*g).data[row * (*g).cols + col];
                    self.draw_char(cell.ch, col, row, &cell.fg, &cell.bg);
                }
            }
        }
    }

    /// Underscore-style cursor: bottom 2 scaled rows of the current cell.
    pub fn cursor_draw(&mut self) {
        let (x, y) = (self.cx * self.cell_w(), self.cy * self.cell_h());
        let h = 2 * self.scale;
        self.fill_rect(x, y + self.cell_h() - h, self.cell_w(), h, &self.fg);
    }

    pub fn cursor_erase(&mut self) {
        let (x, y) = (self.cx * self.cell_w(), self.cy * self.cell_h());
        let h = 2 * self.scale;
        self.fill_rect(x, y + self.cell_h() - h, self.cell_w(), h, &self.bg);
    }
}

// ---------------------------------------------------------------------------
// Global console instance
// ---------------------------------------------------------------------------

pub static CONSOLE: Spinlock<Option<Console>> = Spinlock::new(None);

/// v1.0: true while the framebuffer GUI owns the screen. Background
/// console writers (kstat) check this and stay silent so they cannot
/// scribble over the desktop.
pub static GUI_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Initialize the global console from framebuffer parameters.
pub fn init(fb_base: *mut u8, pitch: usize, width: usize, height: usize, bpp: usize, masks: [u8; 6]) {
    let con = unsafe { Console::new(fb_base, pitch, width, height, bpp, masks[0], masks[1], masks[2], masks[3], masks[4], masks[5]) };
    let (cols, rows, fg, bg) = (con.cols, con.rows, con.fg, con.bg);
    *CONSOLE.lock() = Some(con);
    unsafe { grid_reset(cols, rows, fg, bg) };
}

/// v1.0: repaint the entire text screen from the shadow grid
/// (the GUI's exit path).
pub fn redraw_all_global() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.redraw_all();
    }
}

impl core::fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            self.put_char(if b < 128 { b } else { b'?' });
        }
        Ok(())
    }
}

pub fn print(s: &str) {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.write_str(s);
    }
}

pub fn print_args(args: core::fmt::Arguments) {
    if let Some(c) = CONSOLE.lock().as_mut() {
        let _ = core::fmt::Write::write_fmt(c, args);
    }
}

pub fn set_color_global(color: u8) {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.set_color(color);
    }
}

pub fn print_color(s: &str, color: u8) {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.set_color(color);
        c.write_str(s);
        c.set_color(GLM_GRAY);
    }
}

pub fn newline() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.put_char(b'\n');
    }
}

pub fn cursor_draw() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.cursor_draw();
    }
}

#[allow(dead_code)]
pub fn cursor_erase() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.cursor_erase();
    }
}

pub fn clear() {
    if let Some(c) = CONSOLE.lock().as_mut() {
        c.clear();
    }
}

#[allow(dead_code)]
pub fn size() -> (usize, usize) {
    match CONSOLE.lock().as_ref() {
        Some(c) => (c.cols, c.rows),
        None => (0, 0),
    }
}
