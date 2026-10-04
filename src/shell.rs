//! glmsh — the GLM OS shell.
//!
//! Polls the keyboard ring buffer, maintains the input line, dispatches
//! commands. Runs after interrupts are online; blinks the cursor from
//! PIT ticks without ever touching the console from IRQ context.
//! v2.5: line editing + command history live in the shared lineedit
//! module (arrows, Home/End, Delete, mid-line editing, history ring).

use core::sync::atomic::{AtomicBool, Ordering};

use crate::console;
use crate::console::{GLM_CYAN, GLM_GRAY, GLM_GREEN, GLM_MAGENTA, GLM_RED, GLM_WHITE, GLM_YELLOW};
use crate::cpu::keyboard;
use crate::cpu::pit;
use crate::io::ports::hlt;

static CURSOR_ON: AtomicBool = AtomicBool::new(true);

fn prompt() {
    print_prompt_text();
    console::cursor_draw();
}

/// v2.8: the shared prompt body — the text shell, the terminal windows
/// and lineedit's TAB candidate listing all print exactly the same thing
/// ("glm" + cwd + "> "), so a completion never invents a second style.
pub fn print_prompt_text() {
    console::print_color("glm", GLM_CYAN);
    // v1.9: the prompt shows the working directory once it leaves "/",
    // tail-truncated when the path grows long
    let cwd = crate::sched::current_cwd();
    if cwd.len() > 1 {
        let shown = if cwd.len() > 16 {
            alloc::format!("...{}", &cwd[cwd.len() - 13..])
        } else {
            cwd
        };
        console::print_color(&shown, GLM_CYAN);
    }
    console::print_color("> ", GLM_MAGENTA);
}

/// v1.9: resolve a shell path argument (absolute, relative, or empty =
/// the cwd itself) into a normalized absolute disk path.
fn resolve_arg(arg: &str) -> alloc::string::String {
    crate::fs::sysfile::resolve_path(crate::sched::current_pid(), arg.trim())
}

/// v1.9: cd — move the shell's working directory on the persistent disk.
fn cmd_cd(rest: &str) {
    let arg = rest.trim();
    if arg.is_empty() {
        console::print_args(format_args!("{}\n", crate::sched::current_cwd()));
        return;
    }
    let r = crate::fs::sysfile::chdir_name(crate::sched::current_pid(), arg);
    if r < 0 {
        console::print_color("cd: cannot change directory\n", GLM_YELLOW);
    }
}

/// v1.9: pwd — print the working directory.
fn cmd_pwd() {
    console::print(&crate::sched::current_cwd());
    console::newline();
}

/// v1.9: mkdir — create a directory node (nested paths welcome).
fn cmd_mkdir(rest: &str) {
    let arg = rest.trim();
    if arg.is_empty() {
        console::print_color("usage: mkdir <dir>  (e.g. mkdir /HOME/DOCS)\n", GLM_YELLOW);
        return;
    }
    let r = crate::fs::sysfile::mkdir_name(crate::sched::current_pid(), arg);
    if r < 0 {
        console::print_color("mkdir: cannot create ", GLM_YELLOW);
        console::print(arg);
        console::newline();
    }
}

/// v1.9: rmdir — remove an EMPTY directory (never the shell's own cwd).
fn cmd_rmdir(rest: &str) {
    let arg = rest.trim();
    if arg.is_empty() {
        console::print_color("usage: rmdir <dir>\n", GLM_YELLOW);
        return;
    }
    let pid = crate::sched::current_pid();
    let target = crate::fs::sysfile::resolve_path(pid, arg);
    if target == crate::sched::current_cwd() {
        console::print_color("rmdir: refusing to remove the working directory\n", GLM_YELLOW);
        return;
    }
    if target == "/" {
        console::print_color("rmdir: refusing to remove the root\n", GLM_YELLOW);
        return;
    }
    let r = crate::fs::sysfile::rmdir_name(pid, arg);
    if r < 0 {
        console::print_color("rmdir: cannot remove ", GLM_YELLOW);
        console::print(arg);
        console::newline();
    }
}

pub fn run() -> ! {
    // v2.8: boot-time DHCP — the machine LEARNS its address with zero
    // manual steps. The ring-3 client runs in the background; a failure
    // keeps the previous (boot default) config, so slirp boots are never
    // deconfigured by a missing server.
    boot_dhcp();

    console::newline();
    console::set_color_global(GLM_GREEN);
    console::print("  Welcome to the GLM OS shell (glmsh 2.9). Type 'help'.");
    console::set_color_global(GLM_GRAY);
    console::newline();
    console::newline();
    prompt();

    let mut last_blink = pit::ticks();
    // v2.5: the shared line editor — arrows walk history, Home/End/Delete
    // and mid-line editing work, exactly like the terminal windows
    let mut ed = crate::lineedit::LineEdit::new();
    let mut out = [0u8; crate::lineedit::LINE_CAP];
    loop {
        // v1.4: while the GUI owns the screen the compositor routes keys
        // (terminal windows / ring-3 apps); the text shell must not steal
        // keystrokes or echo into the hidden console.
        if crate::console::GUI_ACTIVE.load(Ordering::Relaxed) {
            crate::sched::ksyscall(crate::sched::SYS_SLEEP, 20, 0, 0);
            continue;
        }
        if let Some(c) = keyboard::pop() {
            match ed.feed(c, &mut out) {
                crate::lineedit::Fed::Line(n) => {
                    console::cursor_erase();
                    console::newline();
                    execute(&out[..n]);
                    prompt();
                }
                crate::lineedit::Fed::Eof => {
                    // v2.8: Ctrl+D on an empty line — the classic logout.
                    // The CONSOLE shell is pid 1: it must survive, so EOF
                    // prints a hint and re-prompts instead of dying.
                    crate::klog!("shell: eof (^D) at the console prompt — the console stays up");
                    console::cursor_erase();
                    console::newline();
                    console::print_color(
                        "  eof: this console stays up — use 'reboot' or 'halt'\n",
                        GLM_GRAY,
                    );
                    prompt();
                }
                crate::lineedit::Fed::None => {}
            }
            last_blink = pit::ticks();
        } else {
            // idle: blink the cursor based on PIT ticks, and let other
            // tasks run (the shell is a regular scheduler citizen now)
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
            crate::sched::ksyscall(crate::sched::SYS_YIELD, 0, 0, 0);
        }
    }
}

/// v1.4: dispatch one command line. Runs in the caller's task; with a
/// terminal window redirect active (`sched::current_out_win() != 0`) the
/// session gets two extra commands and must never re-enter the compositor.
pub fn execute(line: &[u8]) {
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

    // v1.4: terminal-session context — the `gui` command must NOT re-enter
    // the compositor (gui::run is a per-screen singleton), and the session
    // gets an `exit` command that closes its own window.
    let session = crate::sched::current_out_win() != 0;

    // v1.8: pipelines and redirections go through their own builder before
    // the plain-command match ("run A | B", "run A > f", "A.ELF < f", ...).
    let line_str = core::str::from_utf8(line).unwrap_or("");
    if scan_pipeline(line_str) && (matches!(cmd, "run" | "spawn" | "drun") || looks_like_prog(cmd))
    {
        cmd_pipeline(cmd, line_str);
        return;
    }

    match cmd {
        "help" => cmd_help(),
        "clear" => console::clear(),
        "echo" => {
            console::print(rest);
            console::newline();
        }
        "gui" if session => {
            crate::gui::desktop_open_monitor();
            console::print("  desktop: system monitor opened\n");
        }
        "exit" if session => {
            console::print_color("bye\n", GLM_GRAY);
            crate::gui::term_close(crate::sched::current_out_win());
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
        // v2.4: the wall clock (CMOS RTC + uptime, NTP-adjustable)
        "date" => cmd_date(),
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
        "vmm" => cmd_vmm(),
        "run" => cmd_run(rest),
        "spawn" => cmd_spawn(rest),
        "ps" | "tasks" => cmd_ps(),
        "kill" => cmd_kill(rest),
        "ipc" => cmd_ipc(),
        "sleep" => cmd_sleep(rest),
        "cpu" => cmd_cpu(rest),
        "ls" => cmd_ls(rest),
        "cat" => cmd_cat(rest),
        "neofetch" => cmd_neofetch(),
        "gui" => crate::gui::run(),
        "mouse" => cmd_mouse(),
        "pipe" => cmd_pipe(),
        "net" => crate::net::netd::net_status(),
        "arp" => crate::net::netd::arp_dump(),
        "ping" => crate::net::netd::ping_shell(rest),
        // v1.5: persistent disk (ahci + read-write fat32)
        "dstat" => cmd_dstat(),
        "dls" => cmd_dls(rest),
        "dcat" => cmd_dcat(rest),
        "dsave" => cmd_dsave(rest),
        "ddel" => cmd_ddel(rest),
        "drun" => cmd_drun(rest),
        // v1.9: the persistent tree is hierarchical now
        "cd" => cmd_cd(rest),
        "pwd" => cmd_pwd(),
        "mkdir" => cmd_mkdir(rest),
        "rmdir" => cmd_rmdir(rest),
        // v2.0: DNS + HTTP fetch (a thin alias over WGET.ELF in ring 3)
        "wget" => cmd_wget(rest),
        // v2.6: DHCP — the address is LEARNED (a thin alias over DHCP.ELF)
        "dhcp" => cmd_dhcp(rest),
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
    for (name, desc) in HELP {
        console::print_color("  ", GLM_GRAY);
        console::print_color(name, GLM_WHITE);
        console::print(" - ");
        console::print(desc);
        console::newline();
    }
}

/// v2.8: the command table moved to module scope — `complete_line` walks
/// the same list for TAB, so `help` and completion can never disagree.
const HELP: &[(&str, &str)] = &[
    ("help", "show this help"),
    ("clear", "clear the screen"),
    ("echo <text>", "print text back"),
    ("uptime", "time since boot (PIT @ 100 Hz)"),
    ("date", "wall clock: cmos rtc + uptime, ntp-adjustable (v2.4)"),
    ("mem", "physical frames + heap statistics"),
    ("paging", "CR3 and PML4 map introspection"),
    ("vmm", "own page-table manager self-test"),
    ("ls [path]", "list FAT32 ramdisk directory"),
    ("cat <file>", "print a file from the ramdisk"),
    ("dstat", "persistent disk: ahci device + fat32 stats (v1.5)"),
    ("dls [path]", "list the DISK (persistent) directory"),
    ("dcat <file>", "print a file from the disk"),
    ("dsave <ram> <disk>", "copy a ramdisk file onto the disk"),
    ("ddel <file>", "delete a file from the disk"),
    ("drun <elf>", "run an ELF loaded FROM THE DISK (persistent)"),
    ("cd <dir>", "change the working directory on the disk (v1.9)"),
    ("pwd", "print the working directory"),
    ("mkdir <dir>", "create a directory on the disk (nested ok)"),
    ("rmdir <dir>", "remove an EMPTY directory"),
    ("wget <url> [out]", "fetch http:// over DNS+TCP, save the body (v2.0)"),
    (
        "dhcp",
        "learn the ip address: rfc2131 discover/offer/request/ack (v2.6; runs at boot too)",
    ),
    ("Ctrl+C", "interrupt the foreground task / pipeline with SIGINT (v2.7)"),
    ("Tab", "complete a command name or a path; double-check candidates (v2.8)"),
    ("Ctrl+D", "eof on an empty line: terminal session exits, console stays (v2.8)"),
    ("run <elf>", "load ELF64 and wait for it (foreground)"),
    ("spawn <elf>", "load ELF64 in the background, keep typing"),
    ("ps", "task table (pid, name, state, cpu)"),
    ("kill [-9|-u] <pid>", "signal a task: TERM (default), KILL (-9), USR1 (-u)"),
    ("ipc", "named channel table (bytes in ring, msg counters)"),
    ("pipe", "pipe table (v1.8): readers/writers, bytes through"),
    (
        "run A | B > f < g",
        "v1.8 pipelines: `|` between programs, `>`/`>>`/`<` files",
    ),
    ("sleep <ms>", "block the shell for a while"),
    ("cpu", "per-cpu state; 'cpu ipi <n>' pings cpu n"),
    ("neofetch", "system summary with logo"),
    (
        "gui",
        "desktop: taskbar, start menu, resizable windows, ring-3 apps (esc exits)",
    ),
    ("mouse", "ps/2 mouse status (packets, resyncs)"),
    ("net", "nic, ip config, link state, irq counters"),
    ("arp", "show the arp cache"),
    ("ping <ip>", "icmp echo x4 (empty = gateway 10.0.2.2)"),
    (
        "term",
        "terminal windows on the desktop (v1.4): run commands in a GUI window",
    ),
    ("tcp", "tcp is exercised by TCPSERV.ELF / TCPCLI.ELF (v1.3)"),
    ("glm", "wisdom of the machine"),
    ("about", "what is GLM OS"),
    ("reboot", "reset the machine (8042)"),
    ("halt", "stop the CPU forever"),
];

/// v2.4: the wall clock — CMOS RTC read at boot plus PIT uptime, with the
/// session-local NTP adjustment from ring 3 (`clock_set`). Without an RTC
/// the command says so instead of pretending: uptime stays the only clock.
fn cmd_date() {
    if !crate::cpu::rtc::have() {
        console::print_color("no rtc: wall clock unavailable (uptime only)\n", GLM_YELLOW);
        return;
    }
    let epoch = crate::cpu::rtc::now_epoch();
    let mut dt = [0u8; 20];
    console::print_args(format_args!(
        "{} UTC (cmos rtc + uptime; settable via NTP.ELF)\n",
        crate::cpu::rtc::fmt_datetime(epoch, &mut dt)
    ));
}

fn cmd_mouse() {
    console::print_color("ps/2 mouse (irq12, port 2):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  online: {} | packets: {} | resyncs: {} | buttons: {:#b}\n",
        crate::cpu::mouse::online(),
        crate::cpu::mouse::packets(),
        crate::cpu::mouse::resyncs(),
        crate::cpu::mouse::buttons()
    ));
    crate::klog!(
        "mouse: online={} packets={} resyncs={} buttons={:#b}",
        crate::cpu::mouse::online(),
        crate::cpu::mouse::packets(),
        crate::cpu::mouse::resyncs(),
        crate::cpu::mouse::buttons()
    );
}

fn cmd_about() {
    console::print_color("GLM OS v2.9.0\n", GLM_CYAN);
    console::print("  a 64-bit hobby operating system for x86_64\n");
    console::print("  designed, written and tested by GLM (Z.ai)\n");
    console::print("  kernel: pure Rust, no_std, zero runtime dependencies\n");
    console::print("  boot:   Limine 12 (long mode entry), framebuffer console\n");
    console::print("  memory: own 4-level page tables, per-task address spaces\n");
    console::print("  user:   ring 3, ELF64 loader, int 0x80 syscall gate\n");
    console::print("  sched:  preemptive round-robin, per-cpu LAPIC timer\n");
    console::print("  smp:    limine mp bringup, per-cpu gdt/tss, pinned kidles, ipi\n");
    console::print("  ipc:    signals (sigaction/frame surgery/sigreturn) + byte channels\n");
    console::print("  gui:    double buffered compositor, resize grips, int 0x80 window API\n");
    console::print("  stack:  own GDT/IDT/TSS, 8259 PIC + LAPIC, PIT, PS/2 keyboard\n");
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

fn cmd_vmm() {
    console::print_color("vmm: own page-table manager\n", GLM_CYAN);
    console::print_args(format_args!(
        "  kernel cr3 = {:#x} | hhdm offset = {:#x}\n",
        crate::mem::vmm::kernel_cr3(),
        crate::mem::paging::hhdm_offset()
    ));
    console::print_args(format_args!(
        "  user image base = {:#x} | user stack top = {:#x}\n",
        crate::mem::vmm::USER_IMG_BASE,
        crate::mem::vmm::USER_STACK_TOP
    ));
    let (forks, marked, faults) = crate::mem::vmm::cow_stats();
    console::print_args(format_args!(
        "  cow: {} fork(s), last marked {} pages, {} write faults resolved\n",
        forks, marked, faults
    ));
    console::print_args(format_args!(
        "  frames shared >1 space right now: {}\n",
        crate::mem::frames::shared_count()
    ));
    console::print("  self-test:\n");
    let ok = crate::mem::vmm::self_test();
    if ok {
        console::print_color("  result: PASS\n", GLM_GREEN);
    } else {
        console::print_color("  result: FAIL\n", GLM_RED);
    }
}

/// Resolve a program path: bare names are looked up in /BIN (FAT32 names
/// are upper case, so 'run hello' finds /BIN/HELLO.ELF).
fn resolve_prog(path: &str) -> alloc::string::String {
    if path.contains('/') {
        path.into()
    } else {
        let upper = path.to_ascii_uppercase();
        let mut fat = crate::fs::fat32::FAT.lock();
        let mut try_full = |name: &alloc::string::String| {
            let cand = alloc::format!("/BIN/{}", name);
            if fat.as_mut().map(|f| f.exists(&cand)).unwrap_or(false) {
                Some(cand)
            } else {
                None
            }
        };
        // 1) exact upper name; 2) with .ELF appended if no extension
        try_full(&upper)
            .or_else(|| {
                if upper.contains('.') {
                    None
                } else {
                    let with_ext = alloc::format!("{}.ELF", upper);
                    try_full(&with_ext)
                }
            })
            .unwrap_or_else(|| path.into())
    }
}

/// v1.7: split "PROG.ELF arg1 arg2 ..." into (prog, [arg1, arg2]).
fn split_prog_args(rest: &str) -> (alloc::string::String, alloc::vec::Vec<&str>) {
    let mut it = rest.split_whitespace();
    let prog = alloc::string::String::from(it.next().unwrap_or(""));
    (prog, it.collect())
}

/// argv for a spawn: [resolved path, user args...] — argv[0] is the
/// classic program name; the path form makes it self-identifying.
fn build_argv<'a>(full: &'a str, args: &[&'a str]) -> alloc::vec::Vec<&'a str> {
    let mut v: alloc::vec::Vec<&'a str> = alloc::vec::Vec::with_capacity(args.len() + 1);
    v.push(full);
    v.extend_from_slice(args);
    v
}

/// v2.7: wait for the foreground children with the Ctrl+C registration
/// active: while the shell parks in SYS_WAIT, jobs::jobd translates a
/// console / terminal-window 0x03 into SIGINT to every pid in `pids`
/// (a whole pipeline dies, exactly like a Unix process group). Report
/// order matches the old loop: last stage first.
fn fg_wait_all(pids: &[u64]) -> alloc::vec::Vec<(u64, i64)> {
    crate::jobs::fg_begin(
        crate::sched::current_pid(),
        crate::sched::current_out_win(),
        pids,
    );
    let mut report: alloc::vec::Vec<(u64, i64)> = alloc::vec::Vec::new();
    for pid in pids.iter().rev() {
        let code = crate::user::task::wait_for_child(*pid);
        report.push((*pid, code));
    }
    // v2.7: print queued "[ sig ]" reports (they were queued for this
    // command's dying children) BEFORE the shell's own exit reports, so
    // the terminal shows the kill line first, the exit summary second
    crate::jobs::drain_sig_reports_pub();
    crate::jobs::fg_end(crate::sched::current_pid());
    report
}

fn cmd_run(rest: &str) {
    let (path, args) = split_prog_args(rest);
    if path.is_empty() {
        console::print_color("usage: run <elf> [args...]  (try 'ls /BIN')\n", GLM_YELLOW);
        return;
    }
    console::newline();
    let full = resolve_prog(&path);
    let argv = build_argv(&full, &args);
    match crate::user::task::spawn_user_elf(&full, &argv) {
        Ok(pid) => {
            // foreground: block the shell until the child exits.
            // v1.4: NO keyboard drain here — typed-ahead input is the
            // user's next command, do not swallow it.
            // v2.7: the wait runs under the fg registration — Ctrl+C
            // interrupts the child (jobd -> SIGINT).
            let report = fg_wait_all(&[pid]);
            let code = report[0].1;
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("run ", GLM_CYAN);
            console::print_color(" ] ", GLM_GRAY);
            console::print_args(format_args!(
                "task {} exited with code {} ({})\n",
                pid,
                code,
                crate::user::task::describe_exit(code)
            ));
        }
        Err(e) => {
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("run ", GLM_YELLOW);
            console::print_color(" ] ", GLM_GRAY);
            console::print_color(e, GLM_YELLOW);
            console::newline();
        }
    }
}

/// v2.0: `wget <url> [out]` — the shell keyword just re-points at the
/// WGET.ELF ring-3 program (dns + http + file syscalls, zero kernel
/// HTTP code), so the fetch runs in the same process model as 'run'.
fn cmd_wget(rest: &str) {
    if rest.trim().is_empty() {
        console::print_color("usage: wget http://host[:port][/path] [OUT.HTM]\n", GLM_YELLOW);
        return;
    }
    let line = alloc::format!("WGET.ELF {}", rest);
    cmd_run(&line);
}

/// v2.6: `dhcp` — re-point at the DHCP.ELF ring-3 program (RFC 2131 over
/// the UDP socket syscalls; the only kernel part is SYS_NET_SETCONF).
fn cmd_dhcp(_rest: &str) {
    cmd_run("DHCP.ELF");
}

/// v2.8: boot-time DHCP. Right before the first prompt the shell spawns
/// the ring-3 client in the background: the machine configures itself the
/// way a real OS does — no typed command required. No NIC: nothing to
/// learn, skip cleanly (the no-network boot stays exactly as before).
/// The client itself keeps the previous config when no server answers,
/// so a serverless segment never ends up deconfigured.
fn boot_dhcp() {
    if !crate::net::online() {
        crate::klog!("boot dhcp: no nic, the machine boots offline (nothing to learn)");
        return;
    }
    let full = resolve_prog("DHCP.ELF");
    let argv = build_argv(&full, &[]);
    match crate::user::task::spawn_user_elf(&full, &argv) {
        Ok(pid) => {
            crate::klog!(
                "boot dhcp: client pid {} discovering in the background (the address is being learned)",
                pid
            );
        }
        Err(e) => {
            // not fatal: the boot default config still applies
            crate::klog!("boot dhcp: spawn failed ({}) — keeping the boot default", e);
        }
    }
}

// ---------------------------------------------------------------------------
// v2.8: TAB completion — one table for the console and every terminal window
// ---------------------------------------------------------------------------
//
// The completer returns a new line + caret and, when the match is
// ambiguous, the candidate list lineedit prints under a fresh prompt.
// First token: shell builtins (the HELP table) + .ELF names from /BIN.
// Any later token: a filesystem path — the directory part is resolved
// like any shell path (cwd-relative), entries come from FAT32 ls.

/// What TAB did to the line.
pub struct Completion {
    /// the new full input line (LCP applied; unchanged when no progress)
    pub line: alloc::string::String,
    /// where the caret belongs afterwards (byte offset)
    pub pos: usize,
    /// non-empty ONLY when the match is ambiguous and the candidates
    /// should be listed for the user
    pub candidates: alloc::vec::Vec<alloc::string::String>,
}

/// Print candidate names in fixed columns (4 per row at 16 chars, the
/// classic look), capped so a huge directory cannot flood the screen.
pub fn print_columns(items: &[alloc::string::String]) {
    const PER_ROW: usize = 4;
    const CAP: usize = 32;
    for (i, it) in items.iter().take(CAP).enumerate() {
        if i % PER_ROW == 0 {
            console::print("  ");
        }
        console::print_color(it, GLM_WHITE);
        // pad to the column width
        let w = it.chars().count();
        for _ in w..16 {
            console::print(" ");
        }
        if i % PER_ROW == PER_ROW - 1 || i + 1 == items.len().min(CAP) {
            console::newline();
        }
    }
    if items.len() > CAP {
        console::print_args(format_args!("  ... and {} more\n", items.len() - CAP));
    }
}

/// Complete the word at `pos` in `input`. Case-insensitive on the typed
/// prefix (FAT32 stores uppercase 8.3 names, humans type lowercase); the
/// replacement uses the REAL stored name.
pub fn complete_line(input: &str, pos: usize) -> Completion {
    let bytes = input.as_bytes();
    let pos = pos.min(bytes.len());
    let mut start = pos;
    while start > 0 && bytes[start - 1] != b' ' {
        start -= 1;
    }
    let prefix = &input[start..pos];
    let first_token = input[..start].trim().is_empty();

    // (replacement_word, is_dir) candidates as REAL names
    let mut words: alloc::vec::Vec<(alloc::string::String, bool)> = alloc::vec::Vec::new();

    if first_token {
        // builtins from the help table: real commands are lowercase; the
        // special key entries ("Ctrl+C", "Tab", "Ctrl+D") are skipped —
        // they are documentation, not something you type
        for (name, _) in HELP {
            if name.starts_with('C') || *name == "Tab" {
                continue;
            }
            let bare = name.split([' ', '<']).next().unwrap_or(name);
            if bare.len() >= prefix.len() && bare[..prefix.len()].eq_ignore_ascii_case(prefix) {
                words.push((alloc::string::String::from(bare), false));
            }
        }
        // .ELF names from /BIN (the same store resolve_prog searches)
        list_dir_words("/BIN", prefix, &mut words);
    } else {
        // path completion: split the word into dir + base
        let (dir_part, base) = match prefix.rfind('/') {
            Some(i) => (&prefix[..=i], &prefix[i + 1..]),
            None => ("", prefix),
        };
        let dir_abs = if dir_part.is_empty() {
            // relative to the cwd (no slash typed yet)
            crate::sched::current_cwd()
        } else {
            let trimmed = dir_part.trim_end_matches('/');
            let arg = if trimmed.is_empty() { "/" } else { trimmed };
            resolve_arg(arg)
        };
        let probe = if dir_abs.is_empty() { alloc::string::String::from("/") } else { dir_abs };
        list_dir_words(&probe, base, &mut words);
        // re-attach the typed directory part to every candidate
        if !dir_part.is_empty() {
            for w in words.iter_mut() {
                w.0 = alloc::format!("{}{}", dir_part, w.0);
            }
        }
    }

    if words.is_empty() {
        return Completion {
            line: alloc::string::String::from(input),
            pos,
            candidates: alloc::vec::Vec::new(),
        };
    }

    // longest common prefix LENGTH, case-insensitive (FAT32 stores
    // uppercase 8.3 names, the user types lowercase — a case-sensitive
    // LCP over ["ping","pipe","PING.ELF"] would collapse to "" and even
    // SHRINK the line). The emitted text takes its chars from the first
    // candidate (a real stored name).
    let names: alloc::vec::Vec<&str> = words.iter().map(|w| w.0.as_str()).collect();
    let lcp_len = common_prefix_len(&names);
    // never emit less than the user typed (see above)
    let emit_len = lcp_len.max(prefix.len()).min(names[0].len());
    let mut line = alloc::string::String::from(&input[..start]);
    line.push_str(&names[0][..emit_len]);
    let caret = line.len();
    let mut candidates: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    if words.len() == 1 {
        // unique: finish the word with a space (or a slash for a dir)
        line.push_str(if words[0].1 { "/" } else { " " });
    } else if lcp_len <= prefix.len() {
        // no LCP progress beyond the typed prefix: list the alternatives
        // (bash behavior)
        for w in &words {
            candidates.push(w.0.clone());
        }
    }
    Completion {
        line,
        pos: caret,
        candidates,
    }
}

/// Push every entry of `dir` whose name starts with `base` (case-
/// insensitive) as (real_name, is_dir). BOTH stores are consulted and
/// merged: the RAMDISK FAT (`run` resolves against it) and the persistent
/// DISK (where the cwd, /HOME and everything seeded live). The locks are
/// taken one at a time — never nested.
fn list_dir_words(dir: &str, base: &str, out: &mut alloc::vec::Vec<(alloc::string::String, bool)>) {
    let mut seen: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    let mut push_word = |name: &str, is_dir: bool, seen: &mut alloc::vec::Vec<alloc::string::String>| {
        if name == "." || name == ".." {
            return;
        }
        if name.len() >= base.len() && name[..base.len()].eq_ignore_ascii_case(base) {
            if seen.iter().any(|s| s == name) {
                return;
            }
            seen.push(alloc::string::String::from(name));
            out.push((alloc::string::String::from(name), is_dir));
        }
    };
    {
        let mut fat = crate::fs::fat32::FAT.lock();
        if let Some(fs) = fat.as_mut() {
            if let Ok(entries) = fs.ls(dir) {
                for e in entries {
                    push_word(&e.name, e.is_dir, &mut seen);
                }
            }
        }
    }
    {
        let mut d = crate::fs::fat32::DISK.lock();
        if let Some(fs) = d.as_mut() {
            if let Ok(entries) = fs.ls(dir) {
                for e in entries {
                    push_word(&e.name, e.is_dir, &mut seen);
                }
            }
        }
    }
}

fn common_prefix_len(words: &[&str]) -> usize {
    if words.is_empty() {
        return 0;
    }
    let mut n = words[0].len();
    for w in &words[1..] {
        n = n.min(w.len());
        for i in 0..n {
            if w.as_bytes()[i].to_ascii_lowercase() != words[0].as_bytes()[i].to_ascii_lowercase() {
                n = i;
                break;
            }
        }
    }
    n
}

fn cmd_spawn(rest: &str) {
    let (path, args) = split_prog_args(rest);
    if path.is_empty() {
        console::print_color("usage: spawn <elf> [args...]  (background; try 'ps')\n", GLM_YELLOW);
        return;
    }
    console::newline();
    let full = resolve_prog(&path);
    let argv = build_argv(&full, &args);
    match crate::user::task::spawn_user_elf(&full, &argv) {
        Ok(pid) => {
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("spawn ", GLM_CYAN);
            console::print_color(" ] ", GLM_GRAY);
            console::print_args(format_args!(
                "pid {} runs in the background - shell stays interactive\n",
                pid
            ));
        }
        Err(e) => {
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("spawn ", GLM_YELLOW);
            console::print_color(" ] ", GLM_GRAY);
            console::print_color(e, GLM_YELLOW);
            console::newline();
        }
    }
}

// ---------------------------------------------------------------------------
// v1.8: pipelines & redirection — the Unix way to compose programs
// ---------------------------------------------------------------------------
//
//   "run ECHO.ELF hi | UPPER.ELF | READER.ELF > OUT.TXT"
//   "run READER.ELF < IN.TXT"   "spawn CAT.ELF | READER.ELF"
//
// The shell builds every pipe and file redirect, hands each stage its
// endpoints atomically at spawn time (NewTask.redirect), then closes its
// OWN pipe ends so EOF propagates once all stages are gone. Stage stdout
// that is neither piped nor redirected inherits the session (a terminal
// window keeps receiving the final stage's output).

/// Space-delimited operators only (" | ", " > ", " >> ", " < ") so words
/// like "a>b" are never mistaken for redirection.
fn scan_pipeline(line: &str) -> bool {
    [" | ", " > ", " >> ", " < "].iter().any(|op| line.contains(op))
}

/// A first token that is not a builtin: PROG.ELF-style names start a
/// bare pipeline ("ECHO.ELF hi | READER.ELF").
fn looks_like_prog(tok: &str) -> bool {
    tok.len() >= 5 && tok.to_ascii_uppercase().ends_with(".ELF")
}

const MAX_PIPELINE: usize = 4; // stages per pipeline
const MAX_STAGE_ARGS: usize = 8;

struct Stage {
    prog: alloc::string::String,
    args: alloc::vec::Vec<alloc::string::String>,
    out_file: Option<(alloc::string::String, bool)>, // (name, append)
    in_file: Option<alloc::string::String>,
}

/// Parse one stage: PROG args... [> F | >> F] [< F]. Any order of the
/// operators is accepted; duplicates are rejected at the caller.
fn parse_stage(text: &str) -> Result<Stage, &'static str> {
    let mut it = text.split_whitespace();
    let prog = alloc::string::String::from(it.next().ok_or("empty stage")?);
    let mut stage = Stage {
        prog,
        args: alloc::vec::Vec::new(),
        out_file: None,
        in_file: None,
    };
    while let Some(tok) = it.next() {
        match tok {
            ">" | ">>" => {
                if stage.out_file.is_some() {
                    return Err("duplicate output redirect");
                }
                let f = it.next().ok_or("missing file after output redirect")?;
                stage.out_file = Some((alloc::string::String::from(f), tok == ">>"));
            }
            "<" => {
                if stage.in_file.is_some() {
                    return Err("duplicate input redirect");
                }
                let f = it.next().ok_or("missing file after input redirect")?;
                stage.in_file = Some(alloc::string::String::from(f));
            }
            _ => {
                if stage.args.len() >= MAX_STAGE_ARGS {
                    return Err("too many stage arguments");
                }
                stage.args.push(alloc::string::String::from(tok));
            }
        }
    }
    Ok(stage)
}

/// Resolve + load one stage program: ramdisk first (`run` convention),
/// then the persistent disk (`drun` convention). Returns the ELF bytes
/// and the canonical name for argv[0].
fn load_stage(stage: &Stage) -> Result<(alloc::vec::Vec<u8>, alloc::string::String), &'static str> {
    let full = resolve_prog(&stage.prog);
    if let Ok(bytes) = crate::user::elf::read_from_ramdisk(&full) {
        return Ok((bytes, full));
    }
    // disk candidates: root and /BIN, uppercased (same as cmd_drun)
    let upper = stage.prog.to_ascii_uppercase();
    let candidates: [alloc::string::String; 2] = [
        if upper.contains('/') { upper.clone() } else { alloc::format!("/{}", upper) },
        if upper.contains('/') { upper.clone() } else { alloc::format!("/BIN/{}", upper) },
    ];
    let mut dg = crate::fs::fat32::DISK.lock();
    if let Some(fs) = dg.as_mut() {
        for c in candidates.iter() {
            if let Ok(bytes) = fs.cat(c) {
                return Ok((bytes, c.clone()));
            }
        }
    }
    Err("no such program on ramdisk or disk")
}

fn pipeline_fail(msg: &str) {
    console::print_color("  [ ", GLM_GRAY);
    console::print_color("pipe ", GLM_YELLOW);
    console::print_color(" ] ", GLM_GRAY);
    console::print_color(msg, GLM_YELLOW);
    console::newline();
}

fn cmd_pipeline(keyword: &str, line: &str) {
    let background = keyword == "spawn";
    // strip a leading run/spawn/drun keyword; a bare PROG.ELF start keeps all
    let body = if matches!(keyword, "run" | "spawn" | "drun") {
        line[keyword.len()..].trim()
    } else {
        line
    };
    let stage_strs: alloc::vec::Vec<&str> = body.split(" | ").collect();
    if stage_strs.len() > MAX_PIPELINE {
        pipeline_fail("too many stages (max 4)");
        return;
    }
    console::newline();

    // parse stages; a mid-pipeline `<` conflicts with its incoming pipe
    let mut stages: alloc::vec::Vec<Stage> = alloc::vec::Vec::new();
    for (i, raw) in stage_strs.iter().enumerate() {
        // tolerate "run A | run B": strip a leading run/spawn/drun word
        // from non-first stages
        let mut s = raw.trim();
        if i > 0 {
            for kw in ["run ", "drun ", "spawn "] {
                if let Some(rest) = s.strip_prefix(kw) {
                    s = rest.trim();
                    break;
                }
            }
        }
        let st = match parse_stage(s) {
            Ok(st) => st,
            Err(e) => {
                pipeline_fail(e);
                return;
            }
        };
        if i > 0 && st.in_file.is_some() {
            pipeline_fail("a middle stage cannot also read `< file`");
            return;
        }
        stages.push(st);
    }

    // the file redirects the shell owns (opened now, closed after wait)
    let mut fds: alloc::vec::Vec<(i64, bool)> = alloc::vec::Vec::new(); // (fd, is_output)
    // pipes between stages: (read_handle, write_handle)
    let mut pipe_ends: alloc::vec::Vec<(u64, u64)> = alloc::vec::Vec::new();
    // spawned stages for cleanup on failure
    let mut spawned: alloc::vec::Vec<u64> = alloc::vec::Vec::new();

    // 1) open the connecting pipes first (so failures are cheap)
    for _ in 0..stages.len() - 1 {
        let r = crate::fs::pipe::open();
        if r < 0 {
            cleanup_pipeline(&pipe_ends, &fds, &spawned);
            pipeline_fail("pipe table full");
            return;
        }
        let rh = (r as u64) & 0xFFFF_FFFF;
        let wh = (r as u64) >> 32;
        crate::sched::current_pipes_mask_add(rh);
        crate::sched::current_pipes_mask_add(wh);
        pipe_ends.push((rh, wh));
    }

    // 2) open the shell-owned file redirects
    for (i, st) in stages.iter().enumerate() {
        if i == stages.len() - 1 {
            if let Some((f, append)) = &st.out_file {
                let flags = if *append { crate::fs::sysfile::O_APPEND } else { crate::fs::sysfile::O_CREATE };
                let fd = crate::fs::sysfile::open_name(crate::sched::current_pid(), f, flags);
                if fd < 0 {
                    cleanup_pipeline(&pipe_ends, &fds, &spawned);
                    pipeline_fail("cannot open output file for >");
                    return;
                }
                fds.push((fd, true));
            }
        }
        if i == 0 {
            if let Some(f) = &st.in_file {
                let fd = crate::fs::sysfile::open_name(crate::sched::current_pid(), f, 0);
                if fd < 0 {
                    cleanup_pipeline(&pipe_ends, &fds, &spawned);
                    pipeline_fail("cannot open input file for <");
                    return;
                }
                fds.push((fd, false));
            }
        }
    }

    // 3) spawn every stage with its endpoints riding on NewTask.redirect
    let mut in_fd_used = false;
    let mut out_fd_used = false;
    for (i, st) in stages.iter().enumerate() {
        let out_dst = if i < stages.len() - 1 {
            crate::fs::pipe::redir(crate::fs::pipe::REDIR_PIPE, pipe_ends[i].1)
        } else if st.out_file.is_some() {
            let fd = fds.iter().find(|(_, o)| *o).map(|(fd, _)| *fd).unwrap_or(-1);
            out_fd_used = true;
            crate::fs::pipe::redir(crate::fs::pipe::REDIR_FILE, fd as u64)
        } else {
            crate::fs::pipe::REDIR_INHERIT
        };
        let in_src = if i > 0 {
            crate::fs::pipe::redir(crate::fs::pipe::REDIR_PIPE, pipe_ends[i - 1].0)
        } else if st.in_file.is_some() {
            let fd = fds.iter().find(|(_, o)| !*o).map(|(fd, _)| *fd).unwrap_or(-1);
            in_fd_used = true;
            crate::fs::pipe::redir(crate::fs::pipe::REDIR_FILE, fd as u64)
        } else {
            crate::fs::pipe::REDIR_INHERIT
        };
        match load_stage(st) {
            Ok((bytes, name)) => {
                let arg_refs: alloc::vec::Vec<&str> =
                    st.args.iter().map(|a| a.as_str()).collect();
                let argv = build_argv(&name, &arg_refs);
                match crate::user::task::spawn_user_elf_red(
                    &bytes,
                    &name,
                    &argv,
                    Some((out_dst, in_src)),
                ) {
                    Ok(pid) => spawned.push(pid),
                    Err(e) => {
                        cleanup_pipeline(&pipe_ends, &fds, &spawned);
                        pipeline_fail(e);
                        return;
                    }
                }
            }
            Err(e) => {
                cleanup_pipeline(&pipe_ends, &fds, &spawned);
                pipeline_fail(e);
                return;
            }
        }
    }
    let _ = (in_fd_used, out_fd_used);

    // 4) the shell drops its OWN pipe ends: EOF now depends only on the
    // stages themselves (the heart of correct pipeline semantics)
    for (rh, wh) in pipe_ends.iter() {
        crate::sched::current_pipes_mask_del(*rh);
        crate::sched::current_pipes_mask_del(*wh);
        crate::fs::pipe::close(*rh);
        crate::fs::pipe::close(*wh);
    }
    crate::klog!(
        "shell: pipeline '{}' -> stages {:?} ({} pipe(s))",
        body,
        spawned,
        pipe_ends.len()
    );

    if background {
        if !fds.is_empty() {
            // file redirects belong to the shell: a background stage could
            // still be writing when the flush happens, so refuse honestly
            cleanup_pipeline(&[], &fds, &spawned);
            pipeline_fail("background pipelines only support `|`, not file redirects");
            return;
        }
        console::print_color("  [ ", GLM_GRAY);
        console::print_color("pipe ", GLM_CYAN);
        console::print_color(" ] ", GLM_GRAY);
        console::print_args(format_args!(
            "background pipeline: stages {:?} - shell stays interactive\n",
            spawned
        ));
        return;
    }

    // 5) wait for every stage (last first — its fate matters most);
    //    v2.7: all stages are the fg group — Ctrl+C kills the pipeline
    let report = fg_wait_all(&spawned);
    // close (and flush) the shell-owned file redirects now that every
    // writer is gone
    for (fd, _) in fds.iter() {
        crate::fs::sysfile::file_close(crate::sched::current_pid(), *fd as u64);
    }
    for (pid, code) in report {
        console::print_color("  [ ", GLM_GRAY);
        console::print_color("run ", GLM_CYAN);
        console::print_color(" ] ", GLM_GRAY);
        console::print_args(format_args!(
            "stage {} exited with code {} ({})\n",
            pid,
            code,
            crate::user::task::describe_exit(code)
        ));
    }
}

/// Failure path: kill the stages already running, drop the shell's pipe
/// ends and file fds.
fn cleanup_pipeline(
    pipe_ends: &[(u64, u64)],
    fds: &[(i64, bool)],
    spawned: &[u64],
) {
    for pid in spawned {
        // SIGKILL: a stage wedged on a pipe read must not leak
        let _ = crate::sched::send_signal(*pid, 9);
    }
    for (rh, wh) in pipe_ends {
        crate::sched::current_pipes_mask_del(*rh);
        crate::sched::current_pipes_mask_del(*wh);
        crate::fs::pipe::close(*rh);
        crate::fs::pipe::close(*wh);
    }
    for (fd, _) in fds {
        crate::fs::sysfile::file_close(crate::sched::current_pid(), *fd as u64);
    }
}

fn cmd_pipe() {
    console::print_color("kernel pipes (v1.8, 4 KiB ring each):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  {:>4}  {:>7} {:>7} {:>10} {:>10}\n",
        "SLOT", "READERS", "WRITERS", "WRITTEN", "READ"
    ));
    let mut any = false;
    let mut rows: alloc::vec::Vec<(usize, usize, usize, u64, u64)> = alloc::vec::Vec::new();
    crate::fs::pipe::for_each(|slot, r, w, wrote, got| {
        rows.push((slot, r, w, wrote, got));
    });
    for &(slot, r, w, wrote, got) in rows.iter() {
        any = true;
        console::print_args(format_args!(
            "  {:>4}  {:>7} {:>7} {:>10} {:>10}\n",
            slot, r, w, wrote, got
        ));
    }
    if !any {
        console::print("  (no open pipes)\n");
    }
    // serial-visible summary for automated tests
    crate::klog!("pipe: cmd: {} open slot(s)", rows.len());
}

fn cmd_ps() {
    console::print_color("task table (round-robin, per-cpu LAPIC timer preemption):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  switches so far: {}\n",
        crate::sched::switches()
    ));
    console::print_args(format_args!("  {:>4}  {:<12} {:<9} {:<4} {:>5} {:>5} {}\n", "PID", "NAME", "STATE", "CPU", "PPID", "TGID", "PML4"));
    // v1.4: snapshot under SCHED_LOCK, print AFTER the closure returns.
    // With a terminal-window redirect the print path takes GUI_LOCK, and
    // the compositor takes SCHED_LOCK under GUI_LOCK (monitor stats) —
    // printing inside for_each_task would be a textbook ABBA deadlock.
    // Rows keep (prefix, state, color, suffix) so the state stays colored.
    let mut rows: alloc::vec::Vec<(alloc::string::String, &'static str, u8, alloc::string::String)> =
        alloc::vec::Vec::new();
    crate::sched::for_each_task(|t| {
        let color = match t.state {
            crate::sched::State::Running => GLM_GREEN,
            crate::sched::State::Ready => GLM_CYAN,
            crate::sched::State::Sleeping | crate::sched::State::BlockedInput | crate::sched::State::BlockedChan | crate::sched::State::BlockedJoin | crate::sched::State::BlockedSock => GLM_YELLOW,
            crate::sched::State::WaitingChild => GLM_MAGENTA,
            crate::sched::State::Zombie => GLM_RED,
            crate::sched::State::Dead => GLM_GRAY,
        };
        let state = t.state.as_str();
        let cpu_str = if t.state == crate::sched::State::Running && t.on_cpu != 0xFF {
            alloc::format!("{:>4}", t.on_cpu)
        } else {
            alloc::format!("{:>4}", "-")
        };
        let mut suffix = alloc::format!("{}  {:>5}  {:>5}  {:#x}", cpu_str, t.parent, t.tgid, t.pml4);
        if t.state == crate::sched::State::Zombie {
            suffix.push_str(&alloc::format!("  (exit {})", t.exit_code));
        }
        rows.push((
            alloc::format!("  {:>4}  {:<12} ", t.pid, t.name_str()),
            state,
            color,
            suffix,
        ));
    });
    for (pre, state, color, post) in rows {
        console::print(&pre);
        console::print_color(state, color);
        console::print(&post);
        console::print("\n");
    }
}

fn cmd_cpu(rest: &str) {
    use crate::cpu::smp;
    if rest == "ipi" || rest.starts_with("ipi ") {
        let arg = rest.trim_start_matches("ipi").trim();
        let Ok(n) = arg.parse::<usize>() else {
            console::print_color("usage: cpu ipi <cpu index>\n", GLM_YELLOW);
            return;
        };
        let before = smp::ipi_recv(n);
        match smp::send_test_ipi(n) {
            Ok(()) => {
                // give the target cpu a moment to take the interrupt
                let t0 = crate::cpu::pit::ticks();
                while smp::ipi_recv(n) == before && crate::cpu::pit::ticks().saturating_sub(t0) < 20 {
                    core::hint::spin_loop();
                }
                if smp::ipi_recv(n) > before {
                    console::print_color("  [ ", GLM_GRAY);
                    console::print_color(" ok ", GLM_GREEN);
                    console::print_color(" ] ", GLM_GRAY);
                    console::print_args(format_args!("ipi delivered to cpu{} ({} received total)\n", n, smp::ipi_recv(n)));
                } else {
                    console::print_color("cpu ipi: no answer from cpu{}\n", GLM_YELLOW);
                    console::newline();
                }
            }
            Err(e) => {
                console::print_color("cpu ipi: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        }
        return;
    }
    if !rest.is_empty() {
        console::print_color("usage: cpu  |  cpu ipi <n>\n", GLM_YELLOW);
        return;
    }
    console::print_color("cpus (limine mp bringup, per-cpu lapic timer @ 250 Hz):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  {:>3} {:<8} {:<8} {:<12} {:>9} {:>5}\n",
        "CPU", "LAPIC", "STATE", "CURRENT", "SWITCHES", "IPIs"
    ));
    for i in 0..smp::cpu_count().min(8) {
        let online = smp::online_mask() & (1 << i) != 0;
        let (state, color) = if online {
            ("online", GLM_GREEN)
        } else {
            ("offline", GLM_GRAY)
        };
        let cur = if online {
            match crate::sched::task_brief(smp::current_task_idx(i)) {
                Some((pid, name)) => alloc::format!("{}:{}", pid, name),
                None => alloc::format!("-"),
            }
        } else {
            alloc::format!("-")
        };
        console::print_args(format_args!("  {:>3} ", i));
        console::print_args(format_args!("{:<8} ", alloc::format!("{}", smp::lapic_id_of(i))));
        console::print_color(state, color);
        console::print_args(format_args!(" {:<12} {:>9} {:>5}\n", cur, smp::cpu_switches(i), smp::ipi_recv(i)));
    }
}

fn cmd_kill(rest: &str) {
    // parse: kill [-9|-u] <pid>
    let (sig, pid_str) = if let Some(p) = rest.strip_prefix("-9 ") {
        (crate::user::signal::SIGKILL, p)
    } else if let Some(p) = rest.strip_prefix("-u ") {
        (crate::user::signal::SIGUSR1, p)
    } else {
        (crate::user::signal::SIGTERM, rest)
    };
    let Ok(pid) = pid_str.trim().parse::<u64>() else {
        console::print_color("usage: kill [-9|-u] <pid>  (see 'ps')\n", GLM_YELLOW);
        return;
    };
    if pid == crate::sched::current_pid() {
        console::print_color("kill: refusing to signal the shell itself\n", GLM_YELLOW);
        return;
    }
    // zombies are reaped directly; kernel tasks take the old hard-kill
    // path; user tasks get the signal machinery
    let action = match crate::sched::task_state(pid) {
        None => Err("no such task"),
        Some((crate::sched::State::Zombie, _)) => crate::sched::kill(pid),
        Some((_, true)) => crate::sched::send_signal(pid, sig),
        Some((_, false)) => crate::sched::kill(pid),
    };
    match action {
        Ok(msg) => {
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("kill ", GLM_CYAN);
            console::print_color(" ] ", GLM_GRAY);
            console::print_args(format_args!("pid {}: {}\n", pid, msg));
        }
        Err(e) => {
            console::print_color("kill: ", GLM_YELLOW);
            console::print_args(format_args!("pid {}: {}\n", pid, e));
        }
    }
}

fn cmd_ipc() {
    console::print_color("ipc channels (named byte rings; open/send/recv via int 0x80):\n", GLM_CYAN);
    console::print_args(format_args!(
        "  {:>3} {:>5} {:>7} {:>7} {:>7} {:<8} {:<8}\n",
        "ID", "KEY", "BYTES", "SENT", "GOT", "S-WAIT", "R-WAIT"
    ));
    let mut n = 0;
    crate::ipc::for_each(|id, key, bytes, sent, got, sw, rw| {
        console::print_args(format_args!("  {:>3} {:>5} {:>7} {:>7} {:>7} ", id, key, bytes, sent, got));
        if sw {
            console::print_color("yes", GLM_YELLOW);
        } else {
            console::print("-");
        }
        console::print("      ");
        if rw {
            console::print_color("yes", GLM_YELLOW);
        } else {
            console::print("-");
        }
        console::newline();
        n += 1;
    });
    if n == 0 {
        console::print_color("  (no channels open - run PING.ELF / PONG.ELF)\n", GLM_GRAY);
    }
}

fn cmd_sleep(rest: &str) {
    let Ok(ms) = rest.parse::<u64>() else {
        console::print_color("usage: sleep <milliseconds>\n", GLM_YELLOW);
        return;
    };
    console::print_args(format_args!("sleeping {} ms...\n", ms));
    crate::sched::ksyscall(crate::sched::SYS_SLEEP, ms, 0, 0);
    console::print_color("awake (scheduler kept us honest)\n", GLM_GREEN);
}

fn cmd_ls(path: &str) {
    let mut fat = crate::fs::fat32::FAT.lock();
    match fat.as_mut() {
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
    let mut fat = crate::fs::fat32::FAT.lock();
    match fat.as_mut() {
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

// ---------------------------------------------------------------------------
// v1.5: persistent disk commands (ahci + read-write fat32)
// ---------------------------------------------------------------------------

fn disk_not_mounted() {
    console::print_color(
        "disk: no persistent disk mounted (ahci probe found nothing)\n",
        GLM_YELLOW,
    );
}

fn cmd_dstat() {
    console::newline();
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => {
            console::print_color("  disk: ", GLM_GRAY);
            console::print_color("not present", GLM_YELLOW);
            console::print(" - OS runs from the ramdisk only\n");
        }
        Some(fs) => {
            console::print_color("  device: ", GLM_GRAY);
            console::print_color(&fs.device_info(), GLM_CYAN);
            console::newline();
            console::print_args(format_args!(
                "  fs:     fat32, {} MiB, {} files in root, {} writes\n",
                fs.total_bytes() / (1024 * 1024),
                fs.root_file_count(),
                if fs.is_writable() { "read-write" } else { "read-only" }
            ));
            // a tiny liveness probe: read the BPB back
            console::print_color("  status: ", GLM_GRAY);
            match fs.lookup("/") {
                Some(_) => console::print_color("online\n", GLM_GREEN),
                None => console::print_color("unreadable\n", GLM_YELLOW),
            }
            // v1.6: ring-3 open-file table snapshot
            console::print_args(format_args!(
                "  fds:    {} / 16 open slot(s) by ring-3 tasks\n",
                crate::fs::sysfile::open_count()
            ));
            crate::klog!(
                "dstat: {} / 16 open fd slots",
                crate::fs::sysfile::open_count()
            );
        }
    }
}

fn cmd_dls(path: &str) {
    // v1.9: relative paths resolve against the shell's working directory;
    // an empty argument lists the cwd itself
    let p = resolve_arg(path);
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.ls(&p) {
            Ok(entries) => {
                console::print_args(format_args!("listing of DISK:{}\n", p));
                for e in &entries {
                    // v1.9: the "."/".." FAT slots are bookkeeping, hide them
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
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
                console::print_color("dls: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_dcat(path: &str) {
    if path.is_empty() {
        console::print_color("usage: dcat <file>\n", GLM_YELLOW);
        return;
    }
    // v1.9: resolve relative paths against the shell's cwd
    let p = resolve_arg(path);
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.cat(&p) {
            Ok(bytes) => {
                crate::klog!("disk: cat {} ({} bytes)", p, bytes.len());
                console::print_color("\n", GLM_GRAY);
                for chunk in bytes.chunks(256) {
                    let s: alloc::string::String = chunk
                        .iter()
                        .map(|&b| {
                            if b.is_ascii_graphic() || b == b' ' || b == b'\n' || b == b'\t' || b == b'\r' {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    console::print(&s);
                }
                console::newline();
            }
            Err(e) => {
                console::print_color("dcat: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_dsave(rest: &str) {
    let (src, dst) = match rest.split_once(char::is_whitespace) {
        Some((a, b)) => (a.trim(), b.trim()),
        None => ("", ""),
    };
    if src.is_empty() || dst.is_empty() {
        console::print_color("usage: dsave <ramdisk-src> <disk-dst>\n", GLM_YELLOW);
        return;
    }
    // v1.9: the disk destination may be nested/relative — resolve it
    let dst = resolve_arg(dst);
    // read from the ramdisk (errors carry the path context)
    let bytes = {
        let mut fat = crate::fs::fat32::FAT.lock();
        match fat.as_mut() {
            None => {
                console::print_color("dsave: ramdisk not mounted\n", GLM_YELLOW);
                return;
            }
            Some(fs) => match fs.cat(src) {
                Ok(b) => b,
                Err(e) => {
                    console::print_color("dsave: ", GLM_YELLOW);
                    console::print(e);
                    console::print_color(": ", GLM_YELLOW);
                    console::print(src);
                    console::newline();
                    return;
                }
            },
        }
    };
    let n = bytes.len();
    // write onto the disk
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.write_file(&dst, &bytes) {
            Ok(()) => {
                console::print_color("  [ ", GLM_GRAY);
                console::print_color("disk ", GLM_CYAN);
                console::print_color(" ] ", GLM_GRAY);
                console::print_args(format_args!(
                    "saved {} -> DISK:{} ({} bytes, persistent)\n",
                    src, dst, n
                ));
            }
            Err(e) => {
                console::print_color("dsave: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_ddel(path: &str) {
    if path.is_empty() {
        console::print_color("usage: ddel <disk-file>\n", GLM_YELLOW);
        return;
    }
    // v1.9: resolve relative paths against the shell's cwd
    let p = resolve_arg(path);
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.delete(&p) {
            Ok(()) => {
                console::print_args(format_args!("ddel: DISK:{} deleted\n", p));
            }
            Err(e) => {
                console::print_color("ddel: ", GLM_YELLOW);
                console::print(e);
                console::newline();
            }
        },
    }
}

fn cmd_drun(rest: &str) {
    let (path, args) = split_prog_args(rest);
    if path.is_empty() {
        console::print_color("usage: drun <elf-on-disk> [args...]  (try 'dls')\n", GLM_YELLOW);
        return;
    }
    console::newline();
    // v1.7: bare names are looked up in the cwd, the root AND in /BIN/
    // (same convention as `run` on the ramdisk and exec in ring 3)
    // v1.9: every candidate resolves through the shell's working
    // directory first, so `drun TOOLS/X.ELF` works from anywhere
    let mut candidates: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
    if path.contains('/') {
        candidates.push(resolve_arg(&path));
    } else {
        let up = path.to_ascii_uppercase();
        candidates.push(resolve_arg(&up));
        candidates.push(alloc::format!("/{}", up));
        candidates.push(alloc::format!("/BIN/{}", up));
    }
    // read the ELF from the persistent disk (root first, then /BIN/)
    let mut found: Option<(alloc::vec::Vec<u8>, usize)> = None;
    {
        let mut d = crate::fs::fat32::DISK.lock();
        match d.as_mut() {
            None => {
                disk_not_mounted();
                return;
            }
            Some(fs) => {
                for (i, c) in candidates.iter().enumerate() {
                    if let Ok(b) = fs.cat(c) {
                        found = Some((b, i));
                        break;
                    }
                }
            }
        }
    }
    let Some((bytes, hit)) = found else {
        console::print_color("drun: ", GLM_YELLOW);
        console::print("no such file on disk (tried ");
        for (i, c) in candidates.iter().enumerate() {
            if i > 0 {
                console::print(", ");
            }
            console::print(c);
        }
        console::print(")\n");
        return;
    };
    let full = candidates[hit].clone();
    let argv = build_argv(&full, &args);
    match crate::user::task::spawn_user_elf_bytes(&bytes, &full, &argv) {
        Ok(pid) => {
            // v2.7: fg registration — Ctrl+C interrupts a drun child too
            let report = fg_wait_all(&[pid]);
            let code = report[0].1;
            console::print_color("  [ ", GLM_GRAY);
            console::print_color("drun ", GLM_CYAN);
            console::print_color(" ] ", GLM_GRAY);
            console::print_args(format_args!(
                "task {} exited with code {} ({})\n",
                pid,
                code,
                crate::user::task::describe_exit(code)
            ));
        }
        Err(e) => {
            console::print_color("drun: ", GLM_YELLOW);
            console::print(e);
            console::newline();
        }
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

    let bootver = limine_boot_version();
    let ramdisk_note = alloc::format!(
        "{} files",
        crate::fs::fat32::FAT
            .lock()
            .as_mut()
            .map(|f| f.root_file_count())
            .unwrap_or(0)
    );

    let info: [alloc::string::String; 12] = [
        alloc::format!("glm@glm-os"),
        alloc::format!("-----------"),
        alloc::format!("OS:        GLM OS 2.9.0 (x86_64 long mode, SMP)"),
        alloc::format!("Kernel:    glm 2.9.0, pure Rust no_std"),
        alloc::format!("Boot:      Limine {}", bootver),
        alloc::format!("Uptime:    {}", uptime),
        alloc::format!("CPUs:      {} ({} online), LAPIC {} Hz", crate::cpu::smp::cpu_count(), crate::cpu::smp::online_mask().count_ones(), crate::cpu::apic::SCHED_HZ),
        alloc::format!("Sched:     preemptive RR, {} sw", crate::sched::switches()),
        alloc::format!("Userland:  ring 3, ELF64, signals, IPC, COW fork"),
        alloc::format!("GUI:       desktop, taskbar, resizable windows, terminal windows (v1.4)"),
        alloc::format!("Net:       e1000, 10.0.2.15/24, arp+icmp+udp"),
        alloc::format!("Ramdisk:   FAT32, {}", ramdisk_note),
    ];

    for i in 0..12 {
        if i < LOGO.len() {
            console::print_color(LOGO[i], GLM_CYAN);
        } else {
            console::print("                ");
        }
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
