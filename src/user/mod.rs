//! Userland subsystem: ELF64 loader, ring-3 tasks, int 0x80 syscalls.
//! Since v0.3 user tasks are born through the scheduler (sched::spawn).

pub mod elf;
pub mod syscall;
pub mod task;

pub fn init() {
    // kept for boot-log symmetry; ELF loading happens per-spawn
}
