//! GLM OS — a 64-bit hobby operating system, designed and written by GLM (Z.ai).
//!
//! Boot flow (v0.1):
//!   Limine 12 -> long mode -> higher-half ELF kernel (this file)
//!   _start (asm: stack setup) -> kmain()
//!
//! Stage 1: serial + framebuffer console, banner, boot log.

#![no_std]
#![no_main]

extern crate alloc;

mod console;
mod cpu;
mod fs;
mod io;
mod limine_reqs;
mod mem;
mod shell;
mod sync;

use core::arch::global_asm;
use core::panic::PanicInfo;

// Interrupt service routine stubs (hand-written asm, vectors 0..=47)
global_asm!(include_str!("cpu/isr_stubs.S"));

use console::{GLM_CYAN, GLM_GRAY, GLM_GREEN, GLM_MAGENTA, GLM_RED, GLM_WHITE, GLM_YELLOW};
use io::ports::hlt;
use io::serial::COM1;

// ---------------------------------------------------------------------------
// Entry point: set up our own kernel stack, then jump into Rust.
// ---------------------------------------------------------------------------

global_asm!(
    ".section .bss",
    ".balign 16",
    "kstack_bottom:",
    ".skip 262144", // 256 KiB kernel stack
    "kstack_top:",
    ".section .text",
    ".globl _start",
    "_start:",
    // bootstrap ping: raw 'A' straight out of COM1 (QEMU sends THR unconditionally)
    "    mov dx, 0x3f8",
    "    mov al, 65",
    "    out dx, al",
    "    lea rsp, [rip + kstack_top]",
    "    xor ebp, ebp",
    "    call {kmain}",
    "1:  hlt",
    "    jmp 1b",
    kmain = sym kmain,
);

// ---------------------------------------------------------------------------
// Banner (figlet-style "GLM OS", assembled letter by letter at runtime)
// ---------------------------------------------------------------------------

const BANNER_G: [&str; 5] = [
    "  ____ ",
    " / ___|",
    "| |  _ ",
    "| |_| |",
    " \\____|",
];
const BANNER_L: [&str; 5] = [" _     ", "| |    ", "| |    ", "| |___ ", "|_____|"];
const BANNER_M: [&str; 5] = [
    " __  __ ",
    "|  \\/  |",
    "| |\\/| |",
    "| |  | |",
    "|_|  |_|",
];
const BANNER_O: [&str; 5] = [
    "  ___  ",
    " / _ \\ ",
    "| | | |",
    "| |_| |",
    " \\___/ ",
];
const BANNER_S: [&str; 5] = [" ____  ", "/ ___| ", "\\___ \\ ", " ___) |", "|____/ "];
const BANNER_SPACE: [&str; 5] = ["    ", "    ", "    ", "    ", "    "];
const BANNER_GAP: [&str; 5] = [" ", " ", " ", " ", " "];

const BANNER_ROW_COLORS: [u8; 5] = [GLM_CYAN, GLM_CYAN, GLM_MAGENTA, GLM_CYAN, GLM_CYAN];

// ---------------------------------------------------------------------------
// Boot log helpers: colored [ ok ] lines on screen, mirrored to COM1
// ---------------------------------------------------------------------------

macro_rules! okline {
    ($($arg:tt)*) => {{
        console::print("  [ ");
        console::print_color(" ok ", GLM_GREEN);
        console::print(" ] ");
        console::print_args(format_args!($($arg)*));
        console::newline();
        klog!($($arg)*);
    }};
}

macro_rules! warnline {
    ($($arg:tt)*) => {{
        console::print("  [ ");
        console::print_color("warn", GLM_YELLOW);
        console::print(" ] ");
        console::print_args(format_args!($($arg)*));
        console::newline();
        klog!($($arg)*);
    }};
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

extern "C" fn kmain() -> ! {
    // Wait until Limine acknowledges the requested base revision
    // (the bootloader zeroes the revision field of the tag when supported).
    while !limine_reqs::BASE_REVISION.is_supported() {
        core::hint::spin_loop();
    }

    COM1.init();
    klog!("GLM OS v0.1.0 (x86_64, long mode) kernel entry");

    // --- framebuffer console -------------------------------------------------
    let mut fb_desc: Option<(usize, usize, usize)> = None;
    if let Some(resp) = limine_reqs::FRAMEBUFFER_REQUEST.get_response() {
        if let Some(fb) = resp.framebuffers().next() {
            let base = fb.addr();
            let pitch = fb.pitch() as usize;
            let width = fb.width() as usize;
            let height = fb.height() as usize;
            let bpp = fb.bpp() as usize;
            let masks = [
                fb.red_mask_shift(),
                fb.green_mask_shift(),
                fb.blue_mask_shift(),
                fb.red_mask_size(),
                fb.green_mask_size(),
                fb.blue_mask_size(),
            ];
            console::init(base, pitch, width, height, bpp, masks);
            fb_desc = Some((width, height, bpp));
            klog!("framebuffer {}x{}x{} @ {:#x}", width, height, bpp, base as usize);
        }
    }

    // --- boot screen ----------------------------------------------------------
    console::clear();

    let letters: [&[&str]; 10] = [
        &BANNER_G,
        &BANNER_GAP,
        &BANNER_L,
        &BANNER_GAP,
        &BANNER_M,
        &BANNER_SPACE,
        &BANNER_O,
        &BANNER_GAP,
        &BANNER_S,
        &BANNER_GAP,
    ];
    for row in 0..5 {
        console::set_color_global(BANNER_ROW_COLORS[row]);
        for letter in letters.iter() {
            console::print(letter[row]);
        }
        console::newline();
    }

    console::set_color_global(GLM_MAGENTA);
    console::print("   the operating system designed, written and tested by GLM");
    console::newline();
    console::set_color_global(GLM_GRAY);
    console::print("   v0.1.0  x86_64 long mode  written in Rust  boots via Limine");
    console::newline();
    console::newline();

    okline!("cpu: long mode (x86_64) entered via Limine");
    okline!("serial: COM1 115200 8N1 (kernel debug log)");
    if let Some((w, h, bpp)) = fb_desc {
        okline!("framebuffer: {}x{}x{} BGRA", w, h, bpp);
    } else {
        warnline!("framebuffer: unavailable, console disabled (serial only)");
    }
    if let Some(resp) = limine_reqs::BOOTLOADER_INFO_REQUEST.get_response() {
        okline!("bootloader: {} {}", resp.name(), resp.version());
    }
    okline!("console: 8x8 bitmap font, 2x scale, direct framebuffer writes");

    // --- interrupt stack ------------------------------------------------------
    cpu::gdt::init();
    okline!("gdt: null / code64 (0x08) / data64 (0x10) loaded");
    cpu::idt::init();
    okline!("idt: 256 gates, exceptions + pic irqs hooked");
    cpu::pic::init();
    okline!("pic: 8259 remapped to vectors 0x20-0x2f");
    cpu::pit::init();
    okline!("pit: channel 0 @ {} hz (uptime + cursor blink)", 100);
    cpu::keyboard::init();
    okline!("keyboard: ps/2 port 1, scancode set 1, irq1");
    cpu::enable_interrupts();
    okline!("interrupts: enabled (rflags.if = 1)");

    // --- memory ---------------------------------------------------------------
    let hhdm = limine_reqs::hhdm_offset();
    mem::paging::init(hhdm);
    if let Some(resp) = limine_reqs::MEMMAP_REQUEST.get_response() {
        mem::frames::init(resp.entries());
        let st = mem::frames::stats();
        okline!(
            "pmm: {} MiB usable in {} regions (bitmap allocator)",
            st.total * 4 / 1024,
            st.regions
        );
    } else {
        warnline!("pmm: no memory map from bootloader");
    }
    if mem::heap::init() {
        okline!("heap: 64 MiB free-list allocator online (own impl)");
    } else {
        warnline!("heap: init failed");
    }
    let (pml4, mapped) = mem::paging::describe();
    okline!("paging: cr3={:#x}, {}/512 pml4 entries mapped", pml4, mapped);

    // --- fat32 ramdisk --------------------------------------------------------
    match fs::fat32::mount_from_limine() {
        Ok(files) => okline!("fat32: ramdisk mounted, {} files in root", files),
        Err(e) => warnline!("fat32: ramdisk not mounted ({})", e),
    }

    // --- shell ----------------------------------------------------------------
    console::set_color_global(GLM_WHITE);
    console::print("  GLM OS v0.1.0 ready.");
    console::set_color_global(GLM_GRAY);
    console::newline();
    klog!("boot complete, handing over to glmsh");
    shell::run();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    klog!("KERNEL PANIC: {info}");
    console::print_color("KERNEL PANIC: ", GLM_RED);
    console::print_args(format_args!("{info}"));
    console::newline();
    loop {
        hlt();
    }
}
