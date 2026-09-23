//! glmsh — the GLM OS shell.
//!
//! Polls the keyboard ring buffer, maintains the input line, dispatches
//! commands. Runs after interrupts are online; blinks the cursor from
//! PIT ticks without ever touching the console from IRQ context.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::console;
use crate::console::{GLM_CYAN, GLM_GRAY, GLM_GREEN, GLM_MAGENTA, GLM_RED, GLM_WHITE, GLM_YELLOW};
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
    console::print("  Welcome to the GLM OS shell (glmsh 1.6). Type 'help'.");
    console::set_color_global(GLM_GRAY);
    console::newline();
    console::newline();
    prompt();

    let mut last_blink = pit::ticks();
    loop {
        // v1.4: while the GUI owns the screen the compositor routes keys
        // (terminal windows / ring-3 apps); the text shell must not steal
        // keystrokes or echo into the hidden console.
        if crate::console::GUI_ACTIVE.load(Ordering::Relaxed) {
            crate::sched::ksyscall(crate::sched::SYS_SLEEP, 20, 0, 0);
            continue;
        }
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
        ("vmm", "own page-table manager self-test"),
        ("ls [path]", "list FAT32 ramdisk directory"),
        ("cat <file>", "print a file from the ramdisk"),
        ("dstat", "persistent disk: ahci device + fat32 stats (v1.5)"),
        ("dls [path]", "list the DISK (persistent) directory"),
        ("dcat <file>", "print a file from the disk"),
        ("dsave <ram> <disk>", "copy a ramdisk file onto the disk"),
        ("ddel <file>", "delete a file from the disk"),
        ("drun <elf>", "run an ELF loaded FROM THE DISK (persistent)"),
        ("run <elf>", "load ELF64 and wait for it (foreground)"),
        ("spawn <elf>", "load ELF64 in the background, keep typing"),
        ("ps", "task table (pid, name, state, cpu)"),
        ("kill [-9|-u] <pid>", "signal a task: TERM (default), KILL (-9), USR1 (-u)"),
        ("ipc", "named channel table (bytes in ring, msg counters)"),
        ("sleep <ms>", "block the shell for a while"),
        ("cpu", "per-cpu state; 'cpu ipi <n>' pings cpu n"),
        ("neofetch", "system summary with logo"),
        ("gui", "desktop: taskbar, start menu, resizable windows, ring-3 apps (esc exits)"),
        ("mouse", "ps/2 mouse status (packets, resyncs)"),
        ("net", "nic, ip config, link state, irq counters"),
        ("arp", "show the arp cache"),
        ("ping <ip>", "icmp echo x4 (empty = gateway 10.0.2.2)"),
        ("term", "terminal windows on the desktop (v1.4): run commands in a GUI window"),
        ("tcp", "tcp is exercised by TCPSERV.ELF / TCPCLI.ELF (v1.3)"),
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
    console::print_color("GLM OS v1.7.0\n", GLM_CYAN);
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
            let code = crate::user::task::wait_for_child(pid);
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
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.ls(path) {
            Ok(entries) => {
                console::print_args(format_args!(
                    "listing of DISK:{}\n",
                    if path.is_empty() { "/" } else { path }
                ));
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
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.cat(path) {
            Ok(bytes) => {
                crate::klog!("disk: cat {} ({} bytes)", path, bytes.len());
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
        Some(fs) => match fs.write_file(dst, &bytes) {
            Ok(()) => {
                console::print_color("  [ ", GLM_GRAY);
                console::print_color("disk ", GLM_CYAN);
                console::print_color(" ] ", GLM_GRAY);
                console::print_args(format_args!(
                    "saved {} -> DISK:/{} ({} bytes, persistent)\n",
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
    let mut d = crate::fs::fat32::DISK.lock();
    match d.as_mut() {
        None => disk_not_mounted(),
        Some(fs) => match fs.delete(path) {
            Ok(()) => {
                console::print_args(format_args!("ddel: DISK:/{} deleted\n", path));
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
    // v1.7: bare names are looked up in the root AND in /BIN/ (same
    // convention as `run` on the ramdisk and exec in ring 3)
    let candidates: [alloc::string::String; 2] = [
        if path.contains('/') {
            alloc::format!("/{}", path)
        } else {
            alloc::format!("/{}", path.to_ascii_uppercase())
        },
        if path.contains('/') {
            alloc::format!("/{}", path)
        } else {
            alloc::format!("/BIN/{}", path.to_ascii_uppercase())
        },
    ];
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
            let code = crate::user::task::wait_for_child(pid);
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
        alloc::format!("OS:        GLM OS 1.7.0 (x86_64 long mode, SMP)"),
        alloc::format!("Kernel:    glm 1.7.0, pure Rust no_std"),
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
