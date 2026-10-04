//! v2.7: Ctrl+C job control — SIGINT to the foreground task(s).
//!
//! Unix semantics on a terminal: the line discipline turns Ctrl+C into
//! SIGINT for the whole foreground process group. GLM OS has exactly one
//! keyboard (one ring) but many consumers: the text shell (console,
//! out_win == 0) and any number of kterm sessions (out_win == window id).
//! "Foreground" here means: the children a shell registered right before
//! parking in SYS_WAIT (run / drun / a foreground pipeline).
//!
//! The delivery is DEFERRED, and that is the whole trick of this module:
//!
//!   * the keyboard IRQ only flips an atomic flag (no locks in interrupt
//!     context — the v0.8 house rule);
//!   * the GUI compositor only flips an atomic flag when it routes 0x03
//!     into a terminal window's input queue;
//!   * `jobd`, a tiny kernel task, wakes every 40 ms, picks the flags up
//!     and does the real `sched::send_signal(SIGINT)` calls from ordinary
//!     task context, where taking SCHED_LOCK is legal.
//!
//! jobd never echoes for terminal windows (the session's own line editor
//! owns that screen), and only echoes "^C" on the console when something
//! was actually interrupted — a ^C at an idle prompt is echoed by the
//! line editor instead, so the two paths never double-print.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::klog;
use crate::sync::Spinlock;

/// Set by the keyboard IRQ when it decoded a Ctrl+C into the ring
/// (console routing; in GUI mode the compositor drains the ring instead).
pub static CONSOLE_INTR: AtomicBool = AtomicBool::new(false);

/// v2.7: when the last SIGINT delivery happened (uptime ms). The 0x03 byte
/// also sits in the input queue the shell/session is about to pop — the
/// line editor uses this timestamp to swallow it silently (jobd already
/// echoed ^C and the report line follows) instead of echoing it twice.
static LAST_DELIV_MS: AtomicU64 = AtomicU64::new(0);

/// Set by the compositor when it routed 0x03 into terminal window `win`'s
/// input queue. 0 = nothing pending (window ids start at 1).
static TERM_INTR: AtomicU32 = AtomicU32::new(0);

pub const SIGINT: u64 = 2;

/// One foreground registration: the children of one shell command
/// (a pipeline is up to MAX_STAGES tasks) plus the console the shell
/// typed the command at.
const MAX_STAGES: usize = 6;
struct FgEntry {
    shell_pid: u64,
    out_win: u32,
    pids: [u64; MAX_STAGES],
    n: usize,
}

const MAX_FG: usize = 8;
static FG_TABLE: Spinlock<[Option<FgEntry>; MAX_FG]> =
    Spinlock::new([None, None, None, None, None, None, None, None]);

// --- IRQ / compositor side (atomic stores only) -----------------------------

/// Keyboard IRQ: a Ctrl+C just entered the console ring.
#[inline]
pub fn note_console_intr() {
    CONSOLE_INTR.store(true, Ordering::Relaxed);
}

/// Compositor: 0x03 was routed into terminal window `win`'s input queue.
#[inline]
pub fn note_term_intr(win: u32) {
    TERM_INTR.store(win, Ordering::Relaxed);
}

// --- shell side (registration around the SYS_WAIT parking) ------------------

/// Register `pids` as the foreground children of `shell_pid`, typed at
/// console `out_win` (0 = text console, N = kterm window). Called by the
/// shell right before its first wait_for_child; a shell never has two
/// concurrent foreground commands, so one entry per shell is enough and
/// a second begin for the same pid just replaces the first.
pub fn fg_begin(shell_pid: u64, out_win: u32, pids: &[u64]) {
    let mut table = FG_TABLE.lock();
    let n = pids.len().min(MAX_STAGES);
    let mut entry = FgEntry {
        shell_pid,
        out_win,
        pids: [0; MAX_STAGES],
        n,
    };
    entry.pids[..n].copy_from_slice(&pids[..n]);
    // one slot per shell: replace an existing entry, else take a free one
    let slot = table
        .iter()
        .position(|e| matches!(e, Some(e) if e.shell_pid == shell_pid))
        .or_else(|| table.iter().position(|e| e.is_none()));
    if let Some(s) = slot {
        table[s] = Some(entry);
    }
    drop(table);
    klog!(
        "jobs: fg shell {} (win {}) -> {} child{}: {:?}",
        shell_pid,
        out_win,
        n,
        if n == 1 { "" } else { "ren" },
        &pids[..pids.len().min(MAX_STAGES)]
    );
}

/// The wait is over (all children reaped): drop the registration.
pub fn fg_end(shell_pid: u64) {
    let mut table = FG_TABLE.lock();
    for e in table.iter_mut() {
        if matches!(e, Some(e) if e.shell_pid == shell_pid) {
            *e = None;
        }
    }
}

// --- delivery (jobd, task context — locks are legal here) -------------------

/// SIGINT to every foreground registration on `out_win`. Returns how many
/// live tasks were signalled (dead/zombie pids are skipped silently —
/// a stage that already exited does not make the delivery a failure).
fn deliver_to(out_win: u32) -> usize {
    // collect first, send outside the table lock: send_signal takes
    // SCHED_LOCK and the shell path takes FG_TABLE before parking —
    // never the other way round, but collect-then-send keeps the rule
    // trivially one-directional.
    let targets: alloc::vec::Vec<u64> = {
        let table = FG_TABLE.lock();
        let mut v = alloc::vec::Vec::new();
        for e in table.iter().flatten() {
            if e.out_win == out_win {
                v.extend_from_slice(&e.pids[..e.n]);
            }
        }
        v
    };
    let mut sent = 0usize;
    for pid in targets {
        match crate::sched::send_signal(pid, SIGINT) {
            Ok(_) => sent += 1,
            Err(_) => {}
        }
    }
    if sent > 0 {
        klog!(
            "jobs: SIGINT({}) -> {} task{} on win {} (Ctrl+C)",
            SIGINT,
            sent,
            if sent == 1 { "" } else { "s" },
            out_win
        );
    }
    sent
}

/// True when a SIGINT delivery happened within the last 250 ms — the
/// queued 0x03 the editor is about to see is the delivery's own byte.
pub fn recent_delivery() -> bool {
    let now = crate::cpu::pit::uptime_ms();
    now.saturating_sub(LAST_DELIV_MS.load(Ordering::Relaxed)) < 250
}

// --- deferred termination reports -------------------------------------------
//
// The default-action signal delivery runs under SCHED_LOCK inside
// post_dispatch, where neither the per-task console redirect (it can
// route into gui::term_feed -> GUI_LOCK) nor a console print belongs:
// SCHED_LOCK -> GUI_LOCK is forbidden, and a raw console print during
// GUI would paint over the live desktop. The "[ sig ]" report is queued
// here instead; jobd prints it in task context — into the dying task's
// terminal window when it had one, else onto the console (only when the
// console is actually visible).

static SIG_REPORTS: Spinlock<[Option<(u32, u64, u64)>; 8]> =
    Spinlock::new([None, None, None, None, None, None, None, None]);

/// Called from the signal delivery path (SCHED_LOCK held) — spinlock-only,
/// no allocation, never nests back into SCHED.
pub fn queue_sig_report(out_win: u32, pid: u64, sig: u64) {
    let mut q = SIG_REPORTS.lock();
    for s in q.iter_mut() {
        if s.is_none() {
            *s = Some((out_win, pid, sig));
            return;
        }
    }
}

fn drain_sig_reports() {
    let items: alloc::vec::Vec<(u32, u64, u64)> = {
        let mut q = SIG_REPORTS.lock();
        let mut v = alloc::vec::Vec::new();
        for s in q.iter_mut() {
            if let Some(r) = s.take() {
                v.push(r);
            }
        }
        v
    };
    for (out_win, pid, sig) in items {
        let msg = alloc::format!(
            "  [ sig ] task {} terminated by signal {}\n",
            pid, sig
        );
        if out_win != 0 {
            crate::gui::term_feed(out_win, &msg);
        } else if !crate::console::GUI_ACTIVE.load(Ordering::Relaxed) {
            crate::console::print_color("  [ ", crate::console::GLM_GRAY);
            crate::console::print_color("sig", crate::console::GLM_RED);
            crate::console::print_color(" ] ", crate::console::GLM_GRAY);
            crate::console::print(&msg[6..]);
        }
    }
}

/// v2.7: the waiting shell/session calls this right after its last
/// wait_for_child, BEFORE printing its own "[ run ] ... exited" report —
/// so the "[ sig ]" line lands first, like on a real terminal. The
/// caller is an ordinary task (GUI_LOCK is legal here). Drains the WHOLE
/// queue: a foreign report printed by whoever got there first is the
/// same trade real ttys make with their pending-output flush.
pub fn drain_sig_reports_pub() {
    drain_sig_reports();
}

/// One jobd tick: consume both pending flags, then print any queued
/// termination reports (task context — GUI_LOCK is legal here).
pub fn tick() {
    drain_sig_reports();
    if CONSOLE_INTR.swap(false, Ordering::Relaxed) {
        // The ^C echo belongs to whoever actually got interrupted: if no
        // foreground registration matches, the byte stays in the ring for
        // the shell's line editor (idle prompt) or a stdin-reading child —
        // both handle it themselves; jobd stays silent.
        let sent = deliver_to(0);
        if sent > 0 {
            LAST_DELIV_MS.store(crate::cpu::pit::uptime_ms(), Ordering::Relaxed);
            if !crate::console::GUI_ACTIVE.load(Ordering::Relaxed) {
                crate::console::print_color("^C\n", crate::console::GLM_GRAY);
            }
        }
    }
    let win = TERM_INTR.swap(0, Ordering::Relaxed);
    if win != 0 {
        // terminal window: no echo here — the session's line editor owns
        // the display, and the shell report line lands in the window via
        // the redirect.
        let sent = deliver_to(win);
        if sent > 0 {
            LAST_DELIV_MS.store(crate::cpu::pit::uptime_ms(), Ordering::Relaxed);
        }
    }
}

/// The deliverer kernel task: sleep, deliver, repeat.
pub fn jobd_main() -> ! {
    loop {
        crate::sched::ksyscall(crate::sched::SYS_SLEEP, 40, 0, 0);
        tick();
    }
}
