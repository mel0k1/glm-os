//! Userland subsystem (v0.2): ELF64 loader, ring-3 tasks, int 0x80 syscalls.

pub mod elf;
pub mod syscall;
pub mod task;

pub fn init() {
    task::init();
}
