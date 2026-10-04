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

use crate::console::{self, GLM_GREEN};
use crate::cpu::pit;
use crate::sched;

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
        entry_regs: (0, 0),
        redirect: None,
    })
    .is_some()
}

fn prompt() {
    // v2.8: the prompt body lives in the shell (one copy — TAB completion
    // reprints the same thing after listing candidates)
    crate::shell::print_prompt_text();
}

fn session_main() -> ! {
    let pid = sched::current_pid();
    let Some(win) = crate::gui::term_open_for(pid) else {
        crate::klog!("term: session pid {} could not open a window", pid);
        die(1);
    };
    sched::set_current_out_win(win);
    crate::klog!("term: session pid {} attached to window {}", pid, win);

    console::print_color("GLM OS 2.8 terminal\n", GLM_GREEN);
    console::print("type 'help' for commands, 'exit' or Ctrl+D closes the window\n");
    prompt();

    // v2.5: the shared line editor — arrows walk history, Home/End/Delete
    // and mid-line editing work; display goes through the window's
    // caret-offset redraw (this session's out_win is set above)
    let mut ed = crate::lineedit::LineEdit::new();
    let mut out = [0u8; crate::lineedit::LINE_CAP];
    let mut last_caret = pit::ticks();

    loop {
        match crate::gui::term_pop_input(win) {
            Some(c) => match ed.feed(c, &mut out) {
                crate::lineedit::Fed::Line(n) => {
                    console::print("\n");
                    crate::shell::execute(&out[..n]);
                    if crate::gui::term_closed(win) {
                        // `exit` closed the window from inside execute()
                        sched::set_current_out_win(0);
                        die(0);
                    }
                    prompt();
                }
                crate::lineedit::Fed::Eof => {
                    // v2.8: Ctrl+D on an empty line — the classic logout;
                    // a terminal session has nothing to keep alive, so it
                    // closes its window exactly like `exit` does
                    console::print("\n");
                    console::print_color("bye\n", GLM_GREEN);
                    crate::klog!("term: session pid {} eof (^D), closing window {}", pid, win);
                    crate::gui::term_close(win);
                    sched::set_current_out_win(0);
                    die(0);
                }
                crate::lineedit::Fed::None => {}
            },
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
