//! Userland subsystem: ELF64 loader, ring-3 tasks, int 0x80 syscalls.
//! Since v0.3 user tasks are born through the scheduler (sched::spawn).
//! v0.5 adds the signal subsystem (signal.rs) and user-pointer helpers.

pub mod elf;
pub mod exec;
pub mod signal;
pub mod syscall;
pub mod task;
pub mod uaccess;

pub fn init() {
    // kept for boot-log symmetry; ELF loading happens per-spawn
}
