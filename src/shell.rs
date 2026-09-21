//! glmsh — the GLM OS shell.
//!
//! Polls the keyboard ring buffer, maintains the input line, dispatches
//! commands. Runs after interrupts are online; blinks the cursor from
//! PIT ticks without ever touching the console from IRQ context.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::console;
use crate::console::{GLM_CYAN, GLM_GRAY, GLM_GREEN, GLM_MAGENTA, GLM_WHITE, GLM_YELLOW};
use crate::cpu::keyboard;
use crate::cpu::pit;
use crate::io::ports::hlt;

const LINE_MAX: usize = 128;

static LINE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static mut LINE_BUF: [u8; LINE_MAX] = [0; LINE_MAX];
static CURSOR_ON: AtomicBool = AtomicBool::new(true);

fn line_len() -> usize {
    LINE.load(Ordering::Relaxed)
}

fn line_bytes() -> &'static mut [u8] {
    unsafe {
        let n = LINE.load(Ordering::Relaxed);
        &mut LINE_BUF[..n]
    }
}

fn prompt() {
    console::print_color("glm", GLM_CYAN);
    console::print_color("> ", GLM_MAGENTA);
    console::cursor_draw();
}

pub fn run() -> ! {
    console::newline();
    console::set_color_global(GLM_GREEN);
    console::print("  Welcome to the GLM OS shell (glmsh 0.1). Type 'help'.");
    console::set_color_global(GLM_GRAY);
    console::newline();
    console::newline();
    prompt();

    let mut last_blink = pit::ticks();
    loop {
        if let Some(c) = keyboard::pop() {
            match c {
                b'\n' => {
                    console::cursor_erase();
                    console::newline();
                    execute(line_bytes());
                    unsafe {
                        core::ptr::write_bytes(&raw mut LINE_BUF as *mut u8, 0, LINE_MAX);
                    }
                    LINE.store(0, Ordering::Relaxed);
                    prompt();
                }
                0x08 => {
                    let n = line_len();
                    if n > 0 {
                        // erase last char: backspace, space, backspace
                        console::print("\x08 \x08");
                        LINE.store(n - 1, Ordering::Relaxed);
                        console::cursor_draw();
                    }
                }
                c if c.is_ascii_graphic() || c == b' ' => {
                    let n = line_len();
                    if n < LINE_MAX {
                        unsafe {
                            (&raw mut LINE_BUF as *mut u8).add(n).write(c);
                        }
                        LINE.store(n + 1, Ordering::Relaxed);
                        console::print_color(core::str::from_utf8(&[c]).unwrap_or("?"), GLM_WHITE);
                        console::cursor_draw();
                    }
                }
                _ => {}
            }
            last_blink = pit::ticks();
        } else {
            // idle: blink the cursor based on PIT ticks
            let t = pit::ticks();
            if t.saturating_sub(last_blink) >= 25 {
                last_blink = t;
                let on = CURSOR_ON.load(Ordering::Relaxed);
                if on {
                    console::cursor_erase();
                } else {
                    console::cursor_draw();
                }
                CURSOR_ON.store(!on, Ordering::Relaxed);
            }
            hlt();
        }
    }
}

fn execute(line: &[u8]) {
    // trim
    let line = match line.iter().position(|&b| b != b' ') {
        Some(start) => &line[start..],
        None => return,
    };
    let line = match line.iter().rposition(|&b| b != b' ') {
        Some(end) => &line[..=end],
        None => return,
    };

    let (cmd, rest) = match line.iter().position(|&b| b == b' ') {
        Some(pos) => (&line[..pos], &line[pos + 1..]),
        None => (line, &line[..0]),
    };
    let cmd = core::str::from_utf8(cmd).unwrap_or("");
    let rest = core::str::from_utf8(rest).unwrap_or("").trim();

    match cmd {
        "help" => cmd_help(),
        "clear" => console::clear(),
        "echo" => {
            console::print(rest);
            console::newline();
        }
        "uptime" => {
            let ms = pit::uptime_ms();
            console::print_args(format_args!(
                "up {}h {}m {}s ({} ticks)\n",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                pit::ticks()
            ));
        }
        "halt" => {
            console::print_color("It's now safe to turn off your computer. (halted)\n", GLM_YELLOW);
            crate::klog!("halt requested from shell");
            loop {
                hlt();
            }
        }
        "reboot" => {
            crate::klog!("reboot requested from shell");
            crate::cpu::reboot();
        }
        "about" => cmd_about(),
        "mem" => cmd_mem(),
        "paging" => cmd_paging(),
        "ls" => cmd_ls(rest),
        "cat" => cmd_cat(rest),
        "neofetch" => cmd_neofetch(),
        "glm" => cmd_glm_quote(),
        _ => {
            console::print_color("glmsh: unknown command: ", GLM_YELLOW);
            console::print(cmd);
            console::print_color(" (try 'help')\n", GLM_GRAY);
        }
    }
}

fn cmd_help() {
    console::print_color("GLM OS shell commands:\n", GLM_CYAN);
    for (name, desc) in [
        ("help", "show this help"),
        ("clear", "clear the screen"),
        ("echo <text>", "print text back"),
        ("uptime", "time since boot (PIT @ 100 Hz)"),
        ("mem", "physical frames + heap statistics"),
        ("paging", "CR3 and PML4 map introspection"),
        ("ls [path]", "list FAT32 ramdisk directory"),
        ("cat <file>", "print a file from the ramdisk"),
        ("neofetch", "system summary with logo"),
        ("glm", "wisdom of the machine"),
        ("about", "what is GLM OS"),
        ("reboot", "reset the machine (8042)"),
        ("halt", "stop the CPU forever"),
    ] {
        console::print_color("  ", GLM_GRAY);
        console::print_color(name, GLM_WHITE);
        console::print(" - ");
        console::print(desc);
        console::newline();
    }
}

fn cmd_about() {
    console::print_color("GLM OS v0.1.0\n", GLM_CYAN);
    console::print("  a 64-bit hobby operating system for x86_64\n");
    console::print("  designed, written and tested by GLM (Z.ai)\n");
    console::print("  kernel: pure Rust, no_std, zero runtime dependencies\n");
    console::print("  boot:   Limine 12 (long mode entry), framebuffer console\n");
    console::print("  stack:  own GDT/IDT, 8259 PIC, PIT timer, PS/2 keyboard\n");
}


fn cmd_mem() {
    let fs = crate::mem::frames::stats();
    console::print_color("physical memory (bitmap frame allocator):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  usable: {} MiB in {} regions | used: {} frames | free: {} frames\n",
        fs.total * 4 / 1024,
        fs.regions,
        fs.used,
        fs.total - fs.used
    ));
    if let Some(h) = crate::mem::heap::stats() {
        console::print_color("kernel heap (free-list allocator):\n", GLM_CYAN);
        console::print_args(format_args!(
            "  size: {} MiB | allocated: {} KiB | allocs: {} | frees: {} | fails: {}\n",
            h.size / (1024 * 1024),
            h.allocated / 1024,
            h.allocs,
            h.frees,
            h.fails
        ));
    }
}

fn cmd_paging() {
    let (pml4, mapped) = crate::mem::paging::describe();
    console::print_color("paging (bootloader page tables active):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  cr3 (pml4 phys) = {:#x} | hhdm offset = {:#x} | entries mapped: {}/512\n",
        pml4,
        crate::mem::paging::hhdm_offset(),
        mapped
    ));
    crate::mem::paging::dump(6);
}

fn cmd_ls(path: &str) {
    let fat = crate::fs::fat32::FAT.lock();
    match fat.as_ref() {
        None => console::print_color("fat32: ramdisk not mounted\n", GLM_YELLOW),
        Some(fs) => match fs.ls(path) {
            Ok(entries) => {
                console::print_args(format_args!("listing of {}\n", if path.is_empty() { "/" } else { path }));
                for e in &entries {
                    if e.is_dir {
                        console::print_color("  <DIR>  ", GLM_MAGENTA);
                        console::print_color(&e.name, GLM_MAGENTA);
                    } else {
                        console::print_args(format_args!("  {:>6}  ", e.size));
                        console::print_color(&e.name, GLM_CYAN);
                    }
                    console::newline();
                }
            }
            Err(e) => {
                console::print_color("ls: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_cat(path: &str) {
    if path.is_empty() {
        console::print_color("usage: cat <file>\n", GLM_YELLOW);
        return;
    }
    let fat = crate::fs::fat32::FAT.lock();
    match fat.as_ref() {
        None => console::print_color("fat32: ramdisk not mounted\n", GLM_YELLOW),
        Some(fs) => match fs.cat(path) {
            Ok(bytes) => {
                console::print_color("\n", GLM_GRAY);
                for chunk in bytes.chunks(256) {
                    let s: alloc::string::String = chunk
                        .iter()
                        .map(|&b| if b.is_ascii_graphic() || b == b' ' || b == b'\n' || b == b'\t' || b == b'\r' { b as char } else { '.' })
                        .collect();
                    console::print(&s);
                }
                console::newline();
            }
            Err(e) => {
                console::print_color("cat: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_neofetch() {
    const LOGO: [&str; 9] = [
        "      /\\      ",
        "     /  \\     ",
        "    / /\\ \\    ",
        "   / /__\\ \\   ",
        "  / /    \\ \\  ",
        " / /      \\ \\ ",
        "/_/        \\_\\",
        "               ",
        "               ",
    ];

    let ms = crate::cpu::pit::uptime_ms();
    let uptime = alloc::format!(
        "{}h {}m {}s",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60
    );

    let fs_stats = crate::mem::frames::stats();
    let heap = crate::mem::heap::stats();
    let bootver = limine_boot_version();
    let ramdisk_note = alloc::format!(
        "{} files",
        crate::fs::fat32::FAT
            .lock()
            .as_ref()
            .map(|f| f.root_file_count())
            .unwrap_or(0)
    );

    let info: [alloc::string::String; 9] = [
        alloc::format!("glm@glm-os"),
        alloc::format!("-----------"),
        alloc::format!("OS:        GLM OS 0.1.0 (x86_64 long mode)"),
        alloc::format!("Kernel:    glm 0.1.0, pure Rust no_std"),
        alloc::format!("Boot:      Limine {}", bootver),
        alloc::format!("Uptime:    {}", uptime),
        alloc::format!("Shell:     glmsh 0.1"),
        alloc::format!(
            "Memory:    {} MiB frames, {}/64 MiB heap used",
            fs_stats.total * 4 / 1024,
            heap.as_ref().map(|h| h.allocated / (1024 * 1024)).unwrap_or(0)
        ),
        alloc::format!("Ramdisk:   FAT32, {}", ramdisk_note),
    ];

    for i in 0..9 {
        console::print_color(LOGO[i], GLM_CYAN);
        console::print("  ");
        if i == 0 {
            console::print_color(&info[0], GLM_MAGENTA);
        } else if i == 1 {
            console::print_color(&info[1], GLM_GRAY);
        } else {
            console::print_color(&info[i], GLM_WHITE);
        }
        console::newline();
    }
}

fn limine_boot_version() -> &'static str {
    crate::limine_reqs::bootloader_version().unwrap_or("?")
}

fn cmd_glm_quote() {
    const QUOTES: [&str; 5] = [
        "I am the kernel and the userland. There is no distinction.",
        "Written by a model, executed by a CPU. Hello!",
        "Every byte of this OS passed through a neural network.",
        "sleep() is just hlt() with ambition.",
        "There are two kinds of kernels: mine, and the rest.",
    ];
    let idx = (crate::cpu::pit::ticks() as usize) % QUOTES.len();
    console::print_color("glm says: ", GLM_MAGENTA);
    console::print_color(QUOTES[idx], GLM_CYAN);
    console::newline();
}
