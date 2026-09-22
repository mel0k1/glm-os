//! PS/2 mouse (port 2 of the i8042 controller), IRQ12-driven. v1.0.
//!
//! The ISR is deliberately minimal (v0.8 lesson: never take a lock inside an
//! interrupt handler that a task may hold): it only decodes the 3-byte packet
//! stream into atomic deltas, button bits and counters. The GUI task consumes
//! motion at its own frame pace with `take()`.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, Ordering as AO};

use crate::io::ports::{inb, outb};

const PS2_DATA: u16 = 0x60;
const PS2_CMD: u16 = 0x64;

// pending motion, screen-axis (+x right, +y down; the PS/2 y axis is
// inverted here once, at decode time, so consumers see screen coords)
static MX: AtomicI32 = AtomicI32::new(0);
static MY: AtomicI32 = AtomicI32::new(0);

// bit0 left, bit1 right, bit2 middle (snapshot of the latest packet)
static BUTTONS: AtomicU8 = AtomicU8::new(0);

static PACKETS: AtomicU64 = AtomicU64::new(0);
static RESYNC: AtomicU64 = AtomicU64::new(0);
static ONLINE: AtomicBool = AtomicBool::new(false);

// ISR-local packet reassembly state (single producer: irq12 lands on BSP)
static IDX: AtomicU8 = AtomicU8::new(0);
static B0: AtomicU8 = AtomicU8::new(0);
static B1: AtomicU8 = AtomicU8::new(0);

/// Probe + init the aux port: enable irq12, unmask the mouse clock line,
/// set stream-mode defaults and enable data reporting. Polled (no irqs yet),
/// every controller access guarded by a timeout so a missing device can
/// never hang the boot.
pub fn init() -> bool {
    unsafe {
        outb(PS2_CMD, 0xA8); // enable aux port (port 2)

        // flush any stale byte from the output buffer
        for _ in 0..64 {
            if inb(PS2_CMD) & 1 != 0 {
                let _ = inb(PS2_DATA);
            } else {
                break;
            }
        }

        // controller config byte: set bit1 (irq12), clear bit5 (mouse clock)
        outb(PS2_CMD, 0x20);
        let mut cfg: u8 = 0;
        let mut got = false;
        for _ in 0..100_000 {
            if inb(PS2_CMD) & 1 != 0 {
                cfg = inb(PS2_DATA);
                got = true;
                break;
            }
        }
        if !got {
            return false;
        }
        cfg |= 0x02;
        cfg &= !0x20;
        outb(PS2_CMD, 0x60);
        outb(PS2_DATA, cfg);

        // 0xF6 = set defaults (stream mode), 0xF4 = enable data reporting
        if !mouse_cmd(0xF6) || !mouse_cmd(0xF4) {
            return false;
        }

        // QEMU (and real 8042s) may still hold a late ACK byte in the
        // output buffer at this point. If it leaks into the IRQ stream it
        // passes the bit3 sync check (0xFA has bit3 set) and desynchronizes
        // the 3-byte packet reassembly. Drain everything before unmasking.
        for _ in 0..64 {
            if inb(PS2_CMD) & 1 != 0 {
                let _ = inb(PS2_DATA);
            } else {
                break;
            }
        }

        crate::cpu::pic::unmask(12);
    }
    ONLINE.store(true, AO::Relaxed);
    true
}

/// Write a command byte to the mouse (port 2) and wait for the 0xFA ack.
fn mouse_cmd(b: u8) -> bool {
    unsafe {
        outb(PS2_CMD, 0xD4);
        outb(PS2_DATA, b);
        for _ in 0..200_000 {
            if inb(PS2_CMD) & 1 != 0 {
                if inb(PS2_DATA) == 0xFA {
                    return true;
                }
            }
        }
    }
    false
}

/// irq12 (vector 0x2c): pull one byte of the packet stream.
pub fn on_irq() {
    let b = unsafe { inb(PS2_DATA) };

    // 0xFE = device asks to resend: the byte stream is broken, resync
    if b == 0xFE {
        RESYNC.fetch_add(1, AO::Relaxed);
        IDX.store(0, AO::Relaxed);
        return;
    }

    match IDX.load(AO::Relaxed) {
        0 => {
            // byte 0: bit3 must be set (packet sync marker) and the
            // overflow bits (0xc0) must be clear. This also rejects stray
            // controller bytes like a late 0xFA ack, which would otherwise
            // pass the bit3 check and desynchronize the reassembly.
            if b & 0x08 == 0 || b & 0xC0 != 0 {
                crate::klog!("mouse: out-of-sync byte {:#04x} dropped", b);
                return;
            }
            B0.store(b, AO::Relaxed);
            IDX.store(1, AO::Relaxed);
        }
        1 => {
            B1.store(b, AO::Relaxed);
            IDX.store(2, AO::Relaxed);
        }
        _ => {
            IDX.store(0, AO::Relaxed);
            let b0 = B0.load(AO::Relaxed);
            let b1 = B1.load(AO::Relaxed);
            let mut dx = b1 as i32;
            let mut dy = b as i32; // this IS the third packet byte
            if b0 & 0x10 != 0 {
                dx -= 256;
            }
            if b0 & 0x20 != 0 {
                dy -= 256;
            }
            // ps/2: +y is up; framebuffer: +y is down -> invert once here
            MX.fetch_add(dx, AO::Relaxed);
            MY.fetch_add(-dy, AO::Relaxed);
            BUTTONS.store(b0 & 0x07, AO::Relaxed);
            let n = PACKETS.fetch_add(1, AO::Relaxed);
            if n < 8 {
                crate::klog!(
                    "mouse: pkt {} bytes=({:#04x},{:#04x},{:#04x}) dx={} dy={} btn={}",
                    n,
                    b0,
                    b1,
                    b,
                    dx,
                    -dy,
                    b0 & 7
                );
            }
        }
    }
}

/// Consumer: swap out pending motion deltas, peek button state.
pub fn take() -> (i32, i32, u8) {
    let dx = MX.swap(0, AO::Relaxed);
    let dy = MY.swap(0, AO::Relaxed);
    let b = BUTTONS.load(AO::Relaxed);
    (dx, dy, b)
}

pub fn packets() -> u64 {
    PACKETS.load(AO::Relaxed)
}

pub fn resyncs() -> u64 {
    RESYNC.load(AO::Relaxed)
}

pub fn online() -> bool {
    ONLINE.load(AO::Relaxed)
}

pub fn buttons() -> u8 {
    BUTTONS.load(AO::Relaxed)
}
