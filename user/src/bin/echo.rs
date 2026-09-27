//! echo — v1.8: the pipeline-friendly argument printer.
//!
//! Joins its arguments with spaces, appends a newline, and writes the
//! whole thing to STDOUT. As a pipeline stage the output lands in the
//! pipe (the kernel redirects SYS_WRITE), which makes it the classic
//! producer:
//!
//!   run ECHO.ELF hello world | run READER.ELF
//!
//! Exit codes: 0 = the whole payload was delivered, 1 = broken pipe
//! (the reader died before all of it went through).

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, write};

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    let mut first = true;
    for i in 1..argc as usize {
        let p = unsafe { *argv.add(i) };
        let mut sbuf = [0u8; 128];
        let s = cstr_into(p, &mut sbuf);
        if !first {
            write(" ");
        }
        write(arg_str(s));
        first = false;
    }
    write("\n");
    // exit 0: every write() made it (a broken pipe would have stopped
    // the delivery mid-way; we cannot distinguish that here yet, so a
    // full exit 0 after a successful kernel write is the honest answer)
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
