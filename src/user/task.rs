//! User task machinery (v0.3): spawn ELF64 programs as full-blown tasks.
//!
//! v0.2 ran one user program at a time via a longjmp-style crossing
//! (enter_user / user_exit_to_kernel). v0.3 replaces that with the
//! scheduler: spawning builds an address space + a bootstrap trap frame,
//! and the task enters ring 3 whenever the scheduler hands it the CPU.
//! The shell can `run` (wait for the child) or `spawn` (leave it running
//! in the background) — real multitasking.
//!
//! v1.7: every user task now starts with a Unix-style argv — the entry
//! frame carries (rdi=argc, rsi=argv) and the strings live on the user
//! stack. Old programs are unaffected (they ignore the registers); new
//! programs declare `_start(argc, argv)`. The same builder serves both
//! spawn (fresh task) and exec (SYS_EXEC image replacement).

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

/// v1.7: argv limits shared with src/user/exec.rs and user/src/lib.rs.
pub mod spawn_limits {
    pub const MAX_ARGC: usize = 16;
    pub const MAX_ARG: usize = 128;
    pub const MAX_ARGV_BYTES: usize = 2048; // strings total (with NULs)
}

/// A fully built ring-3 image, ready to become a task (spawn) or to
/// replace the current one (exec).
pub struct UserImage {
    pub space: AddressSpace,
    pub entry: u64,
    /// Initial RSP: points at the argc slot, argv array right above.
    pub rsp: u64,
    /// argc including argv[0] (>= 1).
    pub argc: u64,
    /// User VA of argv[0]; argv[argc] is a NULL sentinel.
    pub argv: u64,
}

/// Load `path` from the ramdisk and spawn it as a ring-3 task.
/// Returns the new task's pid. Console feedback is suppressed while the
/// GUI owns the screen UNLESS the caller prints into a terminal window
/// (v1.4: `run` from a terminal session reports inside the window).
pub fn spawn_user_elf(path: &str, args: &[&str]) -> Result<u64, &'static str> {
    let bytes = elf::read_from_ramdisk(path)?;
    spawn_user_elf_bytes(&bytes, path, args)
}

/// v1.5: spawn an in-memory ELF image. `drun` uses this to execute
/// programs loaded from the persistent AHCI disk instead of the ramdisk.
pub fn spawn_user_elf_bytes(bytes: &[u8], name: &str, args: &[&str]) -> Result<u64, &'static str> {
    let redirected = crate::sched::current_out_win() != 0;
    let quiet = crate::console::GUI_ACTIVE.load(core::sync::atomic::Ordering::Relaxed)
        && !redirected;
    let pid = spawn_user_image(bytes, name, args)?;
    if !quiet {
        crate::console::print_color("  [ ", GLM_GRAY);
        crate::console::print_color("run ", GLM_CYAN);
        crate::console::print_color(" ] ", GLM_GRAY);
        crate::console::print_args(format_args!(
            "spawned pid {} ({}, {} bytes, {} args) - scheduler will run it\n",
            pid,
            name,
            bytes.len(),
            args.len()
        ));
    }
    Ok(pid)
}

/// Build a user address space around an ELF image and register a task.
fn spawn_user_image(image: &[u8], name: &str, args: &[&str]) -> Result<u64, &'static str> {
    // task name: last path component, uppercased (FAT32 style)
    let short = name.rsplit('/').next().unwrap_or(name);

    let img = build_user_image(image, short, args)?;
    let pml4 = img.space.pml4;
    let entry = img.entry;
    let rsp = img.rsp;
    let argc = img.argc;
    let argv = img.argv;
    let space = img.space;

    let pid = sched::spawn(sched::NewTask {
        name: short,
        entry,
        user_rsp: Some(rsp),
        pml4,
        is_user: true,
        user_space: Some(alloc::sync::Arc::new(space)),
        pinned_cpu: sched::CPU_ANY,
        entry_regs: (argc, argv),
    })
    .ok_or("task table full")?;

    klog!(
        "user: pid {} ready: entry={:#x} rsp={:#x} cr3={:#x} argc={}",
        pid,
        entry,
        rsp,
        pml4,
        argc
    );
    Ok(pid)
}

/// v1.7: build a complete user image — ELF segments, stack, argv block,
/// sigreturn trampoline. Used by BOTH spawn paths and by exec, so every
/// ring-3 entry has the same shape.
pub fn build_user_image(image: &[u8], name: &str, args: &[&str]) -> Result<UserImage, &'static str> {
    let space = AddressSpace::new_user().ok_or("cannot allocate a PML4")?;
    let loaded = elf::load(&space, image)?;

    // user stack: high pages of the user half. Fresh frames are zeroed so
    // the argv region never inherits recycled-frame junk (v1.7).
    for i in 0..USER_STACK_PAGES {
        let page = USER_STACK_TOP - (i + 1) * PAGE;
        let frame = frames::alloc().ok_or("out of frames for user stack")?;
        space
            .map(page, frame, PRESENT | WRITABLE | USER | NO_EXECUTE)
            .map_err(|e| -> &'static str { e })?;
        unsafe {
            core::ptr::write_bytes(
                crate::mem::paging::phys_to_virt(frame) as *mut u8,
                0,
                PAGE as usize,
            );
        }
    }

    // sigreturn trampoline page (signal handlers `ret` into it)
    super::signal::map_trampoline(&space)?;

    // argv: argv[0] is the program name unless the caller supplied one
    // v1.7: the argv block lives one page below the stack top, so the
    // topmost frame stays pure scratch — nothing precious sits at the tail
    // of the highest frame
    let (rsp, argv, argc) = write_argv(&space, name, args, USER_STACK_TOP - PAGE)?;

    Ok(UserImage {
        space,
        entry: loaded.entry,
        rsp,
        argc,
        argv,
    })
}

/// Write a classic argv block onto the (freshly mapped, zeroed) user stack.
///
/// `stack_base` is where the block TOP starts (one page below USER_STACK_TOP
/// so the highest frame stays scratch). Layout downwards:
///   [argv strings, each NUL-terminated]
///   (align 8)
///   [NULL sentinel]                <- end of the argv array
///   [argv[n-1]] ... [argv[0]]      <- pointers, argv[0] lowest
///   [argc]                         <- initial RSP points here
/// Entry state: rdi = argc, rsi = &argv[0] (= rsp + 8).
fn write_argv(
    space: &AddressSpace,
    name: &str,
    args: &[&str],
    stack_base: u64,
) -> Result<(u64, u64, u64), &'static str> {
    // argv[0] defaults to the program name; shell/exec pass explicit
    // argv[0]s already, this covers direct kernel spawns
    let mut all: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(args.len() + 1);
    if args.is_empty() {
        all.push(name);
    } else {
        all.extend_from_slice(args);
    }
    if all.len() > spawn_limits::MAX_ARGC {
        return Err("too many arguments");
    }
    let total: usize = all.iter().map(|a| a.len() + 1).sum();
    if total > spawn_limits::MAX_ARGV_BYTES {
        return Err("argument list too long");
    }

    let n = all.len();
    let mut sp = stack_base;

    // strings, last to first (order on the stack does not matter, only
    // that each pointer lands inside a mapped page)
    let mut addrs = [0u64; spawn_limits::MAX_ARGC];
    for (i, a) in all.iter().enumerate().rev() {
        let mut buf = [0u8; spawn_limits::MAX_ARG + 1];
        buf[..a.len()].copy_from_slice(a.as_bytes());
        buf[a.len()] = 0; // NUL terminator
        sp -= a.len() as u64 + 1;
        super::uaccess::write_user_bytes(space, sp, &buf[..a.len() + 1])
            .map_err(|_| -> &'static str { "argv string did not land in mapped stack" })?;
        addrs[i] = sp;
    }

    // align the pointer array
    sp &= !0x7;
    // NULL sentinel above argv[n-1]
    sp -= 8;
    write_u64(space, sp, 0)?;
    // the array itself
    sp -= 8 * n as u64;
    let argv0 = sp;
    for (i, &a) in addrs.iter().enumerate().take(n) {
        write_u64(space, argv0 + 8 * i as u64, a)?;
    }
    // argc at the bottom
    sp -= 8;
    write_u64(space, sp, n as u64)?;

    Ok((sp, argv0, n as u64))
}

fn write_u64(space: &AddressSpace, va: u64, v: u64) -> Result<(), &'static str> {
    super::uaccess::write_user_bytes(space, va, &v.to_le_bytes())
        .map_err(|_| -> &'static str { "argv block did not land in mapped stack" })
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
