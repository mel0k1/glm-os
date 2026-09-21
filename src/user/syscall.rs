//! System call interface for ring 3: `int 0x80`.
//!
//! ABI (classic Linux-flavoured):
//!   rax = syscall number, rdi/rsi/rdx = arguments, return value in rax.
//! The IDT gate for vector 0x80 has DPL=3; the asm stub pushes the full
//! GP register set, so everything except rax is preserved for the caller.

use crate::console;
use crate::cpu::idt::Regs;
use crate::cpu::keyboard;
use crate::cpu::pit;
use crate::mem::paging::{cr3, phys_to_virt};
use crate::mem::vmm::{AddressSpace, PAGE};

pub const SYS_WRITE: u64 = 0;
pub const SYS_READCHAR: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_UPTIME: u64 = 3;
pub const SYS_GETPID: u64 = 4;

const MAX_WRITE: usize = 8192;

extern "C" {
    fn user_exit_to_kernel(code: i64) -> !;
}

pub fn dispatch(regs: &mut Regs) {
    match regs.rax {
        SYS_WRITE => {
            regs.rax = sys_write(regs.rdi, regs.rsi as usize);
        }
        SYS_READCHAR => {
            regs.rax = sys_readchar() as u64;
        }
        SYS_EXIT => {
            // never returns to the user task
            unsafe { user_exit_to_kernel(regs.rdi as i64) }
        }
        SYS_UPTIME => {
            regs.rax = pit::uptime_ms();
        }
        SYS_GETPID => {
            regs.rax = 1; // one user task at a time in v0.2
        }
        _ => {
            regs.rax = (-1i64) as u64; // ENOSYS
        }
    }
}

/// write(buf, len): copy user bytes into the kernel via page-table
/// translation + HHDM, then print them on the framebuffer console.
/// Returns the number of bytes actually printed.
fn sys_write(uptr: u64, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let len = len.min(MAX_WRITE);
    // the interrupt came from ring 3, so CR3 is the user task's table set
    let space = AddressSpace::from_pml4(cr3());

    let mut chunk = [0u8; 128];
    let mut done = 0usize;
    while done < len {
        let va = uptr + done as u64;
        let page_left = (PAGE - (va & (PAGE - 1))) as usize;
        let n = (len - done).min(page_left).min(chunk.len());
        let Some(phys) = space.translate(va) else {
            break; // bad buffer: stop, report what we got
        };
        unsafe {
            core::ptr::copy(phys_to_virt(phys) as *const u8, chunk.as_mut_ptr(), n);
        }
        // sanitize to printable ASCII for the framebuffer console
        let mut line = alloc::string::String::new();
        for &b in &chunk[..n] {
            line.push(if b.is_ascii_graphic() || b == b' ' || b == b'\n' {
                b as char
            } else if b == b'\t' {
                ' '
            } else {
                '\u{B7}' // middle dot for invisible bytes
            });
        }
        console::print(&line);
        done += n;
    }
    done as u64
}

/// readchar(): blocking single-character input from the PS/2 keyboard.
fn sys_readchar() -> i64 {
    loop {
        if let Some(c) = keyboard::pop() {
            return c as i64;
        }
        // entered via an interrupt gate (IF=0), so re-enable to not starve
        // the keyboard IRQ while we wait
        unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
    }
}
