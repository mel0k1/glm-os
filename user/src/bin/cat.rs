//! cat — v1.8: concatenate to stdout, from a disk file or from STDIN.
//! Pure dump, no chatter. Exit code = the number of bytes written
//! (negative on errors), so the kernel exit klog verifies the payload.
//!
//!   run CAT.ELF NOTES.TXT      -- dump a file from the persistent disk
//!   run WRITER.ELF | run CAT   -- no args: copy stdin to stdout (EOF ends it)

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, file_open, file_read, stdin_read, write_bytes};

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    let mut total: i64 = 0;
    // no argument: byte-copy stdin to stdout until EOF
    if argc < 2 {
        let mut buf = [0u8; 256];
        loop {
            let n = stdin_read(&mut buf);
            if n < 0 {
                exit(-1);
            }
            if n == 0 {
                break;
            }
            if write_bytes(&buf[..n as usize]) < 0 {
                exit(-2);
            }
            total += n;
        }
        exit(total & 0xFF);
    }

    // one argument: dump that file from the disk root
    let p = unsafe { *argv.add(1) };
    let mut sbuf = [0u8; 128];
    let name = arg_str(cstr_into(p, &mut sbuf));
    let fd = file_open(name, 0);
    if fd < 0 {
        exit(-3); // cannot open
    }
    let mut buf = [0u8; 512];
    loop {
        let n = file_read(fd, &mut buf);
        if n < 0 {
            exit(-4); // read error
        }
        if n == 0 {
            break;
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
