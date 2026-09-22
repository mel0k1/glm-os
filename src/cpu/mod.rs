//! CPU subsystem: GDT, IDT, PIC, PIT, APIC, keyboard + machine reset.

pub mod apic;
pub mod gdt;
pub mod idt;
pub mod keyboard;
pub mod pic;
pub mod pit;

use crate::io::ports::{hlt, outb};

/// Bring up the interrupt stack and enable interrupts.
pub fn init() {
    gdt::init();
    idt::init();
    pic::init();
    pit::init();
    keyboard::init();
    enable_interrupts();
}

pub fn enable_interrupts() {
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
}

/// 8042 controller pulse reset — the classic PC reboot.
pub fn reboot() -> ! {
    loop {
        // wait for the input buffer to be empty
        loop {
            let status = unsafe { crate::io::ports::inb(0x64) };
            if status & 0x02 == 0 {
                break;
            }
            hlt();
        }
        unsafe { outb(0x64, 0xFE) }; // pulse reset line
        hlt();
    }
}
