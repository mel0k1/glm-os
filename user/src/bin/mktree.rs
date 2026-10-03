//! mktree — v1.9: build and verify a directory tree from RING 3.
//!
//! Every step is a fresh directory syscall over int 0x80 (mkdir/rmdir/
//! chdir/getcwd/file_open/read/write). The exit code is the number of
//! checks passed (10 when everything works), so the klog line
//! "exited with code 10" is the whole machine-verifiable contract:
//!
//!   1  mkdir /HOME                    (may already exist -> tolerated)
//!   2  mkdir /HOME/DOCS9              (nested under a fresh parent)
//!   3  write /HOME/DOCS9/NOTE.TXT     (nested open with O_CREATE|O_RDWR)
//!   4  read back the same content
//!   5  chdir /HOME/DOCS9              (absolute)
//!   6  getcwd == /HOME/DOCS9
//!   7  write REL.TXT by BARE name     (lands in the cwd)
//!   8  read /HOME/DOCS9/REL.TXT back  (cross-check via absolute path)
//!   9  chdir ..                       (relative normalization)
//!   10 getcwd == /HOME
//!
//! Then it cleans up: rmdir fails while REL.TXT exists (ddel order is
//! the kernel's business) — this program unlinks its own files first
//! and rmdirs DOCS9, leaving only /HOME behind.

#![no_std]
#![no_main]

use glm_user::{
    chdir, exit, file_close, file_open, file_read, file_unlink, file_write, getcwd, mkdir, rmdir,
    write, O_CREATE, O_RDWR,
};

#[no_mangle]
pub extern "C" fn _start() -> ! {
    write("MKTREE: ring-3 directory tree test (v1.9 syscalls 49-52)\n");
    let mut checks: i64 = 0;

    // 1. /HOME (tolerate "exists" — the disk may be seeded or re-run)
    if mkdir("/HOME") == 0 {
        write("  1. mkdir /HOME ok\n");
    } else {
        write("  1. mkdir /HOME -> exists (tolerated)\n");
    }
    checks += 1;

    // 2. fresh nested directory
    if mkdir("/HOME/DOCS9") == 0 {
        write("  2. mkdir /HOME/DOCS9 ok\n");
    } else {
        write("  2. mkdir /HOME/DOCS9 FAILED\n");
        exit(checks);
    }
    checks += 1;

    // 3. nested write
    let msg = b"nested write from ring 3, v1.9";
    let fd = file_open("/HOME/DOCS9/NOTE.TXT", O_CREATE | O_RDWR);
    if fd < 0 {
        write("  3. nested open FAILED\n");
        exit(checks);
    }
    let wn = file_write(fd, msg);
    file_close(fd);
    if wn == msg.len() as i64 {
        write("  3. write /HOME/DOCS9/NOTE.TXT ok\n");
    } else {
        write("  3. nested write FAILED\n");
        exit(checks);
    }
    checks += 1;

    // 4. nested read-back
    let fd = file_open("/HOME/DOCS9/NOTE.TXT", O_RDWR);
    let mut buf = [0u8; 64];
    let rn = if fd < 0 { -1 } else { file_read(fd, &mut buf) };
    if fd >= 0 {
        file_close(fd);
    }
    if rn == msg.len() as i64 && &buf[..msg.len()] == msg {
        write("  4. read-back matches\n");
    } else {
        write("  4. read-back FAILED\n");
        exit(checks);
    }
    checks += 1;

    // 5-6. absolute chdir + getcwd
    let mut cwd = [0u8; 64];
    if chdir("/HOME/DOCS9") == 0 && getcwd(&mut cwd) == 11 {
        let got = core::str::from_utf8(&cwd[..11]).unwrap_or("?");
        if got == "/HOME/DOCS9" {
            write("  5-6. chdir + getcwd == /HOME/DOCS9 ok\n");
            checks += 2;
        } else {
            write("  5-6. getcwd mismatch\n");
            exit(checks);
        }
    } else {
        write("  5-6. chdir/getcwd FAILED\n");
        exit(checks);
    }

    // 7. bare-name write lands in the cwd
    let rel = b"relative!";
    let fd = file_open("REL.TXT", O_CREATE | O_RDWR);
    let wn = if fd < 0 { -1 } else { file_write(fd, rel) };
    if fd >= 0 {
        file_close(fd);
    }
    if wn == rel.len() as i64 {
        write("  7. write REL.TXT (bare name) ok\n");
    } else {
        write("  7. relative write FAILED\n");
        exit(checks);
    }
    checks += 1;

    // 8. cross-check through the absolute path
    let fd = file_open("/HOME/DOCS9/REL.TXT", O_RDWR);
    let rn = if fd < 0 { -1 } else { file_read(fd, &mut buf) };
    if fd >= 0 {
        file_close(fd);
    }
    if rn == rel.len() as i64 && &buf[..rel.len()] == rel {
        write("  8. /HOME/DOCS9/REL.TXT matches\n");
    } else {
        write("  8. absolute cross-check FAILED\n");
        exit(checks);
    }
    checks += 1;

    // 9-10. relative chdir up + getcwd
    if chdir("..") == 0 && getcwd(&mut cwd) == 5 {
        let got = core::str::from_utf8(&cwd[..5]).unwrap_or("?");
        if got == "/HOME" {
            write("  9-10. chdir .. + getcwd == /HOME ok\n");
            checks += 2;
        } else {
            write("  9-10. getcwd after .. mismatch\n");
            exit(checks);
        }
    } else {
        write("  9-10. relative chdir FAILED\n");
        exit(checks);
    }

    // cleanup: the files first, then the directory (rmdir needs empty)
    file_unlink("/HOME/DOCS9/NOTE.TXT");
    file_unlink("/HOME/DOCS9/REL.TXT");
    if rmdir("/HOME/DOCS9") == 0 {
        write("  cleanup: /HOME/DOCS9 removed\n");
    } else {
        write("  cleanup: rmdir /HOME/DOCS9 FAILED\n");
        exit(checks);
    }

    write("MKTREE: all checks passed, exit code = ");
    let mut nb = [0u8; 20];
    write(core::str::from_utf8(glm_user::fmt_u64(checks as u64, &mut nb)).unwrap_or("?"));
    write("\n");
    exit(checks);
}

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}
