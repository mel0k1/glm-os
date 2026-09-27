//! reader — v1.8: the pipeline consumer. Reads STDIN until EOF and
//! forwards every byte to STDOUT unchanged. Exit code = the number of
//! bytes read (machine-checkable via the kernel exit klog); -1 on a
//! stdin error.
//!
//!   run ECHO.ELF hi | run READER.ELF     -> prints "hi", exits 3
//!   run READER.ELF < FILE.TXT            -> dumps the file, exits size

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
            exit(-1); // stdin error (broken source)
        }
        if n == 0 {
            break; // EOF: drained, no writers left (or end of file)
        }
        if write_bytes(&buf[..n as usize]) < 0 {
            exit(-2); // our own stdout broke
        }
        total += n;
    }
    exit(total & 0xFF);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
