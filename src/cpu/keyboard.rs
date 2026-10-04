//! PS/2 keyboard (port 1, scancode set 1), IRQ1-driven.
//! SPSC ring: producer = IRQ handler (BSP only — the PIC line lands on
//! the boot CPU), consumer = any task on any CPU. v0.4 SMP: pops are
//! serialized with a spinlock because several CPUs may pop concurrently
//! (timer duties + readchar syscalls).
//!
//! v2.5: the 0xE0 extended prefix now decodes navigation keys — arrows,
//! Home/End, Delete, PgUp/PgDn — as high-bit key codes (0x80..=0x88),
//! and the 0xE1 prefix (pause key, 7-byte sequence) is skipped whole.
//! The high codes collide with nothing printable, so every consumer
//! (shell, terminal windows, EV_KEY events) can match on them directly.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use crate::io::ports::{inb, outb};
use crate::sync::Spinlock;

const RING_SIZE: usize = 512;

static RING: [core::sync::atomic::AtomicU8; RING_SIZE] =
    [const { core::sync::atomic::AtomicU8::new(0) }; RING_SIZE];
static HEAD: AtomicUsize = AtomicUsize::new(0); // producer
static TAIL: AtomicUsize = AtomicUsize::new(0); // consumer

/// Guards head/tail updates now that there are multiple consumers.
static RING_LOCK: Spinlock<()> = Spinlock::new(());

static EXT_PREFIX: AtomicBool = AtomicBool::new(false);
static SHIFT: AtomicBool = AtomicBool::new(false);
/// v2.7: Ctrl modifier (scancode 0x1D). Held-state for the interrupt
/// combo: Ctrl+C decodes to 0x03, the terminal interrupt byte.
static CTRL: AtomicBool = AtomicBool::new(false);
/// v2.5: bytes still to swallow after a 0xE1 prefix (pause key).
static E1_SKIP: AtomicU8 = core::sync::atomic::AtomicU8::new(0);

// v2.5: extended key codes (set-1 scancodes behind the 0xE0 prefix).
// High-bit bytes, deliberately outside ASCII so they can ride the same
// single-byte stream as everything else.
pub const KEY_UP: u8 = 0x80;
pub const KEY_DOWN: u8 = 0x81;
pub const KEY_LEFT: u8 = 0x82;
pub const KEY_RIGHT: u8 = 0x83;
pub const KEY_HOME: u8 = 0x84;
pub const KEY_END: u8 = 0x85;
pub const KEY_DEL: u8 = 0x86;
pub const KEY_PGUP: u8 = 0x87;
pub const KEY_PGDN: u8 = 0x88;

/// v2.5: decode one 0xE0-prefixed scancode (press or release) into a
/// key code; None for everything we do not need (modifiers, win keys,
/// releases). The numpad duplicates its arrows behind 0xE0 too — they
/// decode to the same keys, which is exactly what we want.
fn translate_ext(sc: u8) -> Option<u8> {
    match sc {
        0x48 => Some(KEY_UP),
        0x50 => Some(KEY_DOWN),
        0x4B => Some(KEY_LEFT),
        0x4D => Some(KEY_RIGHT),
        0x47 => Some(KEY_HOME),
        0x4F => Some(KEY_END),
        0x53 => Some(KEY_DEL),
        0x49 => Some(KEY_PGUP),
        0x51 => Some(KEY_PGDN),
        _ => None,
    }
}

pub fn init() {
    // Enable PS/2 port 1 clock line + interrupts (controller config best-effort)
    unsafe {
        outb(0x64, 0xAE); // enable port 1
    }
}

pub fn on_irq() {
    let sc = unsafe { inb(0x60) };
    decode(sc);
}

fn push(c: u8) {
    let _g = RING_LOCK.lock();
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RING_SIZE;
    if next == TAIL.load(Ordering::Acquire) {
        return; // ring full, drop
    }
    RING[head].store(c, Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

/// Consumer: pop next decoded key event, if any (multi-CPU safe).
pub fn pop() -> Option<u8> {
    let _g = RING_LOCK.lock();
    let tail = TAIL.load(Ordering::Relaxed);
    let head = HEAD.load(Ordering::Acquire);
    if tail == head {
        return None;
    }
    let c = RING[tail].load(Ordering::Relaxed);
    TAIL.store((tail + 1) % RING_SIZE, Ordering::Release);
    Some(c)
}

/// Drop every buffered key event (used before/after user tasks run).
pub fn drain() {
    while pop().is_some() {}
}

fn decode(sc: u8) {
    // v2.5: pause sends 0xE1 then six more bytes — swallow them whole
    let skip = E1_SKIP.load(Ordering::Relaxed);
    if skip > 0 {
        E1_SKIP.store(skip - 1, Ordering::Relaxed);
        return;
    }
    if sc == 0xE1 {
        E1_SKIP.store(6, Ordering::Relaxed);
        return;
    }
    if sc == 0xE0 {
        EXT_PREFIX.store(true, Ordering::Relaxed);
        return;
    }
    if EXT_PREFIX.swap(false, Ordering::Relaxed) {
        // v2.5: releases (0xE0 0xNN with 0x80 set) and unknown extended
        // keys are dropped; the ones we need decode into high key codes
        if sc & 0x80 == 0 {
            if let Some(k) = translate_ext(sc) {
                push(k);
            }
        }
        return;
    }
    if sc & 0x80 != 0 {
        // key release: track shift
        let released = sc & 0x7F;
        if released == 0x2A || released == 0x36 {
            SHIFT.store(false, Ordering::Relaxed);
        }
        if released == 0x1D {
            CTRL.store(false, Ordering::Relaxed);
        }
        return;
    }
    match sc {
        0x2A | 0x36 => SHIFT.store(true, Ordering::Relaxed),
        0x1D => CTRL.store(true, Ordering::Relaxed),
        0x1C => push(b'\n'),
        0x0E => push(0x08), // backspace
        0x01 => push(0x1B), // esc (v1.0: the GUI's exit key)
        _ => {
            // v2.7: Ctrl+C — the terminal interrupt. 0x03 rides the same
            // byte stream as everything else; the IRQ-side flag (atomic
            // store, no locks) lets jobs::jobd deliver SIGINT to the
            // foreground task(s) from task context. Other Ctrl combos are
            // swallowed for now — emitting raw control letters would
            // surprise every consumer.
            if CTRL.load(Ordering::Relaxed) {
                if sc == 0x2E {
                    crate::jobs::note_console_intr();
                    push(0x03);
                }
                return;
            }
            if let Some(c) = translate(sc) {
                push(c);
            }
        }
    }
}

fn translate(sc: u8) -> Option<u8> {
    const NORMAL: &[u8] = b"??1234567890-=??qwertyuiop[]??asdfghjkl;'`??zxcvbnm,./";
    const SHIFTED: &[u8] = b"??!@#$%^&*()_+??QWERTYUIOP{}??ASDFGHJKL:\"~??ZXCVBNM<>?";
    // v1.8: backslash / pipe (0x2B) — the pipeline operator's key, missing
    // from the table since v0.1 (its slot doubled as a '?')
    if sc == 0x2B {
        return Some(if SHIFT.load(Ordering::Relaxed) { b'|' } else { b'\\' });
    }
    if (sc as usize) < NORMAL.len() {
        let c = NORMAL[sc as usize];
        if c == b'?' {
            return None;
        }
        Some(if SHIFT.load(Ordering::Relaxed) {
            SHIFTED[sc as usize]
        } else {
            c
        })
    } else if sc == 0x39 {
        Some(b' ')
    } else {
        None
    }
}
