//! PS/2 keyboard (port 1, scancode set 1), IRQ1-driven.
//! Lock-free SPSC ring: producer = IRQ handler, consumer = shell loop.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::io::ports::{inb, outb};

const RING_SIZE: usize = 512;

static RING: [core::sync::atomic::AtomicU8; RING_SIZE] =
    [const { core::sync::atomic::AtomicU8::new(0) }; RING_SIZE];
static HEAD: AtomicUsize = AtomicUsize::new(0); // producer
static TAIL: AtomicUsize = AtomicUsize::new(0); // consumer

static EXT_PREFIX: AtomicBool = AtomicBool::new(false);
static SHIFT: AtomicBool = AtomicBool::new(false);

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
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % RING_SIZE;
    if next == TAIL.load(Ordering::Acquire) {
        return; // ring full, drop
    }
    RING[head].store(c, Ordering::Relaxed);
    HEAD.store(next, Ordering::Release);
}

/// Consumer: pop next decoded key event, if any.
pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    let head = HEAD.load(Ordering::Acquire);
    if tail == head {
        return None;
    }
    let c = RING[tail].load(Ordering::Relaxed);
    TAIL.store((tail + 1) % RING_SIZE, Ordering::Release);
    Some(c)
}

fn decode(sc: u8) {
    if sc == 0xE0 {
        EXT_PREFIX.store(true, Ordering::Relaxed);
        return;
    }
    if EXT_PREFIX.swap(false, Ordering::Relaxed) {
        // extended keys (arrows etc.) ignored in v0.1
        return;
    }
    if sc & 0x80 != 0 {
        // key release: track shift
        let released = sc & 0x7F;
        if released == 0x2A || released == 0x36 {
            SHIFT.store(false, Ordering::Relaxed);
        }
        return;
    }
    match sc {
        0x2A | 0x36 => SHIFT.store(true, Ordering::Relaxed),
        0x1C => push(b'\n'),
        0x0E => push(0x08), // backspace
        _ => {
            if let Some(c) = translate(sc) {
                push(c);
            }
        }
    }
}

fn translate(sc: u8) -> Option<u8> {
    const NORMAL: &[u8] = b"??1234567890-=??qwertyuiop[]??asdfghjkl;'`??zxcvbnm,./";
    const SHIFTED: &[u8] = b"??!@#$%^&*()_+??QWERTYUIOP{}??ASDFGHJKL:\"~??ZXCVBNM<>?";
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
