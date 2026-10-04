#!/usr/bin/env python3
"""GLM OS v2.7 test session — Ctrl+C job control.

New in v2.7:
  * keyboard: the Ctrl modifier decodes (0x1D); Ctrl+C emits 0x03 (the
    terminal interrupt byte) + an IRQ-side atomic flag
  * kernel: src/jobs.rs — the "foreground" is a registration: the shell
    wraps its SYS_WAIT parking in jobs::fg_begin/fg_end with every child
    pid (a whole pipeline) and its console (0 = text console, N = kterm
    window). jobd (a new kernel task) wakes every 40 ms and turns the
    IRQ/compositor flags into sched::send_signal(SIGINT) calls — task
    context, no locks in IRQ
  * signals: SIGINT(2) exists and is catchable; default action = terminate
    with 130 = 128+2 (Linux-style), describe_exit says "interrupted"
  * lineedit: ^C at the prompt discards the line (echoes ^C, submits an
    empty line, history untouched)

Covered (all in one boot):
  A. console: run LOOP.ELF foreground -> Ctrl+C -> fg registration klog ->
     delivery klog (win 0) -> exit code 130 -> shell still interactive
  B. console: foreground pipeline of two SLEEPY stages -> Ctrl+C -> BOTH
     stages signalled ("-> 2 tasks") -> two exit-130 lines
  C. console: ^C at the prompt discards the typed line (the line was
     never executed: its exit code is absent from the log), next command
     runs; no delivery klog for a prompt ^C (nothing was interrupted)
  D. console: a spawn'ed (background) LOOP task SURVIVES a Ctrl+C, and is
     kill -9-able afterwards (exit 137) — job control is fg-only
  E. console: INTR.ELF catches SIGINT in a ring-3 handler and exits 0
     (the catchable path: frame surgery + sigreturn on the v2.7 delivery)
  F. gui: terminal window -> run LOOP.ELF in it -> Ctrl+C -> delivery
     klog (win W) -> exit 130 -> the session prompt survives (`exit`
     closes the window cleanly)
  G. esc leaves the gui; the console shell still works (exit 4 check)
"""
import os
import re
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/work-v27"
SHOTS = "/home/z/glm-os/shots-v27"
TASKBAR_H = 28
MENU_H = 176  # panel top offset from the taskbar (8 items: 172 + 4 gap)

results = []


def check(name, ok, extra=""):
    results.append((name, bool(ok)))
    print(f"  [{'ok ' if ok else 'FAIL'}] {name}" + (f" | {extra}" if extra and not ok else ""))


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def wait_for(q, needle, start, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


EXIT_LINE = re.compile(r"sched: task (\d+) exited with code (-?\d+)")
SIG_LINE = re.compile(r"sched: task (\d+) terminated by signal (\d+)")
SPAWN_PID = re.compile(r"user: pid (\d+) ready: entry")


def run_line(q, line, settle=1.5):
    n = len(read_log(q))
    q.type_text(line + "\n")
    time.sleep(settle)
    return n


def last_exit_after(q, start, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        ms = EXIT_LINE.findall(read_log(q)[start:])
        if ms:
            return int(ms[-1][1])
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


def exits_after(q, start):
    return [(int(a), int(b)) for a, b in EXIT_LINE.findall(read_log(q)[start:])]


def sigs_after(q, start):
    return [(int(a), int(b)) for a, b in SIG_LINE.findall(read_log(q)[start:])]


def send_ctrl_c(q, pause=0.6):
    q.hmp("sendkey ctrl-c")
    time.sleep(pause)


class Cur:
    """Guest cursor tracker (gui starts the pointer at screen center)."""

    def __init__(self, q, x, y):
        self.q, self.x, self.y = q, x, y

    def moveto(self, tx, ty, step=110, pause=0.13):
        while self.x != tx or self.y != ty:
            dx = max(-step, min(step, tx - self.x))
            dy = max(-step, min(step, ty - self.y))
            if dx == 0 and dy == 0:
                break
            self.q.hmp(f"mouse_move {dx} {dy}")
            self.x += dx
            self.y += dy
            time.sleep(pause)
        time.sleep(0.25)

    def click(self):
        self.q.hmp("mouse_button 1")
        time.sleep(0.18)
        self.q.hmp("mouse_button 0")
        time.sleep(0.3)


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    shutil.rmtree(SHOTS, ignore_errors=True)
    os.makedirs(SHOTS, exist_ok=True)

    q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000")
    try:
        assert q.wait_serial_marker("boot complete", 120), "boot failed"
        print("boot ok")
        time.sleep(1)

        # 1. banner
        check("1. boot banner 2.8.0", "GLM OS v2.8.0" in read_log(q))

        # --- A. foreground run + Ctrl+C -----------------------------------
        n = run_line(q, "run LOOP.ELF", settle=2)
        ok_reg = wait_for(q, "jobs: fg shell", n, 15)
        reg_seg = read_log(q)[n:]
        m = re.search(r"jobs: fg shell (\d+) \(win 0\) -> 1 child(?:ren)?: \[(\d+)\]", reg_seg)
        check("2. fg registration on the console (win 0)", ok_reg and bool(m),
              reg_seg[-200:] if not m else "")
        send_ctrl_c(q)
        ok_del = wait_for(q, "jobs: SIGINT(2) -> 1 task on win 0 (Ctrl+C)", n, 15)
        deadline = time.time() + 15
        sig = None
        while time.time() < deadline:
            ss = sigs_after(q, n)
            if ss:
                sig = ss[-1][1]
                break
            time.sleep(0.2)
        check("3. Ctrl+C: delivery klog + task terminated by SIGINT",
              ok_del and sig == 2, f"sig={sig}")
        q.screendump(os.path.join(SHOTS, "27-a-after-interrupt"))

        # shell still interactive
        n = run_line(q, "run ARGS.ELF a b c")
        code = last_exit_after(q, n)
        check("4. shell alive after the interrupt (exit 4)", code == 4, f"code={code}")

        # --- B. pipeline: both stages interrupted -------------------------
        n = run_line(q, "run SLEEPY.ELF 30000 | run SLEEPY.ELF 30000", settle=2)
        ok_reg = wait_for(q, "-> 2 children", n, 15)
        send_ctrl_c(q)
        ok_del = wait_for(q, "jobs: SIGINT(2) -> 2 tasks on win 0 (Ctrl+C)", n, 15)
        deadline = time.time() + 20
        sigs = []
        while time.time() < deadline:
            sigs = [s for _, s in sigs_after(q, n)]
            if sigs.count(2) >= 2:
                break
            time.sleep(0.3)
        check("5. pipeline Ctrl+C: both stages terminated by signal 2",
              ok_reg and ok_del and sigs.count(2) >= 2, f"sigs={sigs}")

        # --- C. ^C at the prompt discards the line ------------------------
        n = len(read_log(q))
        q.type_text("run ARGS.ELF a b c d")  # NO enter
        time.sleep(1.0)
        send_ctrl_c(q, pause=1.0)
        seg = read_log(q)[n:]
        delivered = "jobs: SIGINT" in seg
        q.type_text("\n")  # nothing should be pending, submit empties only
        time.sleep(0.8)
        n2 = len(read_log(q))
        q.type_text("run ARGS.ELF a b c\n")
        code = last_exit_after(q, n2)
        codes2 = [c for _, c in exits_after(q, n)]
        discarded = (5 not in codes2) and (not delivered) and code == 4
        check("6. ^C at the prompt discards the line (no exit 5, no delivery)",
              discarded, f"codes2={codes2} code={code} delivered={delivered}")

        # --- D. background task survives Ctrl+C ---------------------------
        n = run_line(q, "spawn LOOP.ELF", settle=2)
        deadline = time.time() + 10
        pid = None
        while time.time() < deadline:
            mm = SPAWN_PID.findall(read_log(q)[n:])
            if mm:
                pid = int(mm[-1])
                break
            time.sleep(0.2)
        check("7. spawn LOOP.ELF into the background", pid is not None)
        n = len(read_log(q))
        send_ctrl_c(q, pause=1.0)
        seg = read_log(q)[n:]
        check("8. Ctrl+C does not touch background tasks (no delivery)",
              "jobs: SIGINT" not in seg)
        n = run_line(q, f"kill -9 {pid}", settle=2)
        deadline = time.time() + 10
        sig = None
        while time.time() < deadline:
            for tp, s in sigs_after(q, n):
                if tp == pid:
                    sig = s
            if sig is not None:
                break
            time.sleep(0.2)
        check("9. the background task was alive and kill -9-able (signal 9)",
              sig == 9, f"sig={sig}")

        # --- E. INTR.ELF: SIGINT caught by a ring-3 handler ---------------
        n = run_line(q, "run INTR.ELF", settle=2)
        ok_reg = wait_for(q, "jobs: fg shell", n, 15)
        send_ctrl_c(q)
        ok_del = wait_for(q, "jobs: SIGINT(2) -> 1 task on win 0 (Ctrl+C)", n, 15)
        code = last_exit_after(q, n)
        check("10. INTR.ELF catches SIGINT, exits 0 (not 130)",
              ok_reg and ok_del and code == 0, f"code={code}")
        q.screendump(os.path.join(SHOTS, "27-b-after-intr"))

        # --- F. gui: terminal window Ctrl+C -------------------------------
        n = run_line(q, "gui", settle=2)
        ok_gui = wait_for(q, "gui: enter (double buffered", n, 15)
        log = read_log(q)
        m = re.search(r"framebuffer (\d+)x(\d+)x(\d+)", log)
        W, H = int(m.group(1)), int(m.group(2))
        ty = H - TASKBAR_H
        my = ty - MENU_H
        cur = Cur(q, W // 2, H // 2)
        time.sleep(1.0)
        cur.moveto(34, ty + 14)  # the GLM start button
        cur.click()
        ok_menu = wait_for(q, "gui: start menu open", n, 10)
        time.sleep(0.4)
        q.screendump(os.path.join(SHOTS, "27-c-start-menu"))
        cur.moveto(96, my + 4 + 10)  # item 0: terminal
        cur.click()
        ok_term = wait_for(q, "term: session pid ", n, 15)
        mm = re.findall(r"term: session pid \d+ attached to window (\d+)", read_log(q)[n:])
        win = mm[-1] if mm else "?"
        check("11. gui + terminal window opened (win %s)" % win,
              ok_gui and ok_menu and ok_term, f"gui={ok_gui} menu={ok_menu} term={ok_term}")
        q.screendump(os.path.join(SHOTS, "27-d-terminal"))
        time.sleep(0.8)

        n = run_line(q, "run LOOP.ELF", settle=2)
        ok_reg = wait_for(q, f"jobs: fg shell", n, 15)
        ok_wreg = wait_for(q, f"(win {win}) -> 1 child", n, 15)
        send_ctrl_c(q)
        ok_del = wait_for(q, f"jobs: SIGINT(2) -> 1 task on win {win} (Ctrl+C)", n, 15)
        deadline = time.time() + 15
        sig = None
        while time.time() < deadline:
            ss = sigs_after(q, n)
            if ss:
                sig = ss[-1][1]
                break
            time.sleep(0.2)
        check("12. Ctrl+C in the terminal window: delivery (win %s) + SIGINT kill" % win,
              ok_reg and ok_wreg and ok_del and sig == 2,
              f"reg={ok_wreg} del={ok_del} sig={sig}")
        q.screendump(os.path.join(SHOTS, "27-e-window-interrupt"))

        # the session survived: `exit` retires it cleanly
        n = len(read_log(q))
        q.type_text("exit\n")
        ok_close = wait_for(q, "gui: terminal window id=%s closed by session" % win, n, 10)
        check("13. terminal session alive after the interrupt (`exit` works)",
              ok_close)

        # --- G. esc leaves the gui, console works -------------------------
        q.hmp("sendkey esc")
        time.sleep(1.5)
        n = run_line(q, "run ARGS.ELF a b c", settle=2)
        code = last_exit_after(q, n)
        check("14. esc left the gui; console shell still interactive (exit 4)",
              code == 4, f"code={code}")
        q.screendump(os.path.join(SHOTS, "27-f-console-restored"))

    finally:
        time.sleep(0.5)
        q.hmp("quit")
        try:
            q.proc.wait(timeout=10)
        except Exception:
            q.proc.kill()

    failed = [name for name, ok in results if not ok]
    print(f"\n==== v2.7: {len(results) - len(failed)}/{len(results)} checks passed")
    if failed:
        print("FAILED:")
        for f in failed:
            print(f"  - {f}")
        sys.exit(1)


if __name__ == "__main__":
    main()
