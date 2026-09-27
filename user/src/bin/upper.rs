//! upper — v1.8: ASCII upper-case filter, the classic pipeline stage.
//! Pure: no headers, no trailers — stdin to stdout, a-z mapped to A-Z.
//! Exit code = the number of bytes forwarded.
//!
//!   run ECHO.ELF hello pipe world | run UPPER.ELF | run READER.ELF
//!     -> HELLO PIPE WORLD

#![no_std]
#![no_main]

use glm_user::{exit, stdin_read, write_bytes};

#[no_mangle]
pub extern "C" fn _start(_argc: i64, _argv: *const *const u8) -> ! {
    let mut buf = [0u8; 256];
    let mut total: i64 = 0;
    loop {
        let n = stdin_read(&mut buf);
        if n < 0 {
            exit(-1);
        }
        if n == 0 {
            break; // EOF
        }
        for b in buf[..n as usize].iter_mut() {
            if (b'a'..=b'z').contains(b) {
                *b &= !0x20;
            }
        }
        if write_bytes(&buf[..n as usize]) < 0 {
            exit(-2);
        }
        total += n;
    }
    exit(total & 0xFF);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
