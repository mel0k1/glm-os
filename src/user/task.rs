//! User task machinery: run an ELF64 in ring 3 and come back alive.
//!
//! Control flow (no scheduler, one user task at a time):
//!
//!   glmsh> run /BIN/HELLO.ELF
//!     run_elf()                        [kernel, shell stack]
//!       enter_user(entry, rsp, cr3)    [asm: save callee regs, switch to the
//!        |      task kernel stack, build an iretq frame, load CR3, iretq]
//!        `--> ring 3 program runs, talks via int 0x80
//!              - sys_exit / fault / any ring-3 exception
//!       user_exit_to_kernel(code)      [asm: back to kernel CR3, restore the
//!        |      saved shell stack + callee regs, return code in rax]
//!     <== exit code
//!     destroy address space, drain keyboard, print status
//!
//! The trick: `enter_user` never returns through its own frame; the exit
//! path lands exactly on the return address `call enter_user` pushed, with
//! all callee-saved registers restored. From the compiler's point of view
//! enter_user was just a normal call that took a while.

use core::arch::global_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::console::GLM_CYAN;
use crate::console::GLM_GRAY;
use crate::console::GLM_RED;
use crate::cpu::gdt;
use crate::cpu::keyboard;
use crate::klog;
use crate::mem::frames;
use crate::mem::vmm::{self, AddressSpace, NO_EXECUTE, PRESENT, USER, USER_STACK_PAGES, USER_STACK_TOP, WRITABLE, PAGE};

use super::elf;

global_asm!(
    ".section .text",
    ".globl enter_user",
    "enter_user:",
    // rdi = entry rip, rsi = user rsp, rdx = pml4 phys, rcx = task kstack top
    "    cli",
    "    push rbp",
    "    push rbx",
    "    push r12",
    "    push r13",
    "    push r14",
    "    push r15",
    "    mov qword ptr [rip + {saved}], rsp",
    "    mov rsp, rcx",
    "    xor ebp, ebp",
    "    push {udata}",   // ss   (DPL3)
    "    push rsi",       // user rsp
    "    push 0x202",     // rflags: IF=1
    "    push {ucode}",   // cs   (DPL3)
    "    push rdi",       // rip  -> ring 3
    "    mov cr3, rdx",
    "    iretq",
    saved = sym USER_EXIT_RSP,
    udata = const gdt::USER_DATA_RPL3,
    ucode = const gdt::USER_CODE_RPL3,
);

global_asm!(
    ".globl user_exit_to_kernel",
    "user_exit_to_kernel:",
    // edi = exit code (i64, negative = killed by signal-ish values)
    "    mov rax, [rip + {kcr3}]",
    "    mov cr3, rax",
    "    mov rsp, [rip + {saved}]",
    "    pop r15",
    "    pop r14",
    "    pop r13",
    "    pop r12",
    "    pop rbx",
    "    pop rbp",
    "    mov rax, rdi",
    "    sti",
    "    ret",
    kcr3 = sym KERNEL_CR3,
    saved = sym USER_EXIT_RSP,
);

/// Saved shell stack pointer while a user task is running.
static USER_EXIT_RSP: AtomicU64 = AtomicU64::new(0);
/// The kernel (bootloader) CR3, saved once at boot.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

extern "C" {
    /// Defined in the global_asm above. "Returns" only via the exit path
    /// (longjmp-style), yielding the task's exit code.
    fn enter_user(entry: u64, user_rsp: u64, pml4: u64, kstack_top: u64) -> i64;
    fn user_exit_to_kernel(code: i64) -> !;
}

pub fn init() {
    KERNEL_CR3.store(vmm::kernel_cr3(), Ordering::Relaxed);
}

/// Exit code used when the kernel kills a misbehaving task.
const KILL_EXIT_CODE: i64 = 139; // 128 + SIGSEGV, Linux-style

/// Called from the IDT dispatcher when ring-3 code raises an exception.
/// Never returns to the faulting context.
pub fn fault_and_terminate(kind: &str, detail: core::fmt::Arguments) -> ! {
    klog!("user task killed: {} ({})", kind, detail);
    crate::console::newline();
    crate::console::print_color("  [ ", crate::console::GLM_GRAY);
    crate::console::print_color("kill", GLM_RED);
    crate::console::print_color(" ] ", crate::console::GLM_GRAY);
    crate::console::print_args(format_args!("task terminated: {} ({})\n", kind, detail));
    unsafe { user_exit_to_kernel(KILL_EXIT_CODE) }
}

/// Load `path` from the ramdisk, run it in ring 3, return its exit code.
pub fn run_elf(path: &str) -> Result<i64, &'static str> {
    keyboard::drain();

    let bytes = elf::read_from_ramdisk(path)?;
    let kb = bytes.len() / 1024;
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print_args(format_args!("loading {} ({} bytes) from ramdisk\n", path, bytes.len()));

    let space = AddressSpace::new_user().ok_or("cannot allocate a PML4")?;
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print_args(format_args!(
        "address space: pml4 {:#x} (kernel half shared, user half fresh)\n",
        space.pml4
    ));

    let loaded = elf::load(&space, &bytes)?;
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print_args(format_args!(
        "ELF64: {} PT_LOAD segments, {} KiB mapped, entry {:#x}\n",
        loaded.seg_count, loaded.mapped_bytes / 1024, loaded.entry
    ));

    // user stack: high pages of the user half
    for i in 0..USER_STACK_PAGES {
        let page = USER_STACK_TOP - (i + 1) * PAGE;
        let frame = frames::alloc().ok_or("out of frames for user stack")?;
        space
            .map(page, frame, PRESENT | WRITABLE | USER | NO_EXECUTE)
            .map_err(|e| -> &'static str { e })?;
    }

    klog!(
        "user: entering ring 3: entry={:#x} rsp={:#x} cr3={:#x} kstack={:#x}",
        loaded.entry,
        USER_STACK_TOP,
        space.pml4,
        gdt::task_kstack_top()
    );
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print_args(format_args!(
        "dropping to ring 3 (cs={:#x} ss={:#x}, if=1)...\n",
        gdt::USER_CODE_RPL3,
        gdt::USER_DATA_RPL3
    ));

    // --- the crossing --------------------------------------------------------
    let code = unsafe { enter_user(loaded.entry, USER_STACK_TOP, space.pml4, gdt::task_kstack_top()) };
    // ...and we are back: kernel CR3 restored, shell stack intact.
    let reclaimed = space.destroy();
    keyboard::drain();

    klog!("user: task exited with code {}", code);
    crate::console::print_color("  [ ", GLM_GRAY);
    crate::console::print_color("run ", GLM_CYAN);
    crate::console::print_color(" ] ", GLM_GRAY);
    crate::console::print(&alloc::format!(
        "address space destroyed, {} frames reclaimed\n",
        reclaimed
    ));
    Ok(code)
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
