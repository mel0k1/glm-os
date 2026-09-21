//! COM1 serial port: the kernel's trusted debug console.
//! 115200 baud, 8N1, polling mode.

use core::arch::asm;
use core::fmt;

use super::ports::{inb, outb};

pub struct SerialPort {
    base: u16,
}

pub static COM1: SerialPort = SerialPort { base: 0x3F8 };

impl SerialPort {
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    pub fn init(&self) {
        let b = self.base;
        outb(b + 1, 0x00); // disable UART interrupts
        outb(b + 3, 0x80); // DLAB on
        outb(b + 0, 0x01); // divisor 1 -> 115200 baud
        outb(b + 1, 0x00);
        outb(b + 3, 0x03); // 8 bits, no parity, 1 stop
        outb(b + 2, 0xC7); // FIFO, clear, 14-byte threshold
        outb(b + 4, 0x0B); // DTR + RTS + OUT2
    }

    fn transmit_empty(&self) -> bool {
        inb(self.base + 5) & 0x20 != 0
    }

    pub fn write_byte(&self, byte: u8) {
        while !self.transmit_empty() {
            unsafe { asm!("pause", options(nomem, nostack, preserves_flags)) }
        }
        outb(self.base, byte);
    }

    pub fn write_bytes(&self, s: &[u8]) {
        for &b in s {
            if b == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(b);
        }
    }
}

impl fmt::Write for &SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_bytes(s.as_bytes());
        Ok(())
    }
}

/// Kernel log: `[glm] message` on COM1.
#[macro_export]
macro_rules! klog {
    ($($arg:tt)*) => {{
        use core::fmt::Write;
        let mut s = &$crate::io::serial::COM1;
        let _ = s.write_str("[glm] ");
        let _ = s.write_fmt(format_args!($($arg)*));
        let _ = s.write_str("\n");
    }};
}
