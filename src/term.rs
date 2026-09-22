//! v1.4: kernel-side terminal sessions living in GUI windows.
//!
//! The start menu spawns a `kterm` task per terminal window. The session:
//!   1. opens its window (`gui::term_open_for`) and redirects its own
//!      console output into it (`sched::set_current_out_win`),
//!   2. polls the window's keystroke queue, doing line editing + echo,
//!   3. executes lines through the ordinary shell command table — every
//!      `console::print*` inside lands in the window via the redirect,
//!   4. exits when the window closes ([x] or `exit`), which also closes
//!      the window through the sched exit hook.
//!
//! Programs `run` from the session inherit `out_win`, so their SYS_WRITE
//! output appears in the same window — a real terminal.

use crate::console::{self, GLM_CYAN, GLM_GREEN, GLM_MAGENTA, GLM_WHITE};
use crate::cpu::pit;
use crate::sched;

const LINE_MAX: usize = 128;

/// Terminate the session task: exit_current parks the task for good, but
/// the compiler still needs a diverging tail.
fn die(code: i64) -> ! {
    sched::exit_current(code);
    loop {
        crate::io::ports::hlt();
    }
}

/// Spawn a terminal session task (called from the start-menu handler,
/// GUI_LOCK held — same legal order as the ring-3 demo spawn).
pub fn launch() -> bool {
    sched::spawn(sched::NewTask {
        name: "kterm",
        entry: session_main as *const () as u64,
        user_rsp: None,
        pml4: unsafe { crate::mem::vmm::kernel_cr3() },
        is_user: false,
        user_space: None,
        pinned_cpu: sched::CPU_ANY,
    })
    .is_some()
}

fn prompt() {
    console::print_color("glm", GLM_CYAN);
    console::print_color("> ", GLM_MAGENTA);
}

fn session_main() -> ! {
    let pid = sched::current_pid();
    let Some(win) = crate::gui::term_open_for(pid) else {
        crate::klog!("term: session pid {} could not open a window", pid);
        die(1);
    };
    sched::set_current_out_win(win);
    crate::klog!("term: session pid {} attached to window {}", pid, win);

    console::print_color("GLM OS 1.4 terminal\n", GLM_GREEN);
    console::print("type 'help' for commands, 'exit' closes the window\n");
    prompt();

    let mut line: [u8; LINE_MAX] = [0; LINE_MAX];
    let mut len = 0usize;
    let mut last_caret = pit::ticks();

    loop {
        match crate::gui::term_pop_input(win) {
            Some(b'\n') => {
                console::print("\n");
                crate::shell::execute(&line[..len]);
                len = 0;
                if crate::gui::term_closed(win) {
                    // `exit` closed the window from inside execute()
                    sched::set_current_out_win(0);
                    die(0);
                }
                prompt();
            }
            Some(0x08) => {
                if len > 0 {
                    len -= 1;
                    // the classic backspace-space-backspace erase, fed
                    // straight into the scrollback (0x08 pops cells)
                    console::print("\x08 \x08");
                }
            }
            Some(c) if c.is_ascii_graphic() || c == b' ' => {
                if len < LINE_MAX {
                    line[len] = c;
                    len += 1;
                    let s = [c; 1];
                    console::print_color(core::str::from_utf8(&s).unwrap_or("?"), GLM_WHITE);
                }
            }
            Some(_) => {} // ignore other control bytes
            None => {
                // idle: blink the caret so the window stays alive
                let t = pit::ticks();
                if t.saturating_sub(last_caret) >= 15 {
                    last_caret = t;
                    crate::gui::term_caret_tick(win);
                }
                sched::ksyscall(sched::SYS_SLEEP, 10, 0, 0);
            }
        }
        if crate::gui::term_closed(win) {
            // [x] clicked, or the whole desktop went away
            crate::klog!("term: session pid {} window closed, exiting", pid);
            sched::set_current_out_win(0);
            die(0);
        }
    }
}
