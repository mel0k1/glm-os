//! files — v1.6 directory listing from ring 3, v1.9 path-aware.
//!
//! Calls SYS_FILE_LIST (int 0x80 #42) — since v1.9 the call takes a
//! PATH (argv[1], or the task's working directory when omitted) and
//! returns entries of THAT directory, each record tagging files and
//! subdirectories. The program walks the packed records, prints every
//! entry, and EXITS WITH THE ENTRY COUNT — the test harness verifies
//! cross-visibility: the kernel shell's dsave/ddel/mkdir change the
//! same tree this program sees, and the listing survives reboots.

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, file_list, file_record, fmt_u64, write};

fn print_num(v: u64) {
    let mut b = [0u8; 20];
    write(core::str::from_utf8(fmt_u64(v, &mut b)).unwrap_or("?"));
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    // v1.9: argv[1] names the directory to list; no argument lists the
    // task's working directory (empty path resolves to the cwd).
    let mut sbuf = [0u8; 128];
    let path: &str = if argc >= 2 {
        arg_str(cstr_into(unsafe { *argv.add(1) }, &mut sbuf))
    } else {
        ""
    };

    write("FILES: listing a persistent-disk directory from ring 3 (int 0x80 #42)\n");
    if !path.is_empty() {
        write("  path: ");
        write(path);
        write("\n");
    }

    let mut buf = [0u8; 2048];
    let n = file_list(path, &mut buf, 32);
    if n < 0 {
        write("  file_list failed - no persistent disk / bad path?\n");
        exit(-1);
    }

    let mut off = 0usize;
    let mut dirs = 0u64;
    let mut files = 0u64;
    for _ in 0..n {
        match file_record(&buf, off) {
            Some((kind, name, size, next)) => {
                if kind == 1 {
                    dirs += 1;
                    write("   <DIR>   ");
                } else {
                    files += 1;
                    write("  ");
                    print_num(size as u64);
                    write("  ");
                }
                write(core::str::from_utf8(name).unwrap_or("(bad utf8)"));
                write("\n");
                off = next;
            }
            None => break,
        }
    }
    write("  total: ");
    print_num(files);
    write(" file(s), ");
    print_num(dirs);
    write(" dir(s); exit code = entry count\n");
    exit(n);
}

#[panic_handler]
fn ph(_: &core::panic::PanicInfo) -> ! {
    exit(-2)
}
