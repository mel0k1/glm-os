//! User task machinery (v0.3): spawn ELF64 programs as full-blown tasks.
//!
//! v0.2 ran one user program at a time via a longjmp-style crossing
//! (enter_user / user_exit_to_kernel). v0.3 replaces that with the
//! scheduler: spawning builds an address space + a bootstrap trap frame,
//! and the task enters ring 3 whenever the scheduler hands it the CPU.
//! The shell can `run` (wait for the child) or `spawn` (leave it running
//! in the background) — real multitasking.

use crate::console::{GLM_CYAN, GLM_GRAY};
use crate::klog;
use crate::mem::frames;
use crate::mem::vmm::{
    AddressSpace, NO_EXECUTE, PRESENT, USER, USER_STACK_PAGES, USER_STACK_TOP, WRITABLE, PAGE,
};
use crate::sched;

use super::elf;

/// Exit code used when the kernel kills a misbehaving task.
pub const KILL_EXIT_CODE: i64 = 139; // 128 + SIGSEGV, Linux-style

/// Load `path` from the ramdisk and spawn it as a ring-3 task.
/// Returns the new task's pid.
pub fn spawn_user_elf(path: &str) -> Result<u64, &'static str> {
    let bytes = elf::read_from_ramdisk(path)?;
    let pid = spawn_user_image(&bytes, path)?;
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print_args(format_args!(
        "spawned pid {} ({}, {} bytes) - scheduler will run it\n",
        pid, path, bytes.len()
    ));
    Ok(pid)
}

/// Build a user address space around an ELF image and register a task.
fn spawn_user_image(image: &[u8], name: &str) -> Result<u64, &'static str> {
    let space = AddressSpace::new_user().ok_or("cannot allocate a PML4")?;
    let loaded = elf::load(&space, image)?;

    // user stack: high pages of the user half
    for i in 0..USER_STACK_PAGES {
        let page = USER_STACK_TOP - (i + 1) * PAGE;
        let frame = frames::alloc().ok_or("out of frames for user stack")?;
        space
            .map(page, frame, PRESENT | WRITABLE | USER | NO_EXECUTE)
            .map_err(|e| -> &'static str { e })?;
    }

    // sigreturn trampoline page (signal handlers `ret` into it)
    super::signal::map_trampoline(&space)?;

    // task name: last path component, uppercased (FAT32 style)
    let short = name.rsplit('/').next().unwrap_or(name);

    let pml4 = space.pml4;
    let entry = loaded.entry;
    let pid = sched::spawn(sched::NewTask {
        name: short,
        entry,
        user_rsp: Some(USER_STACK_TOP),
        pml4,
        is_user: true,
        user_space: Some(alloc::sync::Arc::new(space)),
        pinned_cpu: sched::CPU_ANY,
    })
    .ok_or("task table full")?;

    klog!(
        "user: pid {} ready: entry={:#x} rsp={:#x} cr3={:#x}",
        pid,
        entry,
        USER_STACK_TOP,
        pml4
    );
    Ok(pid)
}

/// Called from the IDT dispatcher when ring-3 code raises an exception.
/// Terminates the faulting task; the tail of common_handler then switches
/// the CPU to the next task (the faulting frame is abandoned).
pub fn fault_and_terminate(kind: &str, detail: core::fmt::Arguments) {
    klog!("user task killed: {} ({})", kind, detail);
    crate::console::newline();
    crate::console::print_color("  [ ", crate::console::GLM_GRAY);
    crate::console::print_color("kill", crate::console::GLM_RED);
    crate::console::print_color(" ] ", crate::console::GLM_GRAY);
    crate::console::print_args(format_args!(
        "task terminated: {} ({})\n",
        kind, detail
    ));
    sched::exit_current(KILL_EXIT_CODE);
    // returns into common_handler's tail, which never resumes this context
}

/// Wait until a specific (or any) child task exits; returns its exit code.
/// The shell uses this for foreground `run`. Blocks via the scheduler.
pub fn wait_for_child(pid: u64) -> i64 {
    sched::ksyscall(sched::SYS_WAIT, pid, 0, 0) as i64
}

/// Report a task's fate for the shell.
pub fn describe_exit(code: i64) -> &'static str {
    if code == 0 {
        "clean exit"
    } else if code == KILL_EXIT_CODE {
        "killed by the kernel"
    } else {
        "non-zero exit"
    }
}
