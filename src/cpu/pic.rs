//! 8259 PIC: remapped to vectors 0x20 (master) / 0x28 (slave).

use core::arch::asm;

use crate::io::ports::{inb, io_wait, outb};

pub const MASTER_BASE: u8 = 0x20;
pub const SLAVE_BASE: u8 = 0x28;

const CMD_MASTER: u16 = 0x20;
const CMD_SLAVE: u16 = 0xA0;
const DATA_MASTER: u16 = 0x21;
const DATA_SLAVE: u16 = 0xA1;

#[inline]
fn pause() {
    unsafe { asm!("pause", options(nomem, nostack, preserves_flags)) }
}

pub fn init() {
    unsafe {
        outb(CMD_MASTER, 0x11); // ICW1: init + ICW4
        io_wait();
        outb(CMD_SLAVE, 0x11);
        io_wait();
        outb(DATA_MASTER, MASTER_BASE); // ICW2: vector base
        io_wait();
        outb(DATA_SLAVE, SLAVE_BASE);
        io_wait();
        outb(DATA_MASTER, 0x04); // ICW3: slave on irq2
        io_wait();
        outb(DATA_SLAVE, 0x02);
        io_wait();
        outb(DATA_MASTER, 0x01); // ICW4: 8086 mode
        io_wait();
        outb(DATA_SLAVE, 0x01);
        io_wait();

        // Masks: unmask irq0 (PIT), irq1 (keyboard), irq2 (cascade)
        outb(DATA_MASTER, 0b1111_1000);
        outb(DATA_SLAVE, 0b1111_1111);
    }
    let _ = pause;
}

pub unsafe fn eoi_master() {
    outb(CMD_MASTER, 0x20);
}

pub unsafe fn eoi_slave() {
    outb(CMD_SLAVE, 0x20);
}

/// Read the in-service register and check bit 7 (spurious detection).
pub fn isr_master_bit7() -> bool {
    unsafe {
        outb(CMD_MASTER, 0x0B); // ISR read command
        inb(CMD_MASTER) & 0x80 != 0
    }
}

pub fn isr_slave_bit7() -> bool {
    unsafe {
        outb(CMD_SLAVE, 0x0B);
        inb(CMD_SLAVE) & 0x80 != 0
    }
}
