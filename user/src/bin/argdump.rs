//! argdump — v1.7 debug: compact argv inspection.
#![no_std]
#![no_main]

use glm_user::{arg_str, cstr_into, exit, fmt_u64, write};

const STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;

fn hexbyte(c: u8, out: &mut [u8; 2]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out[0] = HEX[(c >> 4) as usize];
    out[1] = HEX[(c & 0xF) as usize];
}

#[no_mangle]
pub extern "C" fn _start(argc: i64, argv: *const *const u8) -> ! {
    write("ARGDUMP\n");
    let mut ib = [0u8; 20];
    write("argc = ");
    write(core::str::from_utf8(fmt_u64(argc as u64, &mut ib)).unwrap_or("?"));
    write("\n");

    let n = (argc as usize).min(16);
    let mut sbuf = [0u8; 128];
    for i in 0..n {
        let p = unsafe { *argv.add(i) } as *const u8;
        let s = cstr_into(p, &mut sbuf);
        write("arg[");
        let mut ib2 = [0u8; 20];
        write(core::str::from_utf8(fmt_u64(i as u64, &mut ib2)).unwrap_or("?"));
        write("] len=");
        let mut ib3 = [0u8; 20];
        write(core::str::from_utf8(fmt_u64(s.len() as u64, &mut ib3)).unwrap_or("?"));
        write(" '");
        write(arg_str(s));
        write("'\n");
    }

    // compact hex of the top 48 bytes: two 48-char lines
    write("raw  [TOP-48..TOP):\n");
    let mut half = 0usize;
    let mut hx = [0u8; 2];
    let mut line = [0u8; 96];
    unsafe {
        let mut a = STACK_TOP - 48;
        while a < STACK_TOP {
            hexbyte(*(a as *const u8), &mut hx);
            line[half * 2] = hx[0];
            line[half * 2 + 1] = hx[1];
            half += 1;
            if half == 24 {
                write(core::str::from_utf8(&line).unwrap_or("?"));
                write("\n");
                half = 0;
            }
            a += 1;
        }
    }
    exit(0);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    exit(101)
}
