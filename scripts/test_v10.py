#!/usr/bin/env python3
"""GLM OS v1.0 GUI test session -> deliverable screenshots.

Сценарий (smp 4 + e1000, одна сессия):
  01 boot + init мыши (klog-маркер)
  02 mouse: online=true packets=0 (до GUI)
  03 gui: рабочий стол + окно + курсор в центре
  04 mouse_move x2: курсор сместился (pixel-diff vs 03)
  05 drag за титлбар: окно сместилось (pixel-diff vs 04)
  06 клик [close]: gui exit reason=close-btn, клики/драги из klog
  07 текстовая консоль восстановлена (redraw_all)
  08 mouse после GUI: packets > 0
  09 регрессии: FORKTEST, THREADTEST, UDP-пара, ping
"""
import os
import re
import sys
import time

from PIL import Image, ImageChops

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v10"


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def log_len(q):
    try:
        return len(open(q.serial_log, errors="replace").read())
    except FileNotFoundError:
        return 0


def wait_new_marker(q, marker, start, timeout=40):
    """Wait for a marker that appears in the log AFTER offset `start`
    (older identical lines must not satisfy the wait)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def diff_stats(a, b):
    ia = Image.open(a).convert("RGB")
    ib = Image.open(b).convert("RGB")
    if ia.size != ib.size:
        return (-1, None)
    d = ImageChops.difference(ia, ib).convert("L")
    hist = d.histogram()
    changed = sum(hist[10:])  # pixels noticeably different
    return (changed, d.getbbox())


def main():
    # self-contained environment: sysroot QEMU + its shared libs
    os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
        "/home/z/sysroot/lib/x86_64-linux-gnu:"
        + os.environ.get("LD_LIBRARY_PATH", "")
    )
    os.makedirs(SHOTS, exist_ok=True)
    qemu = QemuSession(ISO, SHOTS, smp="4", nic="user,model=e1000")
    rc = 0
    try:
        # 01 boot
        assert qemu.wait_serial_marker("boot complete", 90), "boot never completed"
        log = read_log(qemu)
        assert "mouse: ps/2 aux port 2" in log, "mouse init line missing"
        m = re.search(r"framebuffer (\d+)x(\d+)x(\d+)", log)
        assert m, "framebuffer size not found in klog"
        W, H = int(m.group(1)), int(m.group(2))
        print(f"[ok] boot, mouse init ok, framebuffer {W}x{H}")

        WIN_W, WIN_H, TASKBAR_H = 380, 244, 24
        win_x = (W - WIN_W) // 2
        win_y = max(0, (H - TASKBAR_H - WIN_H) // 2 - 24)

        time.sleep(1.0)
        qemu.screendump("01-boot")

        # 02 mouse status before gui
        qemu.type_text("mouse\n")
        assert qemu.wait_serial_marker("mouse: online=true", 15), "mouse cmd marker"
        time.sleep(0.5)
        qemu.screendump("02-mouse-cmd")
        assert re.search(r"mouse: online=true packets=0 ", read_log(qemu)), "expected 0 packets before gui"
        print("[ok] mouse online, 0 packets before gui")

        # 03 enter gui
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter", 15), "gui never entered"
        time.sleep(1.5)  # first stats tick + settle
        shot3 = qemu.screendump("03-gui-desktop")

        # 04 move the cursor
        qemu.hmp("mouse_move 150 0")
        time.sleep(0.4)
        qemu.hmp("mouse_move 0 120")
        time.sleep(0.5)
        shot4 = qemu.screendump("04-gui-cursor-moved")
        changed, bbox = diff_stats(shot3, shot4)
        assert changed > 500, f"cursor move produced no visible change ({changed})"
        print(f"[ok] cursor moved: {changed} px changed, bbox={bbox}")
        # cursor is now at (cx, cy) = center + (150, 120)
        cx, cy = W // 2 + 150, H // 2 + 120

        # 05 move onto the title bar, then drag the window
        tx, ty = win_x + 150, win_y + 11
        qemu.hmp(f"mouse_move {tx - cx} {ty - cy}")
        time.sleep(0.5)
        qemu.hmp("mouse_button 1")
        time.sleep(0.2)
        qemu.hmp("mouse_move 60 40")
        time.sleep(0.35)
        qemu.hmp("mouse_move 40 20")
        time.sleep(0.35)
        qemu.hmp("mouse_button 0")
        time.sleep(0.5)
        shot5 = qemu.screendump("05-gui-dragged")
        changed, bbox = diff_stats(shot4, shot5)
        assert changed > 3000, f"window drag produced no visible change ({changed})"
        print(f"[ok] window dragged: {changed} px changed")

        # 06 click [close]
        bx = win_x + 100 + WIN_W - 88 + 36
        by = win_y + 60 + WIN_H - 28 + 9
        qemu.hmp(f"mouse_move {bx - (tx + 100)} {by - (ty + 60)}")
        time.sleep(0.4)
        qemu.hmp("mouse_button 1")
        time.sleep(0.15)
        qemu.hmp("mouse_button 0")
        assert qemu.wait_serial_marker("gui: exit reason=close-btn", 20), "close button did not exit gui"
        line = [l for l in read_log(qemu).splitlines() if "gui: exit" in l][-1]
        clicks = int(re.search(r"clicks=(\d+)", line).group(1))
        drags = int(re.search(r"drags=(\d+)", line).group(1))
        assert clicks >= 2, f"expected >=2 clicks, got {clicks}"
        assert drags >= 1, f"expected >=1 drag frame, got {drags}"
        print(f"[ok] gui exited via close button ({line.strip()[:110]})")

        time.sleep(0.8)
        qemu.screendump("06-after-gui-text-back")

        # 07 mouse counters after the session
        qemu.type_text("mouse\n")
        deadline = time.time() + 10
        pkts_line = None
        while time.time() < deadline:
            lines = [l for l in read_log(qemu).splitlines() if "mouse: online=true" in l]
            if len(lines) >= 2:
                pkts_line = lines[-1]
                break
            time.sleep(0.2)
        assert pkts_line, "second mouse cmd marker missing"
        pkts = int(re.search(r"packets=(\d+)", pkts_line).group(1))
        assert pkts > 0, "no packets after gui session"
        print(f"[ok] mouse packets after session: {pkts}")
        time.sleep(0.5)

        # 08 regressions
        qemu.type_text("run FORKTEST.ELF\n")
        assert qemu.wait_serial_marker("exited with code 42", 40), "forktest child failed"
        qemu.wait_serial_marker("exited with code 0", 20)
        print("[ok] forktest regression")

        qemu.type_text("spawn THREADTEST.ELF\n")
        n_thread = log_len(qemu)
        assert qemu.wait_serial_marker("exited with code 101", 40), "thread worker 101"
        assert qemu.wait_serial_marker("exited with code 102", 20), "thread worker 102"
        assert wait_new_marker(qemu, "exited with code 0", n_thread), "threadtest main 0"
        print("[ok] threadtest regression")

        qemu.type_text("spawn UDPSERV.ELF\n")
        assert qemu.wait_serial_marker("sock: bound id=", 20), "udp server did not bind"
        n_udp = log_len(qemu)
        qemu.type_text("run UDPCLI.ELF\n")
        assert wait_new_marker(qemu, "exited with code 0", n_udp), "udpcli failed"
        print("[ok] userland udp regression")

        qemu.type_text("ping\n")
        deadline = time.time() + 25
        while read_log(qemu).count("icmp echo reply seq=") < 4 and time.time() < deadline:
            time.sleep(0.2)
        assert read_log(qemu).count("icmp echo reply seq=") >= 4, "ping did not get 4 replies"
        print("[ok] ping 4/4")

        qemu.type_text("neofetch\n")
        time.sleep(1.2)
        qemu.screendump("07-neofetch-final")

        qemu.type_text("ps\n")
        time.sleep(0.6)
        qemu.screendump("08-final-ps")

        time.sleep(1.5)
        os.replace(qemu.serial_log, os.path.join(SHOTS, "kernel.log"))
        print("\nALL v1.0 CHECKS PASSED")
    except AssertionError as e:
        print(f"FAIL: {e}")
        qemu.screendump("FAIL-state")
        rc = 1
    finally:
        qemu.proc.terminate()
        try:
            qemu.proc.wait(timeout=5)
        except Exception:
            qemu.proc.kill()
    sys.exit(rc)


if __name__ == "__main__":
    main()
