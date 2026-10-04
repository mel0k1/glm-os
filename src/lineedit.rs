//! v2.5: single-line editor with command history, shared by the text
//! shell and the GUI terminal sessions.
//!
//! The editor keeps one buffer with a caret position and a small ring of
//! history entries. Display is sink-aware, because the two output paths
//! have different semantics for the same byte stream:
//!
//!   * text console (`out_win == 0`): a cell grid with a real caret.
//!     0x08 moves the caret left (row wrap included), printing a char
//!     redraws its cell idempotently — so the editor can stream escape
//!     idioms ("\x08 \x08", reprints, left-moves) straight at it.
//!   * terminal window (`out_win != 0`): an append-only cell stream with
//!     a pop-on-0x08 feed — a streamed reposition would duplicate or eat
//!     cells. Here the editor replaces its owned tail of the live line
//!     atomically through `gui::term_redraw_input` and parks the block
//!     caret via the caret-offset field.
//!
//! Extended keys come from the v2.5 keyboard decode: arrows walk the
//! history, Home/End/Delete/Left/Right edit in place, PgUp/PgDn page
//! through older history entries.

use crate::console::{self, GLM_WHITE};
use crate::cpu::keyboard;

const LINE_MAX: usize = 128;
const HIST_MAX: usize = 16;

pub const LINE_CAP: usize = LINE_MAX;

pub struct LineEdit {
    buf: [u8; LINE_MAX],
    len: usize,
    pos: usize,
    /// cells the editor currently owns on the visible line (after the
    /// prompt) — the unit the erase/redraw idioms work in
    shown: usize,
    /// flat history ring: HIST_MAX entries of LINE_MAX bytes
    hist: [u8; HIST_MAX * LINE_MAX],
    hist_len: [usize; HIST_MAX],
    /// next write slot in the ring (newest entry = head - 1)
    hist_head: usize,
    hist_n: usize,
    /// 0 = editing the live line, k>0 = k steps into the past
    nav: usize,
    stash: [u8; LINE_MAX],
    stash_len: usize,
}

impl LineEdit {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_MAX],
            len: 0,
            pos: 0,
            shown: 0,
            hist: [0; HIST_MAX * LINE_MAX],
            hist_len: [0; HIST_MAX],
            hist_head: 0,
            hist_n: 0,
            nav: 0,
            stash: [0; LINE_MAX],
            stash_len: 0,
        }
    }

    fn in_window(&self) -> Option<u32> {
        let w = crate::sched::current_out_win();
        if w == 0 {
            None
        } else {
            Some(w as u32)
        }
    }

    /// Max input length for this line: terminal windows must stay inside
    /// one feed-time row (a hard-wrapped segment lands in the scrollback
    /// and can never be popped back), the console may use the full 128.
    /// The window budget accounts for the prompt cells already in `cur`.
    fn room(&self) -> usize {
        match self.in_window() {
            Some(id) => crate::gui::term_room(id, self.shown).min(LINE_MAX),
            None => LINE_MAX,
        }
    }

    // ---------------- display helpers ----------------

    fn print_ch(&self, c: u8) {
        let s = [c; 1];
        console::print_color(core::str::from_utf8(&s).unwrap_or("?"), GLM_WHITE);
    }

    fn print_bytes(&self, b: &[u8]) {
        if let Ok(s) = core::str::from_utf8(b) {
            console::print_color(s, GLM_WHITE);
        }
    }

    /// Redraw the whole visible line (console mode). The buffer may have
    /// been replaced (history recall), so the old content is addressed by
    /// cell counts, never by bytes: walk the caret left to the line start
    /// (it sits `pos` cells in), blank the `shown` visible cells, walk
    /// back, then reprint the new buffer up to `to_pos`.
    fn console_redraw(&mut self, to_pos: usize) {
        for _ in 0..self.pos {
            console::print("\x08");
        }
        for _ in 0..self.shown {
            console::print(" ");
        }
        for _ in 0..self.shown {
            console::print("\x08");
        }
        if to_pos > 0 {
            self.print_bytes(&self.buf[..to_pos]);
        }
        self.shown = self.len;
    }

    /// Redraw the whole visible line (window mode): replace the
    /// editor-owned tail of the live line in one locked operation.
    fn window_redraw(&mut self, to_pos: usize) {
        let Some(id) = self.in_window() else { return };
        let mut cells = [(0u8, 0u8); LINE_MAX];
        for i in 0..self.len {
            cells[i] = (self.buf[i], GLM_WHITE);
        }
        let n = self.len;
        let caret_off = n - to_pos;
        crate::gui::term_redraw_input(id, &cells[..n], caret_off, self.shown);
        self.shown = n;
    }

    fn redraw(&mut self, to_pos: usize) {
        if self.in_window().is_some() {
            self.window_redraw(to_pos);
        } else {
            self.console_redraw(to_pos);
        }
    }

    /// Move the caret without changing the buffer (console streams the
    /// move; windows get the cheap structural redraw).
    fn move_to(&mut self, new_pos: usize) {
        let new_pos = new_pos.min(self.len);
        if self.in_window().is_some() {
            self.window_redraw(new_pos);
            self.pos = new_pos;
            return;
        }
        if new_pos > self.pos {
            // caret right: redraw the cells it passes over (idempotent)
            self.print_bytes(&self.buf[self.pos..new_pos]);
        } else if new_pos < self.pos {
            // caret left: pure 0x08 moves (row wrap handled by console)
            for _ in new_pos..self.pos {
                console::print("\x08");
            }
        }
        self.pos = new_pos;
    }

    // ---------------- history ----------------

    fn hist_slot(&self, back: usize) -> usize {
        // newest = hist_head - 1; `back` steps further into the past
        (self.hist_head + HIST_MAX * 2 - 1 - back) % HIST_MAX
    }

    fn load_hist(&mut self, back: usize) {
        let slot = self.hist_slot(back);
        let n = self.hist_len[slot];
        self.buf[..n].copy_from_slice(&self.hist[slot * LINE_MAX..slot * LINE_MAX + n]);
        self.len = n;
        self.pos = n;
    }

    fn remember(&mut self, line: &[u8]) {
        // skip empty lines and exact repeats of the newest entry
        if line.is_empty() {
            return;
        }
        if self.hist_n > 0 {
            let last = self.hist_slot(0);
            let l = self.hist_len[last];
            if l == line.len() && &self.hist[last * LINE_MAX..last * LINE_MAX + l] == line {
                return;
            }
        }
        let slot = self.hist_head;
        self.hist[slot * LINE_MAX..slot * LINE_MAX + line.len()].copy_from_slice(line);
        self.hist_len[slot] = line.len();
        self.hist_head = (self.hist_head + 1) % HIST_MAX;
        if self.hist_n < HIST_MAX {
            self.hist_n += 1;
        }
    }

    fn history_up(&mut self) {
        if self.hist_n == 0 || self.nav >= self.hist_n {
            return;
        }
        if self.nav == 0 {
            // park the live line before leaving it
            self.stash[..self.len].copy_from_slice(&self.buf[..self.len]);
            self.stash_len = self.len;
        }
        self.nav += 1;
        self.load_hist(self.nav - 1);
        self.redraw(self.pos);
    }

    fn history_down(&mut self) {
        if self.nav == 0 {
            return;
        }
        self.nav -= 1;
        if self.nav == 0 {
            // back to the parked live line
            self.buf[..self.stash_len].copy_from_slice(&self.stash[..self.stash_len]);
            self.len = self.stash_len;
            self.pos = self.len;
        } else {
            self.load_hist(self.nav - 1);
        }
        self.redraw(self.pos);
    }

    fn insert_printable(&mut self, c: u8) {
        if self.len >= self.room() {
            return;
        }
        // shift the tail right, store the char
        self.buf.copy_within(self.pos..self.len, self.pos + 1);
        self.buf[self.pos] = c;
        self.len += 1;
        self.pos += 1;
        if self.pos == self.len && self.shown == self.len - 1 {
            // fast path: appending at the end
            self.print_ch(c);
            self.shown = self.len;
        } else {
            // mid-line insert: redraw from the new caret on
            self.redraw(self.pos);
        }
    }

    fn backspace(&mut self) {
        if self.pos == 0 {
            return;
        }
        let at_end = self.pos == self.len && self.shown == self.len;
        self.buf.copy_within(self.pos..self.len, self.pos - 1);
        self.pos -= 1;
        self.len -= 1;
        if at_end {
            // fast path: the classic erase idiom
            console::print("\x08 \x08");
            self.shown -= 1;
        } else {
            self.redraw(self.pos);
        }
    }

    fn forward_delete(&mut self) {
        if self.pos >= self.len {
            return;
        }
        self.buf.copy_within(self.pos + 1..self.len, self.pos);
        self.len -= 1;
        self.redraw(self.pos);
    }

    /// Feed one key event. Returns the submitted line on Enter.
    pub fn feed(&mut self, c: u8, out: &mut [u8]) -> Option<usize> {
        match c {
            b'\n' => {
                // complete the visible line, then submit
                if self.pos < self.len {
                    if self.in_window().is_some() {
                        self.window_redraw(self.len);
                    } else {
                        self.print_bytes(&self.buf[self.pos..self.len]);
                        self.pos = self.len;
                    }
                }
                let n = self.len;
                out[..n].copy_from_slice(&self.buf[..n]);
                // borrow checker: copy the line out before mutating self
                let mut line = [0u8; LINE_MAX];
                line[..n].copy_from_slice(&self.buf[..n]);
                self.remember(&line[..n]);
                self.len = 0;
                self.pos = 0;
                self.shown = 0;
                self.nav = 0;
                self.stash_len = 0;
                Some(n)
            }
            0x03 => {
                // v2.7: Ctrl+C at the prompt — the classic ^C: the line
                // dies (history is not polluted), ^C is echoed, and an
                // empty line is submitted so the caller re-prompts with
                // zero side effects (execute("") is a no-op). When a child
                // is running the shell is parked in SYS_WAIT — the byte
                // never reaches us then; jobs::jobd handles that case.
                if crate::jobs::recent_delivery() {
                    // this very byte already fired a delivery: jobd echoed
                    // ^C, the report line followed — swallow it whole (no
                    // second echo, no extra prompt)
                    self.len = 0;
                    self.pos = 0;
                    self.shown = 0;
                    self.nav = 0;
                    self.stash_len = 0;
                    return None;
                }
                if self.pos < self.len {
                    if self.in_window().is_some() {
                        self.window_redraw(self.len);
                    } else {
                        self.print_bytes(&self.buf[self.pos..self.len]);
                        self.pos = self.len;
                    }
                }
                console::print("^C");
                self.len = 0;
                self.pos = 0;
                self.shown = 0;
                self.nav = 0;
                self.stash_len = 0;
                Some(0)
            }
            0x08 => {
                self.backspace();
                None
            }
            keyboard::KEY_DEL => {
                self.forward_delete();
                None
            }
            keyboard::KEY_LEFT => {
                if self.pos > 0 {
                    self.move_to(self.pos - 1);
                }
                None
            }
            keyboard::KEY_RIGHT => {
                if self.pos < self.len {
                    self.move_to(self.pos + 1);
                }
                None
            }
            keyboard::KEY_HOME => {
                self.move_to(0);
                None
            }
            keyboard::KEY_END => {
                self.move_to(self.len);
                None
            }
            keyboard::KEY_UP => {
                self.history_up();
                None
            }
            keyboard::KEY_DOWN => {
                self.history_down();
                None
            }
            // PgUp/PgDn: page deeper into / back out of history
            keyboard::KEY_PGUP => {
                for _ in 0..4 {
                    self.history_up();
                }
                None
            }
            keyboard::KEY_PGDN => {
                for _ in 0..4 {
                    self.history_down();
                }
                None
            }
            c if c.is_ascii_graphic() || c == b' ' => {
                self.insert_printable(c);
                None
            }
            _ => None,
        }
    }
}
