//! writer — v1.8: a deterministic STDOUT producer for redirect tests.
//!
//! Writes its arguments joined with spaces, then a fixed second line,
//! to STDOUT — which may be a pipe, a file (`> OUT.TXT`) or the console:
//!
//!   run WRITER.ELF alpha beta > OUT.TXT   (shell redirect to disk file)
//!   run WRITER.ELF | run READER.ELF       (pipe)

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
    write("writer line two: 3 + 4 = 7\n");
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
