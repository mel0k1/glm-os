//! CPU subsystem: GDT, IDT, PIC, PIT, APIC, SMP, keyboard, mouse + reset.

pub mod apic;
pub mod gdt;
pub mod idt;
pub mod keyboard;
pub mod mouse;
pub mod pic;
pub mod pit;
pub mod smp;

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

/// Machine reset. Tries the ACPI/ICH PCI reset register at 0xCF9 first
/// (full hard reset; reliable on q35 and virtually every modern chipset),
/// then falls back to the classic 8042 controller pulse.
pub fn reboot() -> ! {
    unsafe { outb(0x0CF9, 0x0E) }; // SYS_RST | RST_CPU | RST_HDR
    loop {
        // 8042 controller pulse reset - the classic PC fallback.
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
