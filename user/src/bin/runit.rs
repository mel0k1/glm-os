//! runit — v1.7 capstone: the Unix process model composed in ring 3.
//!
//!   child = fork();
//!   if child == 0: exec(path, argv)      // never returns on success
//!   code = wait(child); exit(code)       // parent propagates the code
//!
//! Until now only the kernel could start programs. With SYS_EXEC (43) a
//! ring-3 process can become another program and a parent can launch,
//! wait for it and read its exit code — no kernel shell involvement.
//!
//! Exit codes (machine-readable proof in the serial log):
//!   * exit = child's exit code (propagated) — the normal path;
//!   * 127 — the child's exec failed (classic shell "command not found");
//!   * 126 — unreachable (exec succeeded but somehow returned);
//!   * 125 — usage error (no program path given).

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exec, exit, fmt_u64, fork, wait, write};

const USAGE: &str = "RUNIT: usage: RUNIT.ELF <program> [args...]\n";

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    // parse argv without alloc: copy each string into fixed buffers.
    // The two-pass dance keeps the borrow checker happy: pass 1 copies
    // bytes and records each string's LENGTH (cstr_into stops at the
    // NUL), pass 2 slices to that length — taking the whole buffer would
    // drag the zero padding into the string and poison exec's argv.
    let mut bufs = [[0u8; 128]; 16];
    let mut lens = [0usize; 16];
    let mut strs: [&str; 16] = [""; 16];
    let n = (argc as usize).min(16);
    for (i, b) in bufs.iter_mut().enumerate().take(n) {
        let p = unsafe { *argv.add(i) };
        lens[i] = cstr_into(p, b).len();
    }
    for i in 0..n {
        strs[i] = arg_str(&bufs[i][..lens[i]]);
    }

    if n < 2 {
        write(USAGE);
        exit(125);
    }

    let prog = strs[1]; // argv[0] is "RUNIT.ELF", argv[1] the target
    let mut msg = [0u8; 20];

    write("RUNIT: fork+exec+wait in ring 3 -> ");
    write(prog);
    write("\n");

    let pid = fork();
    if pid == 0 {
        // ---- child: become the requested program ------------------
        // argv for the new image: [prog, rest of our args...]
        let child_argv: [&str; 16] = {
            let mut a: [&str; 16] = [""; 16];
            let mut m = 0;
            for (k, s) in strs.iter().enumerate().skip(1).take(n - 1) {
                a[m] = s;
                m += 1;
                let _ = k;
            }
            a
        };
        let m = n - 1;
        let r = exec(prog, &child_argv[..m]);
        if r == -1 {
            write("RUNIT(child): exec failed\n");
            exit(127); // command not found, shell-style
        }
        exit(126); // exec never returns on success
    }

    // ---- parent: wait and propagate the child's exit code ----------
    write("RUNIT: child pid ");
    write(core::str::from_utf8(fmt_u64(pid as u64, &mut msg)).unwrap_or("?"));
    write(" launched, waiting for it\n");
    let code = wait(pid as u64);
    write("RUNIT: child exited with code ");
    write(core::str::from_utf8(fmt_u64(code as u64, &mut msg)).unwrap_or("?"));
    write("\n");
    exit(code);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
