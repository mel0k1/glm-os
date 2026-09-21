//! fault — demonstrates ring-3 memory protection.
//!
//! Deliberately dereferences address 0. The kernel's page-fault handler
//! sees the violation came from ring 3, terminates THIS task and returns
//! to the shell with exit code 139. The kernel itself must survive.

#![no_std]
#![no_main]

use glm_user::{exit, write};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("this is FAULT.ELF, a crash-test dummy.\n");
    write("  it is about to read address 0x0 from ring 3...\n");

    let bad = 0u64 as *const u64;
    let v = unsafe { core::ptr::read_volatile(bad) };

    // unreachable: the kernel kills the task on the page fault
    if v == 0x474C4D5F4F535F32 {
        write("surprised: the zero page holds the GLM OS magic?!\n");
    }
    write("unreachable: survived the null dereference?!\n");
    exit(1);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
