//! hello — the first GLM OS userland program.
//!
//! Loaded by the kernel from the FAT32 ramdisk, mapped into a fresh
//! address space (own PML4, kernel half shared), entered in ring 3.

#![no_std]
#![no_main]

use glm_user::{exit, fmt_u64, getpid, uptime_ms, write};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("hello from ring 3! this is HELLO.ELF,\n");
    write("  a static ELF64 program loaded by the kernel\n");
    write("  from the FAT32 ramdisk into its own address space.\n");
    write("  syscalls in use: write(0), uptime(3), getpid(4), exit(2)\n");

    let ms = uptime_ms();
    let mut buf = [0u8; 20];
    write("  uptime at launch: ");
    write(core::str::from_utf8(fmt_u64(ms, &mut buf)).unwrap_or("?"));
    write(" ms | pid ");
    write(core::str::from_utf8(fmt_u64(getpid(), &mut buf)).unwrap_or("?"));
    write("\n");

    write("goodbye from userland - exiting with code 0\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
