//! counter — v1.6 proof that a RING-3 program owns the persistent disk.
//!
//! Reads its boot count from /COUNTER.DAT via the file syscalls (36-42),
//! prints "boot #N", writes N+1 back, and EXITS WITH CODE N — so the
//! serial log ("exited with code N") is the machine-readable proof.
//!
//! Boot 1: file missing -> boot #1. Boot 2 on the same disk: reads "1"
//! -> boot #2. The state never touches the kernel except through
//! open/read/write/seek/close on the FAT32 disk.

#![no_std]
#![no_main]

use glm_user::{file_close, file_open, file_read, file_seek, file_write, exit, fmt_u64, write, O_CREATE, O_RDWR};

const NAME: &str = "COUNTER.DAT";
const CAP: u64 = 90; // keep exit codes sane if someone boots a hundred times

fn print_num(v: u64) {
    let mut b = [0u8; 20];
    write(core::str::from_utf8(fmt_u64(v, &mut b)).unwrap_or("?"));
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("COUNTER: ring-3 file I/O over int 0x80 (v1.6)\n");

    // 1. read the old count, if any (read-only open)
    let mut count: u64 = 0;
    let mut had_file = true;
    let fd = file_open(NAME, 0);
    if fd < 0 {
        had_file = false;
        write("  open /COUNTER.DAT: miss -> first boot this disk has seen\n");
    } else {
        let mut buf = [0u8; 16];
        let n = file_read(fd, &mut buf);
        if n > 0 {
            for &b in &buf[..n as usize] {
                if b.is_ascii_digit() {
                    count = count.saturating_mul(10).saturating_add((b - b'0') as u64);
                }
            }
        }
        file_close(fd);
        write("  open /COUNTER.DAT: hit, read ");
        print_num(count);
        write("\n");
    }

    let boot = count + 1;
    write("  boot #");
    print_num(boot);
    write(" (this number lives in COUNTER.DAT on the FAT32 disk)\n");

    // 2. write the new count back (create-or-patch, then flush on close)
    let fd2 = file_open(NAME, O_RDWR | O_CREATE);
    if fd2 < 0 {
        write("  no persistent disk mounted - count not saved\n");
        exit(boot.min(CAP) as i64);
    }
    let mut nb = [0u8; 20];
    let digits = fmt_u64(boot, &mut nb);
    file_seek(fd2, 0, 0); // SET
    file_write(fd2, &digits);
    file_write(fd2, b"\n");
    file_close(fd2);
    write("  wrote ");
    print_num(digits.len() as u64);
    write(" bytes -> COUNTER.DAT (flushed at close)\n");

    let _ = had_file;
    exit(boot.min(CAP) as i64);
}

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}
