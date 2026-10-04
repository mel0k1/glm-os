#!/usr/bin/env python3
"""GLM OS v2.2 test session: the ring-3 text editor (EDIT.ELF).

Сценарий (smp 4 + e1000 + ahci disk, одна сессия + одна после reboot):
  01 boot + версия 2.2.0
  02 MALLOC.ELF (регрессия кучи после реорганизации userlib) -> exit 0
  03 gui enter
  04 start menu -> 'text editor': EDIT.ELF запущен (klog spawned as pid)
  05 окно редактора нарисовано: зелёная кнопка SAVE + статусбар
  06 клик в текст -> карет, набор строки 1 'hello from glm os'
  07 enter + строка 2 'editing in ring 3' -> текст в окне (пиксели)
  08 клик [SAVE] -> klog: file: flush /HOME/UNTITLED.TXT
  09 [x] -> редактор вышел с кодом 0
  10 esc -> текстовая консоль; run FILES.ELF /HOME -> exit 2 (DOCS + UNTITLED)
  11 reboot -> QEMU уходит; вторая сессия: FILES /HOME -> exit 2 (персистентность)
  12 повторный запуск редактора на СУЩЕСТВУЮЩЕМ файле: текст виден (пиксели)
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
WORK = "/home/z/glm-os/shots-v22"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = WORK + "/disk-copy.img"

TASKBAR_H = 28
MENU_H = 172  # 4 + 6*20 + 6 + 2*20 + 2
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
        assert "GLM OS v2.8.0" in log, "kernel version banner missing"
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
        ok("boot v2.8.0 + MALLOC regression", "exit 0")

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
        qemu.screendump("04-start-menu")
        cur.moveto(6 + 94, my + 4 + 4 * 20 + 10)
        cur.click()
        assert wait_new_marker(qemu, "gui: text editor spawned as pid", log_len(qemu) - 3000, 20), \
            "text editor not spawned from start menu"
        time.sleep(1.0)
        ok("EDIT spawned from start menu")

        # 05 editor window painted (SAVE button + status bar)
        cx, cy = EDIT_X + 2, EDIT_Y + 23
        cw, ch = EDIT_W - 4, EDIT_H - 24
        shot5 = qemu.screendump("05-editor-empty")
        im5 = Image.open(shot5).convert("RGB")
        g = green_px(im5, (cx, cy, cx + cw, cy + 20))
        st = status_px(im5, (cx, cy + ch - 18, cx + cw, cy + ch))
        assert g > 200, f"SAVE button not painted ({g} px)"
        assert st > 4000, f"status bar not painted ({st} px)"
        ok("editor window painted", f"SAVE {g}px, status {st}px")

        # 06 click into the text area, type line 1
        cur.moveto(cx + 60, cy + 24 + 7)
        cur.click()
        time.sleep(0.4)
        qemu.type_text("hello from glm os")
        time.sleep(0.6)
        shot6 = qemu.screendump("06-typed-line1")
        im6 = Image.open(shot6).convert("RGB")
        t6 = text_px(im6, (cx, cy + 20, cx + cw, cy + ch - 18))
        assert t6 > 220, f"typed line not visible ({t6} px)"
        ok("typed line 1", f"{t6} text px")

        # 07 enter + line 2
        qemu.hmp("sendkey ret")
        time.sleep(0.3)
        qemu.type_text("editing in ring 3")
        time.sleep(0.6)
        shot7 = qemu.screendump("07-typed-line2")
        im7 = Image.open(shot7).convert("RGB")
        t7 = text_px(im7, (cx, cy + 20, cx + cw, cy + ch - 18))
        assert t7 > t6 + 120, f"line 2 not added ({t6} -> {t7})"
        ok("typed line 2", f"{t6} -> {t7} px")

        # 08 [SAVE] -> kernel flush klog
        cur.moveto(cx + 26, cy + 10)
        cur.click()
        assert wait_new_marker(qemu, "file: flush /HOME/UNTITLED.TXT", log_len(qemu) - 3000, 20), \
            "save did not flush the file"
        ok("SAVE flushed the file", "/HOME/UNTITLED.TXT")

        # 09 [x] closes -> exit 0
        cur.moveto(EDIT_X + EDIT_W - 24 + 9, EDIT_Y + 4 + 7)
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

        # 10 esc -> console; FILES /HOME = 2 (DOCS + UNTITLED.TXT)
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        time.sleep(0.8)
        n0 = log_len(qemu)
        qemu.type_text("run FILES.ELF /HOME\n")
        code = exit_code(qemu, "FILES", n0, 60)
        assert code == 2, f"FILES /HOME exited {code}, want 2"
        ok("saved file visible in /HOME", "FILES exit 2")

        # 11 reboot -> persistence
        qemu.type_text("reboot\n")
        deadline = time.time() + 30
        while time.time() < deadline and qemu.proc.poll() is None:
            time.sleep(0.3)
        time.sleep(1.0)
        qemu.proc.kill()
        qemu2 = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
        try:
            assert qemu2.wait_serial_marker("boot complete", 90), "second boot failed"
            qemu2.type_text("run FILES.ELF /HOME\n")
            code = exit_code(qemu2, "FILES", 0, 60)
            assert code == 2, f"FILES /HOME after reboot: {code}, want 2"
            ok("edited file survives the reboot", "FILES exit 2")

            # 12 reopen the editor on the EXISTING file: text visible
            qemu2.type_text("gui\n")
            assert qemu2.wait_serial_marker("gui: enter (double buffered", 15), "gui never entered"
            time.sleep(1.2)
            cur2 = Cur(qemu2, W // 2, H // 2)
            cur2.moveto(34, ty + 14)
            cur2.click()
            assert wait_new_marker(qemu2, "gui: start menu open", log_len(qemu2) - 2000, 15), "menu missing"
            time.sleep(0.4)
            cur2.moveto(6 + 94, my + 4 + 4 * 20 + 10)
            cur2.click()
            assert wait_new_marker(qemu2, "gui: text editor spawned as pid", log_len(qemu2) - 3000, 20), \
                "editor not respawned"
            time.sleep(1.0)
            shot12 = qemu2.screendump("12-editor-reloaded")
            im12 = Image.open(shot12).convert("RGB")
            t12 = text_px(im12, (cx, cy + 20, cx + cw, cy + ch - 18))
            assert t12 > 220, f"reloaded document not rendered ({t12} px)"
            ok("editor reloaded the saved document", f"{t12} text px")
        finally:
            try:
                qemu2.proc.kill()
            except Exception:
                pass

    except AssertionError as e:
        print(f"[FAIL] {e}")
        rc = 1
    finally:
        try:
            qemu.proc.kill()
        except Exception:
            pass

    print(f"\n=== v2.2: {len(checks)} checks passed ===")
    for c in checks:
        print(f"  - {c}")
    sys.exit(rc)


if __name__ == "__main__":
    main()
