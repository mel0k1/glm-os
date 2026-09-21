//! Raw x86 port I/O.

use core::arch::asm;

#[inline]
pub fn outb(port: u16, value: u8) {
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags));
    }
}

#[inline]
pub fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe {
        asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

#[inline]
#[allow(dead_code)]
pub fn outl(port: u16, value: u32) {
    unsafe {
        asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags));
    }
}

#[inline]
#[allow(dead_code)]
pub fn inl(port: u16) -> u32 {
    let value: u32;
    unsafe {
        asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Short I/O delay used by legacy devices (ports 0x80 POST).
#[inline]
pub fn io_wait() {
    outb(0x80, 0);
}

#[inline]
pub fn hlt() {
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) }
}
