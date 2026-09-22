//! System call interface for ring 3: `int 0x80`.
//!
//! ABI (classic Linux-flavoured):
//!   rax = syscall number, rdi/rsi/rdx = arguments, return value in rax.
//! The IDT gate for vector 0x80 has DPL=3; the asm stub pushes the full
//! GP register set, so everything except rax is preserved for the caller.
//!
//! v0.3 adds yield/sleep/wait — kernel tasks (shell, kstat, kidle) call
//! the same numbers from ring 0 via sched::ksyscall.

use crate::console;
use crate::cpu::idt::Regs;
use crate::cpu::keyboard;
use crate::cpu::pit;
use crate::mem::paging::{cr3, phys_to_virt};
use crate::mem::vmm::{AddressSpace, PAGE};
use crate::sched;

pub const SYS_WRITE: u64 = 0;
pub const SYS_READCHAR: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_UPTIME: u64 = 3;
pub const SYS_GETPID: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_SLEEP: u64 = 6;
pub const SYS_WAIT: u64 = 7;
// --- v0.5: signals + ipc channels -------------------------------------------
pub const SYS_KILL: u64 = 8;
pub const SYS_SIGACTION: u64 = 9;
pub const SYS_SIGRETURN: u64 = 10;
pub const SYS_CHAN_OPEN: u64 = 11;
pub const SYS_CHAN_SEND: u64 = 12;
pub const SYS_CHAN_RECV: u64 = 13;
// --- v0.6: copy-on-write fork -------------------------------------------------
pub const SYS_FORK: u64 = 14;
// --- v0.7: threads -------------------------------------------------------------
pub const SYS_CLONE: u64 = 15;
pub const SYS_TEXIT: u64 = 16;
pub const SYS_JOIN: u64 = 17;
pub const SYS_SET_FS: u64 = 18;
// --- v0.9: userland UDP sockets --------------------------------------------------
pub const SYS_NET_BIND: u64 = 19;
pub const SYS_NET_SENDTO: u64 = 20;
pub const SYS_NET_RECVFROM: u64 = 21;
pub const SYS_NET_CLOSE: u64 = 22;
pub const SYS_NET_INFO: u64 = 23;

const MAX_WRITE: usize = 8192;

pub fn dispatch(regs: &mut Regs) {
    match regs.rax {
        SYS_WRITE => {
            regs.rax = sys_write(regs.rdi, regs.rsi as usize);
        }
        SYS_READCHAR => {
            if let Some(c) = keyboard::pop() {
                regs.rax = c as i64 as u64;
            } else {
                // no key yet: park the task; the LAPIC timer delivers the
                // keystroke into its saved frame once one arrives
                regs.rax = (-1i64) as u64;
                sched::sys_block_on_input();
            }
        }
        SYS_EXIT => {
            // never returns to the caller as a running task
            sched::exit_current(regs.rdi as i64);
        }
        SYS_UPTIME => {
            regs.rax = pit::uptime_ms();
        }
        SYS_GETPID => {
            regs.rax = sched::current_pid();
        }
        SYS_YIELD => {
            sched::sys_yield_now();
        }
        SYS_SLEEP => {
            sched::sys_sleep(regs.rdi);
            regs.rax = 0;
        }
        SYS_WAIT => {
            sched::sys_wait(regs, regs.rdi);
        }
        SYS_KILL => {
            // signals from userland: same path the shell uses
            regs.rax = match sched::send_signal(regs.rdi, regs.rsi) {
                Ok(_) => 0,
                Err(_) => (-1i64) as u64,
            };
        }
        SYS_SIGACTION => {
            regs.rax = sched::sig_set_handler_current(regs.rdi, regs.rsi) as u64;
        }
        SYS_SIGRETURN => {
            // restores rax from the saved context itself; only failures
            // overwrite it with -1
            if super::signal::sys_sigreturn(regs) < 0 {
                regs.rax = (-1i64) as u64;
            }
        }
        SYS_CHAN_OPEN => {
            regs.rax = crate::ipc::open(regs.rdi) as u64;
        }
        SYS_CHAN_SEND => {
            regs.rax = crate::ipc::send(regs.rdi, regs.rsi, regs.rdx) as u64;
        }
        SYS_CHAN_RECV => {
            regs.rax = crate::ipc::recv(regs.rdi, regs.rsi, regs.rdx) as u64;
        }
        SYS_FORK => {
            // copy-on-write fork: parent gets the child pid, the child a
            // private frame copy with rax = 0 (set inside sys_fork)
            regs.rax = sched::sys_fork(regs) as u64;
        }
        SYS_CLONE => {
            // v0.7: spawn a thread into the SAME address space; the child
            // starts at regs.rdi with rsp = regs.rsi and rdi = regs.rdx
            regs.rax = sched::sys_clone(regs) as u64;
        }
        SYS_TEXIT => {
            // v0.7: pthread_exit — only the calling thread dies (the main
            // thread is redirected to a full process exit inside)
            sched::sys_texit_current(regs.rdi as i64);
        }
        SYS_JOIN => {
            // v0.7: wait for a sibling thread; returns its exit code
            sched::sys_join(regs, regs.rdi);
        }
        SYS_SET_FS => {
            // v0.7: install a user TLS base (FS segment)
            regs.rax = sched::sys_set_fs_current(regs.rdi) as u64;
        }
        SYS_NET_BIND => {
            // v0.9: bind a UDP socket to a port; returns the socket id
            regs.rax = crate::net::sock::bind(regs.rdi as u16) as u64;
        }
        SYS_NET_SENDTO => {
            // v0.9: sendto(id, dst_ip, dst_port, buf, len)
            regs.rax = crate::net::sock::sendto(
                regs.rdi, regs.rsi as u32, regs.rdx as u16, regs.rcx, regs.r8,
            ) as u64;
        }
        SYS_NET_RECVFROM => {
            // v0.9: recvfrom(id, buf, len, src_out); parks the task when the
            // queue is empty (-2 = would block, userland retries transparently)
            regs.rax = crate::net::sock::recvfrom(regs.rdi, regs.rsi, regs.rdx, regs.rcx) as u64;
        }
        SYS_NET_CLOSE => {
            regs.rax = crate::net::sock::close(regs.rdi) as u64;
        }
        SYS_NET_INFO => {
            // v0.9: net info (0 = our IPv4 address)
            regs.rax = match regs.rdi {
                0 => crate::net::OUR_IP as u64,
                1 => crate::net::GW_IP as u64,
                _ => (-1i64) as u64,
            };
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
    // the interrupt came from ring 3, so CR3 is the caller task's table set
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
