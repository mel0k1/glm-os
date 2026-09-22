//! PS/2 keyboard (port 1, scancode set 1), IRQ1-driven.
//! SPSC ring: producer = IRQ handler (BSP only — the PIC line lands on
//! the boot CPU), consumer = any task on any CPU. v0.4 SMP: pops are
//! serialized with a spinlock because several CPUs may pop concurrently
//! (timer duties + readchar syscalls).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
        0x01 => push(0x1B), // esc (v1.0: the GUI's exit key)
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
