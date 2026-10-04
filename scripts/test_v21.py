#!/usr/bin/env python3
"""GLM OS v2.1 test session: ring-3 heap (sbrk + malloc) + FMGR desktop app.

Сценарий (smp 4 + e1000, одна сессия):
  01 boot + версия 2.1.0
  02 MALLOC.ELF: 7 фаз тюринга кучи -> exit 0 (klog: task MALLOC exited with code 0)
  03 HEAPFORK.ELF: CoW-изоляция sbrk-страниц при fork -> exit 0
  04 FILES.ELF /HOME/DOCS (регрессия v1.9) -> exit 1 (одна запись)
  05 gui enter
  06 start menu -> 'file manager': FMGR.ELF запущен (klog spawned as pid)
  07 окно FMGR нарисовано: тулбар + статусбар (пиксель-проверки)
  08 клик по строке 1 (/HOME) -> навигация (содержимое окна изменилось)
  09 клик по строке 0 (/HOME/DOCS) -> снова навигация
  10 клик по WELCOME.TXT -> viewer: зелёная кнопка BACK
  11 [x] -> FMGR вышел с кодом 0, окно исчезло
  12 регрессии: FORKTEST, THREADTEST, echo|upper, ping
  13 esc -> текстовая консоль восстановлена
"""
import os
import re
import sys
import time

from PIL import Image, ImageChops

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v21"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = "/home/z/glm-os/shots-v21/disk-copy.img"

TASKBAR_H = 28
MENU_H = 172  # 4 + 6*20 + 6 + 2*20 + 2 (v2.2: +text editor item)
FMGR_X, FMGR_Y, FMGR_W, FMGR_H = 140, 90, 400, 280


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


def exit_code(q, task, start, timeout=60):
    """Wait for the exit of the task spawned as 'task' after `start`.

    The kernel klogs two lines: spawned 'NAME' pid N ... and later
    'sched: task N exited with code M'. Chain them by pid.
    """
    log = read_log(q)[start:]
    m = re.search(r"spawned '%s[^\n]*pid (\d+)" % re.escape(task), log)
    if not m:
        # wait for the spawn line first
        deadline = time.time() + 15
        while time.time() < deadline:
            log = read_log(q)[start:]
            m = re.search(r"spawned '%s[^\n]*pid (\d+)" % re.escape(task), log)
            if m:
                break
            if q.proc.poll() is not None:
                return None
            time.sleep(0.2)
        if not m:
            return None
    pid = m.group(1)
    pat = "task %s exited with code " % pid
    deadline = time.time() + timeout
    while time.time() < deadline:
        log = read_log(q)[start:]
        idx = log.find(pat)
        if idx >= 0:
            mm = re.search(r"task %s exited with code (-?\d+)" % pid, log[idx:])
            return int(mm.group(1))
        if q.proc.poll() is not None:
            return None
        time.sleep(0.2)
    return None


def count_px(img, box, pred):
    px = img.crop(box).getdata()
    return sum(1 for (r, g, b) in px if pred(r, g, b))


def toolbar_px(img, box):
    # toolbar 0x2A3140 = (42, 49, 64)
    return count_px(img, box, lambda r, g, b: 30 <= r <= 60 and 38 <= g <= 66 and 52 <= b <= 84)


def status_px(img, box):
    # status bar 0x1B2029 = (27, 32, 41)
    return count_px(img, box, lambda r, g, b: 14 <= r <= 40 and 18 <= g <= 46 and 26 <= b <= 58)


def green_px(img, box):
    # viewer BACK button 0x5EE28D = (94, 226, 141)
    return count_px(img, box, lambda r, g, b: g > 170 and r < 150 and 90 < b < 190)


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


def main():
    os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
        "/home/z/sysroot/lib/x86_64-linux-gnu:"
        + os.environ.get("LD_LIBRARY_PATH", "")
    )
    os.makedirs(SHOTS, exist_ok=True)
    import shutil
    shutil.copyfile(DISK, DISK_COPY)
    qemu = QemuSession(ISO, SHOTS, smp="4", nic="user,model=e1000", disk=DISK_COPY)
    rc = 0
    checks = []

    def ok(name, detail=""):
        checks.append(name)
        print(f"[ok] {name}" + (f" ({detail})" if detail else ""))

    try:
        # 01 boot
        assert qemu.wait_serial_marker("boot complete", 90), "boot never completed"
        log = read_log(qemu)
        assert "GLM OS v2.7.0" in log, "kernel version banner missing"
        m = re.search(r"framebuffer (\d+)x(\d+)x(\d+)", log)
        assert m, "framebuffer size not found"
        W, H = int(m.group(1)), int(m.group(2))
        ty = H - TASKBAR_H
        my = ty - MENU_H - 4  # start-menu panel top
        cur = Cur(qemu, W // 2, H // 2)
        print(f"[ok] boot, framebuffer {W}x{H}")
        qemu.type_text("neofetch\n")
        time.sleep(1.0)
        ok("boot v2.7.0")

        # 02 MALLOC torture
        n0 = log_len(qemu)
        qemu.type_text("run MALLOC.ELF\n")
        code = exit_code(qemu, "MALLOC", n0, 90)
        assert code == 0, f"MALLOC exited with {code}, want 0"
        ok("MALLOC.ELF 7 heap phases", "exit 0")

        # 03 HEAPFORK: CoW isolation of sbrk pages
        n0 = log_len(qemu)
        qemu.type_text("run HEAPFORK.ELF\n")
        code = exit_code(qemu, "HEAPFORK", n0, 60)
        assert code == 0, f"HEAPFORK exited with {code}, want 0"
        ok("HEAPFORK.ELF CoW isolation", "exit 0")

        # 04 FILES regression (v1.9 ABI): /HOME/DOCS has exactly 1 entry
        n0 = log_len(qemu)
        qemu.type_text("run FILES.ELF /HOME/DOCS\n")
        code = exit_code(qemu, "FILES", n0, 60)
        assert code == 1, f"FILES /HOME/DOCS exited with {code}, want 1"
        ok("FILES.ELF regression", "exit 1 (one entry)")

        # 05 enter gui
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "gui never entered"
        time.sleep(1.2)
        qemu.screendump("05-gui-desktop")
        ok("gui entered")

        # 06 start menu -> file manager (item k=3)
        cur.moveto(34, ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), "menu missing"
        time.sleep(0.4)
        qemu.screendump("06-start-menu")
        cur.moveto(6 + 94, my + 4 + 3 * 20 + 10)  # item 'file manager'
        cur.click()
        assert wait_new_marker(qemu, "gui: file manager spawned as pid", log_len(qemu) - 3000, 20), \
            "file manager not spawned from start menu"
        time.sleep(1.0)
        ok("FMGR spawned from start menu")

        # 07 window painted: toolbar + status bar in the content area
        # content origin = (FMGR_X + 2, FMGR_Y + 23)
        cx, cy = FMGR_X + 2, FMGR_Y + 23
        cw, ch = FMGR_W - 4, FMGR_H - 24
        shot7 = qemu.screendump("07-fmgr-root")
        im7 = Image.open(shot7).convert("RGB")
        tb = toolbar_px(im7, (cx, cy, cx + cw, cy + 20))
        st = status_px(im7, (cx, cy + ch - 18, cx + cw, cy + ch))
        assert tb > 2500, f"toolbar not painted ({tb} px)"
        assert st > 3500, f"status bar not painted ({st} px)"
        ok("FMGR window painted", f"toolbar {tb}px, status {st}px")
        root_dump = im7.crop((cx, cy + 20, cx + cw, cy + ch - 18))

        # 08 click row 1 -> navigate into /HOME (root: [BIN, HOME, README.TXT])
        cur.moveto(cx + 100, cy + 24 + 1 * 14 + 7)
        cur.click()
        time.sleep(0.8)
        shot8 = qemu.screendump("08-fmgr-home")
        im8 = Image.open(shot8).convert("RGB")
        home_rows = im8.crop((cx, cy + 20, cx + cw, cy + ch - 18))
        diff8 = ImageChops.difference(root_dump, home_rows).convert("L")
        changed8 = sum(diff8.histogram()[10:])
        assert changed8 > 300, f"navigation to /HOME changed nothing ({changed8})"
        ok("navigation into /HOME", f"{changed8} px changed")
        home_dump = home_rows

        # 09 click row 0 -> /HOME/DOCS (only DOCS dir inside /HOME)
        cur.moveto(cx + 100, cy + 24 + 0 * 14 + 7)
        cur.click()
        time.sleep(0.8)
        shot9 = qemu.screendump("09-fmgr-docs")
        im9 = Image.open(shot9).convert("RGB")
        docs_rows = im9.crop((cx, cy + 20, cx + cw, cy + ch - 18))
        diff9 = ImageChops.difference(home_dump, docs_rows).convert("L")
        changed9 = sum(diff9.histogram()[10:])
        assert changed9 > 100, f"navigation to /HOME/DOCS changed nothing ({changed9})"
        ok("navigation into /HOME/DOCS", f"{changed9} px changed")

        # 10 click row 0 -> WELCOME.TXT opens in the viewer (green BACK btn)
        cur.moveto(cx + 100, cy + 24 + 0 * 14 + 7)
        cur.click()
        time.sleep(0.8)
        shot10 = qemu.screendump("10-fmgr-viewer")
        im10 = Image.open(shot10).convert("RGB")
        g = green_px(im10, (cx, cy, cx + cw, cy + 20))
        assert g > 200, f"viewer BACK button not painted ({g} green px)"
        ok("file viewer opened", f"BACK {g}px green")

        # 11 [x] closes the window -> FMGR exits 0, window gone
        cur.moveto(FMGR_X + FMGR_W - 24 + 9, FMGR_Y + 4 + 7)
        n1 = log_len(qemu)
        cur.click()
        assert wait_new_marker(qemu, "gui: close file manager window", n1, 20), \
            "close [x] missed"
        # the exit line (by pid) follows the close marker
        code = None
        deadline = time.time() + 20
        while time.time() < deadline:
            tail = read_log(qemu)[n1:]
            idx = tail.find("gui: close file manager window")
            if idx >= 0:
                m = re.search(r"task (\d+) exited with code (-?\d+)", tail[idx:])
                if m:
                    code = int(m.group(2))
                    break
            time.sleep(0.2)
        assert code == 0, f"FMGR exited with {code}, want 0"
        time.sleep(0.6)
        shot11 = qemu.screendump("11-fmgr-closed")
        im11 = Image.open(shot11).convert("RGB")
        tb11 = toolbar_px(im11, (cx, cy, cx + cw, cy + 20))
        assert tb11 < 60, f"window still visible after close ({tb11} px)"
        ok("[x] -> FMGR exit 0, window gone")

        # 12 regressions in the text console
        cur.moveto(34, ty + 14)
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        time.sleep(0.8)
        n0 = log_len(qemu)
        qemu.type_text("run FORKTEST.ELF\n")
        # v1.7 convention: the child exits 42, the parent prints 0
        codes = []
        deadline = time.time() + 60
        while time.time() < deadline and len(codes) < 2:
            codes = re.findall(r"task (\d+) exited with code (-?\d+)", read_log(qemu)[n0:])
            time.sleep(0.2)
        codes = [int(c) for _, c in codes[:2]]
        assert codes == [42, 0], f"FORKTEST codes {codes}, want [42, 0]"
        n0 = log_len(qemu)
        qemu.type_text("run THREADTEST.ELF\n")
        code = exit_code(qemu, "THREADTEST", n0, 90)
        assert code == 0, f"THREADTEST exited {code}, want 0"
        qemu.type_text("echo hello | upper\n")
        time.sleep(1.5)
        n0 = log_len(qemu)
        qemu.type_text("spawn UDPSERV.ELF\n")
        assert qemu.wait_serial_marker("sock: bound id=", 20), "udp server did not bind"
        n0 = log_len(qemu)
        qemu.type_text("run UDPCLI.ELF\n")
        code = exit_code(qemu, "UDPCLI", n0, 60)
        assert code == 0, f"UDPCLI exited {code}, want 0"
        ok("regressions: FORKTEST [42,0], THREADTEST 0, pipe, udp pair 0")

        # 13 console still alive
        n0 = log_len(qemu)
        qemu.type_text("run HELLO.ELF\n")
        code = exit_code(qemu, "HELLO", n0, 15)
        assert code == 0, "console dead after full session (hello exit %s)" % code
        ok("shell alive after everything")

    except AssertionError as e:
        print(f"[FAIL] {e}")
        rc = 1
    finally:
        try:
            qemu.proc.kill()
        except Exception:
            pass

    print(f"\n=== v2.1: {len(checks)} checks passed ===")
    for c in checks:
        print(f"  - {c}")
    sys.exit(rc)


if __name__ == "__main__":
    main()
