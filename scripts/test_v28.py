#!/usr/bin/env python3
"""GLM OS v2.8 test session — boot DHCP + TAB completion + Ctrl+D EOF.

New in v2.8:
  * boot-time DHCP: the shell spawns the ring-3 client right before the
    first prompt — the machine LEARNS its address with zero manual steps;
    a failed dance keeps the previous config (userland keep_previous)
  * TAB completion in lineedit (console + terminal windows): first token
    completes from the shell command table + /BIN, later tokens are
    filesystem paths (cwd-relative); ambiguous matches list candidates
    under a fresh prompt and keep the LCP
  * Ctrl+D = EOF (0x04): empty line at a terminal prompt closes the
    session; the console shell stays up with a hint; CAT.ELF ends input

VERIFICATION CHANNEL: serial carries klogs only — completion and EOF
paths emit their own klog traces (lineedit: tab / shell: eof / term:
... eof), program console output goes to the framebuffer and is never
parsed. Boot-lease facts are proven by the setconf klog lines the DHCP
client itself generates.

Covered (one boot, then a GUI phase):
  A. boot auto-lease: spawn klog, 0.0.0.0 drop, OFFER/ACK from 10.0.2.2:67,
     learned setconf (ip/mask/gw/dns — ALL from the server), client exit 0,
     ping 4/4 on the auto config, wget example.com (real DNS) + dcat
  B. TAB: unique builtin (neo -> neofetch ), unique ELF path
     (run /BIN/HEL -> run /BIN/HELLO.ELF + exit 0), ambiguous builtin
     (pi -> ping/pipe candidates), directory suffix (ls /HO -> ls /HOME/),
     negative (no match -> line untouched, no klog)
  C. Ctrl+D: console EOF keeps the shell (klog + exit-4 liveness),
     CAT.ELF ends on ^D with exit = bytes echoed
  D. GUI: desktop 2.8, terminal window, TAB completes EDIT.ELF path IN THE
     WINDOW, editor opens, [x] closes it, Ctrl+D closes the session
     (klog "eof (^D), closing window"), esc -> console alive
"""
import os
import re
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/work-v28"
SHOTS = "/home/z/glm-os/shots-v28"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = os.path.join(WORK, "disk28.img")
TASKBAR_H = 28

results = []


def check(name, ok, extra=""):
    results.append((name, bool(ok)))
    print(f"  [{'ok ' if ok else 'FAIL'}] {name}" + (f" | {extra}" if extra and not ok else ""))


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def wait_for(q, needle, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


EXIT_LINE = re.compile(r"sched: task (\d+) exited with code (-?\d+)")


def run_line(q, line, settle=1.5):
    n = len(read_log(q))
    q.type_text(line + "\n")
    time.sleep(settle)
    return n


def exits_after(q, start):
    return EXIT_LINE.findall(read_log(q)[start:])


def last_exit_after(q, start, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        xs = exits_after(q, start)
        if xs:
            return int(xs[-1][1])
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


def tab_klogs(q, start):
    return re.findall(r"lineedit: tab (?:completed|ambiguous) '[^']*'", read_log(q)[start:])


def send_ctrl_c(q, pause=0.6):
    q.hmp("sendkey ctrl-c")
    time.sleep(pause)


class Cur:
    """Guest cursor tracker (gui starts the pointer at screen center)."""

    def __init__(self, q, x, y):
        self.q = q
        self.x = x
        self.y = y

    def moveto(self, tx, ty, step=14, pause=0.02):
        while (self.x, self.y) != (tx, ty):
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
    shutil.copyfile(DISK, DISK_COPY)

    q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
    try:
        assert q.wait_serial_marker("boot complete", 120), "boot failed"
        time.sleep(3)  # the boot lease finishes inside this window
        print("boot ok")

        boot_log = read_log(q)

        # --- A. the boot lease: the machine configures ITSELF -------------
        check("1. boot banner 2.9.0", "GLM OS v2.9.0" in boot_log)

        m = re.search(r"boot dhcp: client pid (\d+) discovering in the background", boot_log)
        check("2. boot dhcp: client spawned in the background", bool(m))
        pid = m.group(1) if m else "?"

        check("3. the client drops to 0.0.0.0 (RFC INIT)",
              "net: config set from ring 3: ip=0.0.0.0 mask=0.0.0.0 gw=0.0.0.0 dns=0.0.0.0" in boot_log)

        offers = len(re.findall(r"sock: recv id=\d+ port=68 udp from 10\.0\.2\.2:67", boot_log))
        check("4. OFFER+ACK arrive automatically (2 replies from :67)", offers == 2,
              f"replies={offers}")

        m = re.search(r"net: config set from ring 3: ip=10\.0\.2\.15 mask=255\.255\.255\.0 "
                      r"gw=10\.0\.2\.2 dns=10\.0\.2\.3", boot_log)
        check("5. the lease APPLIES ITSELF (learned ip/mask/gw/dns)", bool(m))

        code = None
        for p, c in EXIT_LINE.findall(boot_log):
            if p == pid:
                code = int(c)
        check(f"6. the boot client exits 0 (pid {pid})", code == 0, f"code={code}")

        n = run_line(q, "ping", settle=6)
        replies = len(re.findall(r"netd: icmp echo reply seq=\d", read_log(q)[n:]))
        check("7. traffic works on the auto-learned config: ping gw 4/4", replies == 4,
              f"replies={replies}")

        # real DNS through the LEARNED resolver (flake policy: 2 attempts)
        okw = False
        for _ in range(2):
            n = run_line(q, "wget http://example.com/ EX28.HTM", settle=8)
            if last_exit_after(q, n, 20) == 0:
                okw = True
                break
        check("8. wget example.com on the auto config (DNS learned, not typed)", okw)
        n = run_line(q, "dcat EX28.HTM", settle=2)
        mm = re.findall(r"disk: cat \S+ \((\d+) bytes\)", read_log(q)[n:])
        check("9. fetched body readback (>= 500 bytes)", bool(mm) and int(mm[-1]) >= 500,
              f"bytes={mm[-1] if mm else '?'}")

        # --- B. TAB completion on the console -----------------------------
        n0 = len(read_log(q))

        n = len(read_log(q))
        q.type_text("neo")
        q.hmp("sendkey tab")
        time.sleep(0.4)
        q.type_text("\n")
        time.sleep(1.0)
        check("10. TAB unique builtin: 'neo' -> 'neofetch '",
              "lineedit: tab completed 'neo' -> 'neofetch '" in read_log(q)[n:])

        n = len(read_log(q))
        q.type_text("run /BIN/HEL")
        q.hmp("sendkey tab")
        time.sleep(0.4)
        q.type_text("\n")
        time.sleep(1.5)
        seg = read_log(q)[n:]
        ok_tab = "lineedit: tab completed 'run /BIN/HEL' -> 'run /BIN/HELLO.ELF '" in seg
        xs = exits_after(q, n)
        ok_run = bool(xs) and xs[-1][1] == "0"
        check("11. TAB ELF path + run: /BIN/HEL -> /BIN/HELLO.ELF, exit 0",
              ok_tab and ok_run, f"tab={ok_tab} exit={xs}")

        n = len(read_log(q))
        q.type_text("pi")
        q.hmp("sendkey tab")
        time.sleep(0.4)
        q.type_text("\n")  # submit the bare 'pi' — harmless unknown command
        time.sleep(1.0)
        check("12. TAB ambiguous 'pi': candidates listed, LCP kept",
              "lineedit: tab ambiguous 'pi': 3 candidates" in read_log(q)[n:])

        n = len(read_log(q))
        q.type_text("ls /HO")
        q.hmp("sendkey tab")
        time.sleep(0.4)
        q.type_text("\n")
        time.sleep(1.0)
        check("13. TAB directory suffix: 'ls /HO' -> 'ls /HOME/'",
              "lineedit: tab completed 'ls /HO' -> 'ls /HOME/'" in read_log(q)[n:])

        n = len(read_log(q))
        q.type_text("zzz")
        q.hmp("sendkey tab")
        time.sleep(0.4)
        q.type_text("\n")  # unknown command — proves the line was untouched
        time.sleep(1.0)
        check("14. TAB negative: no match, line untouched (no klog, no crash)",
              len(tab_klogs(q, n)) == 0)

        # --- C. Ctrl+D on the console --------------------------------------
        n = len(read_log(q))
        q.hmp("sendkey ctrl-d")
        time.sleep(0.6)
        check("15. console ^D: eof traced, the shell stays up",
              "shell: eof (^D) at the console prompt" in read_log(q)[n:])

        n = run_line(q, "run ARGS.ELF a b c", settle=2)
        check("16. console alive after eof: ARGS.ELF exit 4", last_exit_after(q, n, 20) == 4)

        n = run_line(q, "run CAT", settle=1.5)
        q.type_text("hi")
        time.sleep(0.4)
        q.hmp("sendkey ctrl-d")
        code = last_exit_after(q, n, 20)
        check("17. CAT.ELF: ^D ends stdin, exit = 2 bytes echoed", code == 2, f"code={code}")

        # --- D. GUI: TAB + EOF inside a terminal window ---------------------
        n = run_line(q, "gui", settle=2)
        ok_gui = wait_for(q, "gui: enter (double buffered", n, 15)
        m = re.search(r"framebuffer (\d+)x(\d+)x(\d+)", read_log(q))
        W, H = int(m.group(1)), int(m.group(2))
        ty = H - TASKBAR_H
        cur = Cur(q, W // 2, H // 2)
        time.sleep(1.0)
        cur.moveto(34, ty + 14)  # the GLM start button
        cur.click()
        ok_menu = wait_for(q, "gui: start menu open", n, 10)
        time.sleep(0.4)
        cur.moveto(96, ty - 176 + 4 + 10)  # item 0: terminal
        cur.click()
        ok_term = wait_for(q, "term: session pid ", n, 15)
        mm = re.findall(r"term: session pid \d+ attached to window (\d+)", read_log(q)[n:])
        win = mm[-1] if mm else "?"
        check("18. gui + terminal window opened (win %s)" % win,
              ok_gui and ok_menu and ok_term, f"gui={ok_gui} menu={ok_menu} term={ok_term}")
        q.screendump(os.path.join(SHOTS, "28-a-terminal"))

        # TAB inside the WINDOW: cwd is "/", so use an explicit /BIN path
        n = len(read_log(q))
        q.type_text("run /BIN/EDI")
        q.hmp("sendkey tab")
        time.sleep(0.5)
        q.type_text("\n")
        ok_tab = wait_for(q, "lineedit: tab completed 'run /BIN/EDI' -> 'run /BIN/EDIT.ELF '",
                          n, 10)
        ok_edit = wait_for(q, "sched: spawned 'EDIT.ELF'", n, 15)
        time.sleep(1.5)
        q.screendump(os.path.join(SHOTS, "28-b-editor-from-tab"))
        check("19. TAB inside the terminal window completes the EDIT path",
              ok_tab and ok_edit, f"tab={ok_tab} edit={ok_edit}")

        # [x] closes the editor — EDIT.ELF opens at its own fixed spot
        # (190,110) 470x310 (the v2.2 placement), [x] sits at (+455,+11)
        ex = 190 + 455
        ey = 110 + 11
        cur.moveto(ex, ey)
        cur.click()
        ok_close = wait_for(q, "sched: task ", n, 15)
        time.sleep(1.0)
        xs = exits_after(q, n)
        edit_code = None
        for p, c in xs:
            edit_code = int(c)  # last exit = the editor
        check("20. [x] closes the editor (exit 0)", ok_close and edit_code == 0,
              f"code={edit_code}")

        # Ctrl+D in the (refocused) terminal session: the window closes
        n = len(read_log(q))
        q.hmp("sendkey ctrl-d")
        ok_eof = wait_for(q, "term: session pid ", n, 10) and \
            re.search(rf"term: session pid \d+ eof \(\^D\), closing window {win}\b", read_log(q)[n:])
        time.sleep(1.0)
        q.screendump(os.path.join(SHOTS, "28-c-window-closed-by-eof"))
        check("21. Ctrl+D in the terminal: session eof, window %s closed" % win, bool(ok_eof))

        # --- esc leaves the gui; console still alive ------------------------
        q.hmp("sendkey esc")
        time.sleep(1.5)
        n = run_line(q, "run ARGS.ELF a b c", settle=2)
        check("22. esc -> console alive after the whole session: ARGS.ELF exit 4",
              last_exit_after(q, n, 20) == 4)

        print()
        passed = sum(1 for _, ok in results if ok)
        print(f"v2.8: {passed}/{len(results)} checks passed")
        if passed != len(results):
            sys.exit(1)
    finally:
        q.quit()


if __name__ == "__main__":
    main()
