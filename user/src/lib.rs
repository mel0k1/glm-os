//! glm-user: freestanding syscall library for GLM OS ring-3 programs.
//!
//! ABI: `int 0x80`, number in rax, args in rdi/rsi/rdx, result in rax.
//! The kernel preserves every GP register except rax across the gate.

#![no_std]

pub const SYS_WRITE: u64 = 0;
pub const SYS_READCHAR: u64 = 1;
pub const SYS_EXIT: u64 = 2;
pub const SYS_UPTIME: u64 = 3;
pub const SYS_GETPID: u64 = 4;
pub const SYS_YIELD: u64 = 5;
pub const SYS_SLEEP: u64 = 6;
pub const SYS_WAIT: u64 = 7;

/// Give the CPU back to the scheduler (SYS_YIELD).
pub fn yield_now() {
    syscall0(SYS_YIELD);
}

/// Sleep for `ms` milliseconds (SYS_SLEEP) — other tasks run meanwhile.
pub fn sleep_ms(ms: u64) {
    syscall1(SYS_SLEEP, ms);
}

#[inline]
pub fn syscall0(n: u64) -> u64 {
    let ret: u64;
    unsafe { core::arch::asm!("int 0x80", inlateout("rax") n => ret, options(nostack)) };
    ret
}

#[inline]
pub fn syscall1(n: u64, a1: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            options(nostack)
        )
    };
    ret
}

#[inline]
pub fn syscall2(n: u64, a1: u64, a2: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") n => ret,
            in("rdi") a1,
            in("rsi") a2,
            options(nostack)
        )
    };
    ret
}

/// Print a string on the kernel console (SYS_WRITE).
pub fn write(s: &str) {
    syscall2(SYS_WRITE, s.as_ptr() as u64, s.len() as u64);
}

/// Terminate the task (SYS_EXIT). Never returns.
pub fn exit(code: i64) -> ! {
    syscall1(SYS_EXIT, code as u64);
    loop {
        core::hint::spin_loop();
    }
}

/// Milliseconds since boot (SYS_UPTIME).
pub fn uptime_ms() -> u64 {
    syscall0(SYS_UPTIME)
}

/// Current task id (SYS_GETPID).
pub fn getpid() -> u64 {
    syscall0(SYS_GETPID)
}

/// Format an unsigned integer as decimal ASCII into `buf`,
/// returning the digits slice.
pub fn fmt_u64(mut v: u64, buf: &mut [u8; 20]) -> &[u8] {
    if v == 0 {
        buf[0] = b'0';
        return &buf[..1];
    }
    let mut i = 20;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    &buf[i..]
}
