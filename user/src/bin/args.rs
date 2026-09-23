//! args — v1.7 argv echo: proof that ring-3 programs receive arguments.
//!
//! Every GLM OS program starts in ring 3 with (rdi=argc, rsi=argv) —
//! the classic Unix entry ABI, planted by the kernel bootstrap frame
//! (spawn) or by exec's frame surgery. This program prints the argument
//! list and EXITS WITH CODE argc (including argv[0]), so the serial log
//! line "exited with code N" is the machine-readable proof:
//!
//!   run ARGS.ELF               -> argc=1 -> exit 1
//!   run ARGS.ELF one two three -> argc=4 -> exit 4
//!   drun ARGS.ELF a b          -> argc=3 -> exit 3 (from the disk)

#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, fmt_u64, write};

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    let mut nbuf = [0u8; 20];
    write("ARGS: argv echo from ring 3 (v1.7)\n");
    write("  argc = ");
    write(core::str::from_utf8(fmt_u64(argc as u64, &mut nbuf)).unwrap_or("?"));
    write("\n");

    let n = (argc as usize).min(16);
    let mut sbuf = [0u8; 128];
    for i in 0..n {
        let p = unsafe { *argv.add(i) };
        let s = cstr_into(p, &mut sbuf);
        let mut ibuf = [0u8; 20];
        write("  argv[");
        write(core::str::from_utf8(fmt_u64(i as u64, &mut ibuf)).unwrap_or("?"));
        write("] = \"");
        write(arg_str(s));
        write("\"\n");
    }

    write("ARGS: goodbye, exiting with code = argc\n");
    exit(argc);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
