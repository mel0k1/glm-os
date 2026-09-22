#!/usr/bin/env python3
"""GLM OS v1.2 GUI test session -> deliverable screenshots.

Сценарий (smp 4 + e1000, одна сессия):
  01 boot + init мыши (klog-маркер)
  02 gui enter
  03 стартовое меню -> 'run ring-3 demo': ring-3 приложение открыло окно
     (klog: spawned as pid + user window id=1), GUIDEMO.ELF в ramdisk
  04 окно ring-3 рисует: красный мяч (324px) внутри региона окна
  05 анимация: два скриншота региона окна различаются (мяч двигается)
  06 клик по содержимому окна: EV_CLICK -> мяч перекрасился (красный -> голубой)
  07 resize монитора за grip: drag +40,+40 -> klog 'resized system monitor window'
  08 drag монитора за титлбар (регрессия) +30,+20
  09 [x] окна ring-3: EV_CLOSE -> приложение вышло с кодом 0
  10 esc: текстовая консоль восстановлена, exit reason=esc
  11 регрессии: FORKTEST, THREADTEST, UDP-пара, ping
  12 neofetch (1.2.0)
  13 повторный gui -> start -> reboot: QEMU уходит (-no-reboot)
"""
import os
import re
import sys
import time

from PIL import Image, ImageChops

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v12"

MON_W, MON_H = 380, 244
TASKBAR_H = 28
DEMO_X, DEMO_Y, DEMO_W, DEMO_H = 120, 120, 250, 180
START_BTN_X = 34


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


def count_px(img, box, pred):
    px = img.crop(box).getdata()
    return sum(1 for (r, g, b) in px if pred(r, g, b))


def red_ball(img, box):
    # ball 0xEB5A5A
    return count_px(img, box, lambda r, g, b: r > 170 and g < 130 and b < 130)


def cyan_ball(img, box):
    # ball 0x46E2E2
    return count_px(img, box, lambda r, g, b: g > 150 and b > 150 and r < 130)


def blue_title(img, box):
    return count_px(img, box, lambda r, g, b: b > 140 and b - r > 60 and b - g > 40)


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
        try:
            self.q.hmp("mouse_button 1")
            time.sleep(0.18)
            self.q.hmp("mouse_button 0")
        except (BrokenPipeError, OSError):
            pass
        time.sleep(0.3)

    def drag(self, dx, dy, step=40, pause=0.12):
        self.q.hmp("mouse_button 1")
        time.sleep(0.2)
        sx, sy = self.x, self.y
        n = max(abs(dx), abs(dy)) // step + 1
        for i in range(1, n + 1):
            self.q.hmp(f"mouse_move {dx // n} {dy // n}")
            self.x = sx + dx * i // n
            self.y = sy + dy * i // n
            time.sleep(pause)
        self.q.hmp("mouse_button 0")
        time.sleep(0.35)


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
        assert m, "framebuffer size not found"
        W, H = int(m.group(1)), int(m.group(2))
        ty = H - TASKBAR_H
        mon_x = (W - MON_W) // 2
        mon_y = max(0, (H - TASKBAR_H - MON_H) // 2 - 24)
        my = ty - 112 - 4  # start-menu panel top (5 items now)
        cur = Cur(qemu, W // 2, H // 2)
        print(f"[ok] boot, mouse init ok, framebuffer {W}x{H}")

        # 02 enter gui
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "gui never entered"
        time.sleep(1.2)
        shot = qemu.screendump("02-gui-desktop")
        print("[ok] gui entered")

        # 03 start menu -> run ring-3 demo (item k=2)
        cur.moveto(START_BTN_X, ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), "menu missing"
        time.sleep(0.4)
        qemu.screendump("03-start-menu")
        cur.moveto(6 + 90, my + 4 + 2 * 20 + 10)  # item 'run ring-3 demo'
        cur.click()
        assert wait_new_marker(qemu, "gui: ring-3 gui demo spawned as pid", log_len(qemu) - 3000, 20), \
            "ring-3 demo not spawned from start menu"
        assert wait_new_marker(qemu, "gui: user window id=1 opened by pid", log_len(qemu) - 3000, 20), \
            "user window not opened via syscall"
        line = [l for l in read_log(qemu).splitlines() if "user window id=1" in l][-1]
        print(f"[ok] ring-3 demo launched: {line.strip()[:80]}")

        # 04 the ring-3 window paints (red ball present in the CONTENT area;
        # content origin = window + (2, 23), excludes title bar + close btn)
        time.sleep(0.8)
        shot4 = qemu.screendump("04-ring3-window")
        im4 = Image.open(shot4).convert("RGB")
        dbox = (DEMO_X + 2, DEMO_Y + 23, DEMO_X + DEMO_W - 2, DEMO_Y + DEMO_H - 2)
        red0 = red_ball(im4, dbox)
        cyan0 = cyan_ball(im4, dbox)
        assert red0 > 150, f"ring-3 window did not paint the ball ({red0} red px)"
        # focused user window title bar is blue
        dtbox = (DEMO_X + 1, DEMO_Y + 1, DEMO_X + DEMO_W - 1, DEMO_Y + 22)
        b4 = blue_title(im4, dtbox)
        assert b4 > 1500, f"user window title not focused ({b4})"
        print(f"[ok] ring-3 window painted by the app (ball {red0}px, title-blue {b4})")

        # 05 animation: two frames differ inside the window
        shot5a = qemu.screendump("05a-anim")
        time.sleep(0.5)
        shot5b = qemu.screendump("05b-anim")
        ia = Image.open(shot5a).convert("RGB").crop(dbox)
        ib = Image.open(shot5b).convert("RGB").crop(dbox)
        diff = ImageChops.difference(ia, ib).convert("L")
        changed = sum(diff.histogram()[10:])
        assert changed > 50, f"no animation inside the ring-3 window ({changed} px)"
        print(f"[ok] ring-3 window animates ({changed} px changed)")

        # 06 click inside content -> EV_CLICK -> ball recolors red -> cyan
        cur.moveto(DEMO_X + 140, DEMO_Y + 120)
        cur.click()
        time.sleep(0.6)
        shot6 = qemu.screendump("06-clicked")
        im6 = Image.open(shot6).convert("RGB")
        red6 = red_ball(im6, dbox)
        cyan6 = cyan_ball(im6, dbox)
        assert red6 < 40 and cyan6 > cyan0 + 150, \
            f"click did not recolor the ball (red {red6}, cyan {cyan0}->{cyan6})"
        print(f"[ok] EV_CLICK delivered to ring-3 app (red {red6}, cyan {cyan0}->{cyan6})")

        # 07 resize the monitor via its grip
        # (the monitor lost focus to the demo window: the first click only
        # raises a window, so focus it by its title bar first)
        cur.moveto(mon_x + 150, mon_y + 11)
        cur.click()
        cur.moveto(mon_x + MON_W - 7, mon_y + MON_H - 7)
        n0 = log_len(qemu)
        cur.drag(40, 40)
        assert wait_new_marker(qemu, "gui: resize system monitor window started", n0, 15), \
            "resize never started (grip missed)"
        assert wait_new_marker(qemu, "gui: resized system monitor window to ", n0, 15), \
            "resize never finished"
        rline = [l for l in read_log(qemu).splitlines() if "gui: resized system monitor" in l][-1]
        mm = re.search(r"to (\d+)x(\d+)", rline)
        assert mm and int(mm.group(1)) >= MON_W + 20 and int(mm.group(2)) >= MON_H + 20, \
            f"resize too small: {rline.strip()}"
        time.sleep(0.4)
        qemu.screendump("07-monitor-resized")
        print(f"[ok] monitor resized via grip: {rline.strip()[:70]}")

        # 08 drag the monitor by its title bar (regression)
        cur.moveto(mon_x + 150, mon_y + 11)
        cur.drag(30, 20)
        time.sleep(0.4)
        qemu.screendump("08-monitor-dragged")
        mon2_x, mon2_y = mon_x + 30, mon_y + 20
        im8 = Image.open(qemu.screendump("08b-drag-check")).convert("RGB")
        tb_old = blue_title(im8, (mon_x + 1, mon_y + 1, mon_x + MON_W - 1, mon_y + 22))
        tb_new = blue_title(im8, (mon2_x + 1, mon2_y + 1, mon2_x + MON_W - 1, mon2_y + 22))
        # after resize the window is wider; check the new title region kept blue
        assert tb_new > 1000, f"monitor drag failed (old {tb_old}, new {tb_new})"
        print(f"[ok] monitor drag regression (title-blue new pos {tb_new})")

        # 09 close the ring-3 window with [x] -> app exits with code 0
        # (the demo window lost focus to the monitor: click 1 raises it,
        # click 2 lands on [x] of the now-focused window)
        cur.moveto(DEMO_X + DEMO_W - 24 + 9, DEMO_Y + 4 + 7)
        n1 = log_len(qemu)
        cur.click()
        cur.click()
        assert wait_new_marker(qemu, "gui: close ring 3 demo window", n1, 15), "close [x] missed"
        assert wait_new_marker(qemu, "exited with code 0", n1, 20), \
            "ring-3 app did not exit after EV_CLOSE"
        time.sleep(0.5)
        shot9 = qemu.screendump("09-ring3-closed")
        im9 = Image.open(shot9).convert("RGB")
        red9 = red_ball(im9, dbox)
        b9 = blue_title(im9, dtbox)
        assert red9 < 30 and b9 < 100, f"user window still visible after close (red {red9}, blue {b9})"
        print("[ok] [x] -> EV_CLOSE -> ring-3 app exited 0, window gone")

        # 10 esc -> text console back
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        time.sleep(0.8)
        qemu.screendump("10-text-console-back")
        print("[ok] gui exited via esc, text console restored")

        # 11 regressions
        qemu.type_text("run FORKTEST.ELF\n")
        assert qemu.wait_serial_marker("exited with code 42", 40), "forktest child failed"
        qemu.wait_serial_marker("exited with code 0", 20)
        print("[ok] forktest regression")

        qemu.type_text("spawn THREADTEST.ELF\n")
        n_thread = log_len(qemu)
        assert qemu.wait_serial_marker("exited with code 101", 40), "thread worker 101"
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

        # 12 neofetch
        qemu.type_text("neofetch\n")
        time.sleep(1.2)
        qemu.screendump("11-neofetch")
        print("[ok] neofetch")

        # 13 reboot from the start menu (fresh gui session)
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "second gui enter failed"
        time.sleep(1.0)
        cur = Cur(qemu, W // 2, H // 2)
        cur.moveto(START_BTN_X, ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), "menu 2 missing"
        # 'reboot' item = k=3: below the separator (3 launch items)
        cur.moveto(6 + 90, my + 4 + 3 * 20 + 6 + 10)
        cur.click()
        assert wait_new_marker(qemu, "gui: reboot requested from the start menu", log_len(qemu) - 2000, 15), \
            "reboot marker missing"
        deadline = time.time() + 15
        while qemu.proc.poll() is None and time.time() < deadline:
            time.sleep(0.2)
        assert qemu.proc.poll() is not None, "QEMU did not exit after guest reboot"
        print("[ok] reboot from start menu: guest reset, QEMU exited")

        os.replace(qemu.serial_log, os.path.join(SHOTS, "kernel.log"))
        print("\nALL v1.2 CHECKS PASSED")
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
