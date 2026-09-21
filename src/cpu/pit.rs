//! PIT (8254) channel 0 in mode 3: 100 Hz system tick.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::io::ports::outb;

const CH0: u16 = 0x40;
const CMD: u16 = 0x43;
const FREQ_HZ: u64 = 100;
const BASE_HZ: u64 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let divisor = (BASE_HZ / FREQ_HZ) as u16; // 11932
    unsafe {
        outb(CMD, 0x36); // ch0, lobyte/hibyte, mode 3, binary
        outb(CH0, divisor as u8);
        outb(CH0, (divisor >> 8) as u8);
    }
}

pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Uptime in milliseconds (tick = 10 ms).
pub fn uptime_ms() -> u64 {
    ticks() * 10
}
