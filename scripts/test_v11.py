#!/usr/bin/env python3
"""GLM OS v1.1 GUI test session -> deliverable screenshots.

Сценарий (smp 4 + e1000, одна сессия):
  01 boot + init мыши (klog-маркер)
  02 mouse: online=true packets=0 (до GUI)
  03 gui enter: двойная буферизация, градиент + вотермарка + таскбар + монитор
  04 minimize монитора кнопкой '-': окно исчезло (pixel-diff), klog
  05 клик по кнопке окна в таскбаре: окно восстановлено (pixel-diff)
  06 клик GLM (start): стартовое меню открыто, hover-подсветка
  07 клик 'about glm os': окно about поверх монитора (z-order)
  08 drag about за титлбар: окно сместилось (pixel-diff)
  09 close about 'x': окно исчезло, монитор снова в фокусе
  10 esc: текстовая консоль восстановлена, klog exit reason=esc
  11 mouse packets > 0 после сессии
  12 регрессии: FORKTEST, THREADTEST, UDP-пара, ping
  13 neofetch (1.1.0) + ps
  14 повторный gui -> start -> reboot: QEMU уходит (-no-reboot)
"""
import os
import re
import sys
import time

from PIL import Image, ImageChops

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v11"

MON_W, MON_H = 380, 244
ABOUT_W = 330
TASKBAR_H = 28
START_BTN = (34,)  # x center; y computed from H
TASKBTN0_X = 68 + 66  # first task button center x


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def log_len(q):
    try:
        return len(open(q.serial_log, errors="replace").read())
    except FileNotFoundError:
        return 0


def wait_new_marker(q, marker, start, timeout=40):
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
    changed = sum(hist[10:])
    return (changed, d.getbbox())


def count_px(img, box, pred):
    """Count pixels satisfying pred(r,g,b) inside box=(x0,y0,x1,y1)."""
    px = img.crop(box).getdata()
    return sum(1 for (r, g, b) in px if pred(r, g, b))


def blue_title(img, box):
    """Focused-title-blue pixels (40,96,210) in a region."""
    return count_px(img, box, lambda r, g, b: b > 140 and b - r > 60 and b - g > 40)


def light_px(img, box):
    """Bright pixels (menu text/icons) in a region."""
    return count_px(img, box, lambda r, g, b: 0.299 * r + 0.587 * g + 0.114 * b > 150)


class Cur:
    """Guest-side cursor tracker: gui starts the pointer at screen center."""

    def __init__(self, q, x, y):
        self.q = q
        self.x, self.y = x, y

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
        # a click can terminate the guest (reboot from the start menu),
        # so a broken monitor socket here is not an error
        try:
            self.q.hmp("mouse_button 1")
            time.sleep(0.18)
            self.q.hmp("mouse_button 0")
        except (BrokenPipeError, OSError):
            pass
        time.sleep(0.3)


def main():
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

        ty = H - TASKBAR_H
        mon_x = (W - MON_W) // 2
        mon_y = max(0, (H - TASKBAR_H - MON_H) // 2 - 24)
        my = ty - 92 - 4            # start-menu panel top
        ab_x, ab_y = mon_x + 36, mon_y + 36  # about window cascade position
        cur = Cur(qemu, W // 2, H // 2)

        time.sleep(1.0)
        qemu.screendump("01-boot")

        # 02 mouse status before gui
        qemu.type_text("mouse\n")
        assert qemu.wait_serial_marker("mouse: online=true", 15), "mouse cmd marker"
        assert re.search(r"mouse: online=true packets=0 ", read_log(qemu)), "expected 0 packets"
        print("[ok] mouse online, 0 packets before gui")

        # 03 enter gui (double buffered)
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "gui never entered"
        time.sleep(1.5)  # first stats tick + fps window
        shot3 = qemu.screendump("03-gui-desktop")
        print("[ok] gui entered (double buffered)")

        # 04 minimize the monitor window via its '-' title button
        cur.moveto(mon_x + MON_W - 42 + 9, mon_y + 4 + 7)
        cur.click()
        assert wait_new_marker(qemu, "gui: minimize system monitor window", log_len(qemu) - 2000, 15), \
            "minimize marker missing"
        time.sleep(0.6)
        shot4 = qemu.screendump("04-minimized")
        im3, im4 = Image.open(shot3).convert("RGB"), Image.open(shot4).convert("RGB")
        tbox = (mon_x + 1, mon_y + 1, mon_x + MON_W - 1, mon_y + 22)
        b3, b4 = blue_title(im3, tbox), blue_title(im4, tbox)
        assert b3 > 2000 and b4 < 100, f"minimize: title blue {b3} -> {b4}"
        print(f"[ok] minimized: title-blue in region {b3} -> {b4}")

        # 05 restore via the taskbar task-button
        cur.moveto(TASKBTN0_X, ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: restore system monitor window", log_len(qemu) - 2000, 15), \
            "restore marker missing"
        time.sleep(0.6)
        shot5 = qemu.screendump("05-restored")
        im5 = Image.open(shot5).convert("RGB")
        b5 = blue_title(im5, tbox)
        assert b5 > 2000, f"restore: title blue did not return ({b5})"
        print(f"[ok] restored from taskbar: title-blue back ({b5})")

        # 06 start menu
        cur.moveto(START_BTN[0], ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), \
            "start menu marker missing"
        time.sleep(0.5)
        shot6 = qemu.screendump("06-start-menu")
        im6 = Image.open(shot6).convert("RGB")
        mbox = (6, my, 6 + 180, my + 96)
        l5, l6 = light_px(im5, mbox), light_px(im6, mbox)
        assert l6 > 200 and l6 > l5 * 3, f"start menu: light px {l5} -> {l6}"
        print(f"[ok] start menu open: menu light px {l5} -> {l6}")

        # 07 launch 'about glm os' from the menu (item k=1)
        cur.moveto(6 + 88, my + 4 + 20 + 10)
        cur.click()
        assert wait_new_marker(qemu, "gui: launch about window", log_len(qemu) - 2000, 15), \
            "about launch marker missing"
        time.sleep(0.6)
        shot7 = qemu.screendump("07-about-open")
        im7 = Image.open(shot7).convert("RGB")
        atbox = (ab_x + 1, ab_y + 1, ab_x + ABOUT_W - 1, ab_y + 22)
        ba = blue_title(im7, atbox)
        assert ba > 2000, f"about title not drawn/focused ({ba})"
        print(f"[ok] about window launched, focused (title-blue {ba})")

        # 08 drag the about window by its title bar
        cur.moveto(ab_x + 150, ab_y + 11)
        qemu.hmp("mouse_button 1")
        time.sleep(0.2)
        cur.moveto(ab_x + 150 + 70, ab_y + 11 + 50, pause=0.16)
        qemu.hmp("mouse_button 0")
        time.sleep(0.5)
        shot8 = qemu.screendump("08-about-dragged")
        im8 = Image.open(shot8).convert("RGB")
        ab2_x, ab2_y = ab_x + 70, ab_y + 50
        at2box = (ab2_x + 1, ab2_y + 1, ab2_x + ABOUT_W - 1, ab2_y + 22)
        b_old, b_new = blue_title(im8, atbox), blue_title(im8, at2box)
        assert b_new > 2000 and b_old < 100, f"drag: title blue old={b_old} new={b_new}"
        print(f"[ok] about window dragged: title moved (old={b_old}, new={b_new})")

        # 09 close the about window with its 'x' box
        cur.moveto(ab2_x + ABOUT_W - 24 + 9, ab2_y + 4 + 7)
        cur.click()
        assert wait_new_marker(qemu, "gui: close about window", log_len(qemu) - 2000, 15), \
            "about close marker missing"
        time.sleep(0.6)
        shot9 = qemu.screendump("09-about-closed")
        im9 = Image.open(shot9).convert("RGB")
        b_gone = blue_title(im9, at2box)
        assert b_gone < 100, f"about close: title still present ({b_gone})"
        print(f"[ok] about window closed: title region cleared ({b_gone})")

        # 10 esc -> text console back
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        line = [l for l in read_log(qemu).splitlines() if "gui: exit" in l][-1]
        clicks = int(re.search(r"clicks=(\d+)", line).group(1))
        drags = int(re.search(r"drags=(\d+)", line).group(1))
        assert clicks >= 5, f"expected >=5 clicks, got {clicks}"
        assert drags >= 1, f"expected >=1 drag frame, got {drags}"
        print(f"[ok] gui exited via esc ({line.strip()[:130]})")
        time.sleep(0.8)
        qemu.screendump("10-text-console-back")

        # 11 mouse packets accumulated
        qemu.type_text("mouse\n")
        deadline = time.time() + 10
        pkts_line = None
        while time.time() < deadline:
            lines = [l for l in read_log(qemu).splitlines() if "mouse: online=true" in l]
            if len(lines) >= 2:
                pkts_line = lines[-1]
                break
            time.sleep(0.2)
        pkts = int(re.search(r"packets=(\d+)", pkts_line).group(1))
        assert pkts > 0, "no packets after gui session"
        print(f"[ok] mouse packets after session: {pkts}")

        # 12 regressions
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

        # 13 neofetch + ps
        qemu.type_text("neofetch\n")
        time.sleep(1.2)
        qemu.screendump("11-neofetch")
        qemu.type_text("ps\n")
        time.sleep(0.6)
        qemu.screendump("12-final-ps")

        # 14 reboot from the start menu (fresh gui session)
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "second gui enter failed"
        time.sleep(1.0)
        cur = Cur(qemu, W // 2, H // 2)  # a fresh gui session centers the pointer
        cur.moveto(START_BTN[0], ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), "menu 2 missing"
        # 'reboot' item = k=2: below the separator
        cur.moveto(6 + 88, my + 4 + 40 + 6 + 10)
        cur.click()
        assert wait_new_marker(qemu, "gui: reboot requested from the start menu", log_len(qemu) - 2000, 15), \
            "reboot marker missing"
        deadline = time.time() + 15
        while qemu.proc.poll() is None and time.time() < deadline:
            time.sleep(0.2)
        assert qemu.proc.poll() is not None, "QEMU did not exit after guest reboot"
        print("[ok] reboot from start menu: guest reset, QEMU exited")

        os.replace(qemu.serial_log, os.path.join(SHOTS, "kernel.log"))
        print("\nALL v1.1 CHECKS PASSED")
    except AssertionError as e:
        print(f"FAIL: {e}")
        try:
            qemu.screendump("FAIL-state")
        except Exception:
            pass
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
