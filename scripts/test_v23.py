#!/usr/bin/env python3
"""GLM OS v2.3 test session: the smooth desktop.

v2.3 = пер-оконная двойная буферизация + явный present (SYS_GUI_FLUSH 54)
       + коалесценция грязных прямоугольников + clip-copy live-resize.

Сценарий (smp 4 + e1000 + ahci disk, одна сессия):
  01 boot + версия 2.3.0
  02 MALLOC.ELF (регрессия кучи) -> exit 0
  03 gui enter
  04 start menu -> 'text editor': EDIT.ELF запущен
  05 окно редактора нарисовано (SAVE + статусбар) -- это ПЕРВЫЙ present
  06 набор 'hello smooth desktop' -> текст виден (пиксели)
  07 klog 'gui: presents total=' -- present-путь реально работает
  08 RESIZE за grip c УДЕРЖАНИЕМ кнопки: скриншот в середине drag'а ->
     текст ВСЁ ЕЩЁ виден (clip-copy front, никакой чёрной вспышки)
  09 release -> 'resized text editor window to' + перерисовка -> текст
     и статусбар на НОВОЙ геометрии
  10 [SAVE] -> klog: file: flush /HOME/UNTITLED.TXT
  11 [x] -> exit 0
  12 esc -> 'gui: N window presents this session', N >= 1
  13 run FILES.ELF /HOME -> exit 2 (сохранённый файл)
"""
import os
import re
import sys
import time
import shutil

from PIL import Image

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/shots-v23"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = WORK + "/disk-copy.img"

TASKBAR_H = 28
MENU_H = 172
EDIT_X, EDIT_Y, EDIT_W, EDIT_H = 190, 110, 470, 310


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
    """Chain spawned 'NAME' -> pid -> 'task N exited with code M'."""
    deadline = time.time() + 15
    m = None
    while time.time() < deadline:
        m = re.search(r"spawned '%s[^\n]*pid (\d+)" % re.escape(task), read_log(q)[start:])
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


def green_px(img, box):
    # SAVE button 0x5EE28D
    return count_px(img, box, lambda r, g, b: g > 170 and r < 150 and 90 < b < 190)


def text_px(img, box):
    # bright glyphs 0xC6CCD8
    return count_px(img, box, lambda r, g, b: r > 150 and g > 150 and b > 150)


def status_px(img, box):
    # status bar 0x1B2029
    return count_px(img, box, lambda r, g, b: 14 <= r <= 40 and 18 <= g <= 46 and 26 <= b <= 58)


def dark_px(img, box):
    # black-ish content (a zeroed buffer / black flash)
    return count_px(img, box, lambda r, g, b: r < 12 and g < 12 and b < 12)


class Cur:
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

    def drag_hold(self, dx, dy, step=40, pause=0.12):
        """Press and move WITHOUT releasing (for mid-drag screenshots)."""
        self.q.hmp("mouse_button 1")
        time.sleep(0.2)
        sx, sy = self.x, self.y
        n = max(abs(dx), abs(dy)) // step + 1
        for i in range(1, n + 1):
            self.q.hmp(f"mouse_move {dx // n} {dy // n}")
            self.x = sx + dx * i // n
            self.y = sy + dy * i // n
            time.sleep(pause)

    def release(self):
        self.q.hmp("mouse_button 0")
        time.sleep(0.35)


def main():
    os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
        "/home/z/sysroot/lib/x86_64-linux-gnu:"
        + os.environ.get("LD_LIBRARY_PATH", "")
    )
    os.makedirs(WORK, exist_ok=True)
    shutil.copyfile(DISK, DISK_COPY)
    qemu = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
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
        my = ty - MENU_H - 4
        cur = Cur(qemu, W // 2, H // 2)
        print(f"[ok] boot, framebuffer {W}x{H}")

        # 02 MALLOC regression
        n0 = log_len(qemu)
        qemu.type_text("run MALLOC.ELF\n")
        code = exit_code(qemu, "MALLOC", n0, 90)
        assert code == 0, f"MALLOC exited with {code}, want 0"
        ok("boot v2.7.0 + MALLOC regression", "exit 0")

        # 03 gui
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "gui never entered"
        time.sleep(1.2)
        ok("gui entered")

        # 04 start menu -> text editor (item k=4)
        cur.moveto(34, ty + 14)
        cur.click()
        assert wait_new_marker(qemu, "gui: start menu open", log_len(qemu) - 2000, 15), "menu missing"
        time.sleep(0.4)
        cur.moveto(6 + 94, my + 4 + 4 * 20 + 10)
        cur.click()
        assert wait_new_marker(qemu, "gui: text editor spawned as pid", log_len(qemu) - 3000, 20), \
            "text editor not spawned from start menu"
        time.sleep(1.0)
        ok("EDIT spawned from start menu")

        # 05 editor window painted == the FIRST present landed
        cx, cy = EDIT_X + 2, EDIT_Y + 23
        cw, ch = EDIT_W - 4, EDIT_H - 24
        shot5 = qemu.screendump("05-editor-empty")
        im5 = Image.open(shot5).convert("RGB")
        g = green_px(im5, (cx, cy, cx + cw, cy + 20))
        st = status_px(im5, (cx, cy + ch - 18, cx + cw, cy + ch))
        assert g > 200, f"SAVE button not painted ({g} px)"
        assert st > 4000, f"status bar not painted ({st} px)"
        ok("editor window painted (first present)", f"SAVE {g}px, status {st}px")

        # 06 click into the text area, type
        cur.moveto(cx + 60, cy + 24 + 7)
        cur.click()
        time.sleep(0.4)
        qemu.type_text("hello smooth desktop")
        time.sleep(0.6)
        shot6 = qemu.screendump("06-typed")
        im6 = Image.open(shot6).convert("RGB")
        t6 = text_px(im6, (cx, cy + 20, cx + cw, cy + ch - 18))
        assert t6 > 220, f"typed line not visible ({t6} px)"
        ok("typed line", f"{t6} text px")

        # 07 the present path is real: rate-limited klog from sys_flush
        n7 = log_len(qemu)
        assert wait_new_marker(qemu, "gui: presents total=", n7 - 4000, 15), \
            "no 'gui: presents total=' klog -- flush path never ran"
        ok("presents klog present", "SYS_GUI_FLUSH works")

        # 08 live resize with the button HELD: the old content must stay
        # visible (clip-copied front buffer). The newly exposed L-strip is
        # black until the app repaints at release -- that is the expected
        # live-resize look; what must NOT happen is the whole content going
        # black (the zeroed-buffer bug this clip-copy exists to prevent).
        cur.moveto(EDIT_X + EDIT_W - 7, EDIT_Y + EDIT_H - 7)
        n8 = log_len(qemu)
        cur.drag_hold(80, 60)
        time.sleep(0.35)  # let the compositor render the held state
        shot8 = qemu.screendump("08-mid-resize")
        im8 = Image.open(shot8).convert("RGB")
        # the OLD content area (top-left) keeps toolbar + SAVE + text + caret
        ocx, ocy, ocw, och = cx, cy, cw, ch
        t8 = text_px(im8, (ocx, ocy + 20, ocx + ocw, ocy + och - 18))
        g8 = green_px(im8, (ocx, ocy, ocx + ocw, ocy + 20))
        st8 = status_px(im8, (ocx, ocy + och - 18, ocx + ocw, ocy + och))
        assert t8 > 150, f"old content vanished mid-resize ({t8} px text)"
        assert g8 > 100, f"toolbar vanished mid-resize ({g8} px green)"
        assert st8 > 3000, f"status bar vanished mid-resize ({st8} px)"
        ok("mid-resize: old content clip-copied and visible",
           f"text {t8}px, SAVE {g8}px, status {st8}px")

        # 09 release -> EV_RESIZE -> full repaint at the new geometry
        cur.release()
        assert wait_new_marker(qemu, "gui: resized text editor window to ", n8, 15), \
            "resize never finished"
        time.sleep(0.8)
        shot9 = qemu.screendump("09-after-resize")
        im9 = Image.open(shot9).convert("RGB")
        cx2, cy2 = cx, cy
        cw2, ch2 = cw + 80, ch + 60
        t9 = text_px(im9, (cx2, cy2 + 20, cx2 + cw2, cy2 + ch2 - 18))
        st9 = status_px(im9, (cx2, cy2 + ch2 - 18, cx2 + cw2, cy2 + ch2))
        assert t9 > 220, f"repaint after resize lost the text ({t9} px)"
        assert st9 > 4000, f"status bar not at the new geometry ({st9} px)"
        ok("repaint at new geometry", f"{t9} text px, status {st9}px")

        # 10 [SAVE] -> kernel flush klog
        cur.moveto(cx2 + 26, cy2 + 10)
        cur.click()
        assert wait_new_marker(qemu, "file: flush /HOME/UNTITLED.TXT", log_len(qemu) - 3000, 20), \
            "save did not flush the file"
        ok("SAVE flushed the file", "/HOME/UNTITLED.TXT")

        # 11 [x] closes -> exit 0
        cur.moveto(EDIT_X + EDIT_W + 80 - 24 + 9, EDIT_Y + 4 + 7)
        n1 = log_len(qemu)
        cur.click()
        code = None
        deadline = time.time() + 20
        while time.time() < deadline:
            tail = read_log(qemu)[n1:]
            idx = tail.find("gui: close text editor window")
            if idx >= 0:
                mm = re.search(r"task (\d+) exited with code (-?\d+)", tail[idx:])
                if mm:
                    code = int(mm.group(2))
                    break
            time.sleep(0.2)
        assert code == 0, f"EDIT exited with {code}, want 0"
        ok("[x] -> EDIT exit 0")

        # 12 esc -> console; the session presents counter is on the log
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        pmm = re.search(r"gui: (\d+) window presents this session", read_log(qemu))
        assert pmm, "presents session summary missing"
        assert int(pmm.group(1)) >= 1, "presents counter is zero"
        ok("session presents counter", f"{pmm.group(1)} presents")

        # 13 saved file visible in /HOME
        time.sleep(0.8)
        n0 = log_len(qemu)
        qemu.type_text("run FILES.ELF /HOME\n")
        code = exit_code(qemu, "FILES", n0, 60)
        assert code == 2, f"FILES /HOME exited {code}, want 2"
        ok("saved file visible in /HOME", "FILES exit 2")

    except AssertionError as e:
        print(f"[FAIL] {e}")
        rc = 1
    finally:
        try:
            qemu.proc.kill()
        except Exception:
            pass

    print(f"\n=== v2.3: {len(checks)} checks passed ===")
    for c in checks:
        print(f"  - {c}")
    sys.exit(rc)


if __name__ == "__main__":
    main()
