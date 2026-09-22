//! POSIX-flavoured signals for GLM OS (v0.5).
//!
//! A signal is one bit in `Task::sig_pending` plus, optionally, a
//! user-space handler installed via `sigaction`. Delivery happens at
//! RESUME time: when `sched::post_dispatch` is about to switch a task
//! back in, pending signals are inspected and either
//!
//!   * executed through "frame surgery" — a `SigFrame` is pushed onto the
//!     user stack and the saved ring-3 frame is hijacked so that `iretq`
//!     lands directly in the handler (rdi = signal number); or
//!   * turned into a default-action termination (no handler, or SIGKILL),
//!     which destroys the address space and zombifies the task — exactly
//!     what `exit_current` does, except from the scheduler's point of view.
//!
//! When a handler returns, it "returns" to the sigreturn trampoline page
//! mapped at a fixed user VA in every address space; the trampoline raises
//! `SYS_SIGRETURN`, and the kernel restores the interrupted context from
//! the `SigFrame` still sitting on the user stack. This is the classic
//! Linux restorer-page scheme, minus the vdso.

use crate::cpu::idt::Regs;
use crate::mem::vmm::{AddressSpace, PRESENT, USER};
use crate::sched::Task;

use super::uaccess::{read_user_bytes, write_user_bytes};

// --- signal numbers (Linux values for authenticity) --------------------------

pub const SIGKILL: u64 = 9; // terminate, cannot be caught
pub const SIGUSR1: u64 = 10; // default: terminate
pub const SIGUSR2: u64 = 12; // default: terminate
pub const SIGTERM: u64 = 15; // default: terminate
/// Handler slots are indexed 0..NSIG directly by signal number.
pub const NSIG: usize = 16;

/// Signals user code may install a handler for (SIGKILL is never here).
pub fn catchable(sig: u64) -> bool {
    matches!(sig, SIGUSR1 | SIGUSR2 | SIGTERM)
}

// --- the sigreturn trampoline page -------------------------------------------

/// Fixed user VA for the trampoline (user half, below the stack, above
/// the image base). Every user address space gets one page mapped here.
pub const SIGTRAMP_VA: u64 = 0x0000_7FFF_F000;

/// mov eax, 10 (SYS_SIGRETURN); int 0x80; hlt; nop — 10 bytes of machine
/// code the kernel writes once per address space. A handler's `ret` pops
/// `restorer` off the signal frame and lands here.
pub const TRAMPOLINE: [u8; 10] = [0xB8, 10, 0, 0, 0, 0xCD, 0x80, 0xF4, 0x90, 0x90];

pub fn map_trampoline(space: &AddressSpace) -> Result<(), &'static str> {
    let frame = crate::mem::frames::alloc().ok_or("signal: no frame for trampoline")?;
    // executable on purpose (no NO_EXECUTE): ring 3 runs this code
    space
        .map(SIGTRAMP_VA, frame, PRESENT | USER)
        .map_err(|e| -> &'static str { e })?;
    unsafe {
        core::ptr::copy_nonoverlapping(
            TRAMPOLINE.as_ptr(),
            crate::mem::paging::phys_to_virt(frame) as *mut u8,
            TRAMPOLINE.len(),
        );
    }
    Ok(())
}

// --- the signal frame ---------------------------------------------------------

/// Saved user context restored by SYS_SIGRETURN. Field order mirrors Regs'
/// GP section so restoration is a field-for-field copy.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UserCtx {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

/// What the kernel pushes onto the user stack before entering a handler.
/// At handler entry rsp points at `restorer`, so a plain `ret` jumps to
/// the trampoline; `signum` also arrived in rdi (SysV first argument).
#[repr(C)]
pub struct SigFrame {
    pub restorer: u64, // SIGTRAMP_VA
    pub signum: u64,
    pub magic: u64, // sanity check for sigreturn
    pub ctx: UserCtx,
}

const FRAME_MAGIC: u64 = 0x0000_5347_4652_3035; // "SGFR05"
pub const FRAME_SIZE: u64 = core::mem::size_of::<SigFrame>() as u64;

// --- delivery (called from sched under SCHED_LOCK) ----------------------------

/// Rewrite `t`'s saved ring-3 frame so that resuming it enters `handler`
/// with the signal number in rdi. On success the pending bit must be
/// cleared by the caller.
pub fn enter_handler(t: &mut Task, sig: u64, handler: u64) -> Result<(), ()> {
    let Some(space) = t.user_space.as_ref() else {
        return Err(());
    };
    if t.saved_regs == 0 {
        return Err(());
    }
    // SAFETY: saved_regs points at the task's parked Regs frame on its own
    // kernel stack; the task is not running anywhere (caller holds the
    // scheduler lock).
    let frame: &mut Regs = unsafe {
        &mut *core::ptr::with_exposed_provenance_mut::<Regs>(t.saved_regs as usize)
    };

    let old = UserCtx {
        r15: frame.r15,
        r14: frame.r14,
        r13: frame.r13,
        r12: frame.r12,
        r11: frame.r11,
        r10: frame.r10,
        r9: frame.r9,
        r8: frame.r8,
        rbp: frame.rbp,
        rdi: frame.rdi,
        rsi: frame.rsi,
        rdx: frame.rdx,
        rcx: frame.rcx,
        rbx: frame.rbx,
        rax: frame.rax,
        rip: frame.rip,
        rflags: frame.rflags,
        rsp: frame.rsp,
    };

    // place the frame below a 128-byte guard gap; keep rsp % 16 == 8 at
    // handler entry (as if the handler was `call`ed), so SysV code with
    // aligned prologues behaves
    let top = (frame.rsp.saturating_sub(128)) & !0xF;
    let frame_va = top - 8 - FRAME_SIZE;

    let sf = SigFrame {
        restorer: SIGTRAMP_VA,
        signum: sig,
        magic: FRAME_MAGIC,
        ctx: old,
    };
    let bytes =
        unsafe { core::slice::from_raw_parts(&sf as *const SigFrame as *const u8, FRAME_SIZE as usize) };
    if write_user_bytes(space, frame_va, bytes).is_err() {
        return Err(()); // stack not mapped/writable: caller terminates
    }

    frame.rip = handler;
    frame.rdi = sig;
    frame.rsi = 0;
    frame.rdx = 0;
    frame.rax = 0;
    frame.rsp = frame_va;
    t.sig_frame_va = frame_va;
    t.sig_depth += 1;
    Ok(())
}

/// SYS_SIGRETURN: called on the trampoline's int 0x80. Reads the SigFrame
/// back from the user stack and restores every register in the interrupt
/// frame, including rax. Returns 0 on success, -1 on a bogus frame.
pub fn sys_sigreturn(regs: &mut Regs) -> i64 {
    let slot = crate::sched::current_slot();
    let (frame_va, depth) = crate::sched::sig_frame_of(slot);
    if frame_va == 0 || depth == 0 {
        return -1;
    }
    // CR3 here is the caller's own table (the trampoline ran in ring 3),
    // but going through translate + HHDM keeps the rule uniform.
    let space = AddressSpace::from_pml4(crate::mem::paging::cr3());
    let mut buf = [0u8; core::mem::size_of::<SigFrame>()];
    if read_user_bytes(&space, frame_va, &mut buf).is_err() {
        return -1;
    }
    // SAFETY: buf holds a full SigFrame copied from the task's own stack.
    let sf = unsafe { core::ptr::read(buf.as_ptr() as *const SigFrame) };
    if sf.magic != FRAME_MAGIC {
        return -1;
    }
    regs.r15 = sf.ctx.r15;
    regs.r14 = sf.ctx.r14;
    regs.r13 = sf.ctx.r13;
    regs.r12 = sf.ctx.r12;
    regs.r11 = sf.ctx.r11;
    regs.r10 = sf.ctx.r10;
    regs.r9 = sf.ctx.r9;
    regs.r8 = sf.ctx.r8;
    regs.rbp = sf.ctx.rbp;
    regs.rdi = sf.ctx.rdi;
    regs.rsi = sf.ctx.rsi;
    regs.rdx = sf.ctx.rdx;
    regs.rcx = sf.ctx.rcx;
    regs.rbx = sf.ctx.rbx;
    regs.rip = sf.ctx.rip;
    regs.rflags = sf.ctx.rflags;
    regs.rsp = sf.ctx.rsp;
    regs.rax = sf.ctx.rax; // restored, NOT the syscall return path
    crate::sched::sig_frame_consumed(slot);
    0
}
