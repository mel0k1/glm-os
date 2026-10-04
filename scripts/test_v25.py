#!/usr/bin/env python3
"""GLM OS v2.5 test session — keyboard usability.

New in v2.5:
  * kernel keyboard: 0xE0 extended decode — arrows, Home/End, Delete,
    PgUp/PgDn as high key codes (0x80..=0x88); 0xE1 pause swallowed
  * console: real backspace (0x08 moves the caret, row wrap included)
  * src/lineedit.rs: shared single-line editor with a history ring —
    used by BOTH the text shell and GUI terminal sessions
  * EDIT.ELF: arrows/Home/End/PgUp/PgDn navigation + forward delete

Oracle: UPPER.ELF exits with the number of bytes it forwarded, so an
edited pipeline `run ECHO.ELF hi15 | run UPPER.ELF` is a precise
byte-level oracle for what the edited line contained:
    hi15\\n -> exit 5,  hi115\\n -> exit 6,  hi055\\n -> exit 6

Covered:
  A. text console: baseline pipeline exit 5; UP-recall + HOME + RIGHT*15
     + insert -> exit 6; recall + DEL -> exit 5; end-of-line backspace
     + append -> exit 5
  B. terminal window (kterm): same editing keys through the window
     caret-offset redraw -> exits 5/6/6; recall line visibly rendered
  C. EDIT: navigation keys move the caret (status bar LN/COL diffs),
     mid-line insert growth, forward delete, PgDn/PgUp page jumps
  D. regressions: MALLOC heap torture, FILES listing, EDIT exit 0
"""
import os
import re
import shutil
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402
from PIL import Image  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = "/home/z/glm-os/work-v25/disk-test.img"
WORK = "/home/z/glm-os/work-v25"
SHOTS = "/home/z/glm-os/shots-v25"

TASKBAR_H = 28
MENU_H = 172
EDIT_X, EDIT_Y, EDIT_W, EDIT_H = 190, 110, 470, 310
TERM_W, TERM_H, TITLE_H = 464, 304, 22

results = []


def check(name, ok, extra=""):
    results.append((name, bool(ok)))
    print(f"  [{'ok ' if ok else 'FAIL'}] {name}" + (f" | {extra}" if extra and not ok else ""))


def read_log(q):
    try:
        return open(q.serial_log, errors="replace").read()
    except FileNotFoundError:
        return ""


def log_len(q):
    return len(read_log(q))


EXIT_LINE = re.compile(r"sched: task (\d+) exited with code (-?\d+)")
SPAWN_LINE = re.compile(r"sched: spawned '([^']+)' pid (\d+)")


def exit_code_for(q, name, start, timeout=60):
    """Exit code of the newest task spawned as `name` after `start`."""
    deadline = time.time() + timeout
    log = read_log(q)[start:]
    pids = [int(m.group(2)) for m in SPAWN_LINE.finditer(log) if m.group(1) == name]
    if not pids:
        return None
    pid = pids[-1]
    while time.time() < deadline:
        for m in EXIT_LINE.finditer(read_log(q)[start:]):
            if int(m.group(1)) == pid:
                return int(m.group(2))
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


def send_key(q, name, pause=0.09):
    q.hmp(f"sendkey {name}")
    time.sleep(pause)


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
        self.q.hmp("mouse_button 1")
        time.sleep(0.18)
        self.q.hmp("mouse_button 0")
        time.sleep(0.3)


def count_px(img, box, pred):
    return sum(1 for (r, g, b) in img.crop(box).getdata() if pred(r, g, b))


def bright_px(img, box):
    # terminal/editor text glyphs: all channels clearly above the dark bg
    return count_px(img, box, lambda r, g, b: r > 100 and g > 100 and b > 100)


def px_diff(img_a, img_b, box):
    a = list(img_a.crop(box).getdata())
    b = list(img_b.crop(box).getdata())
    return sum(1 for (p, q) in zip(a, b) if p != q)


def main():
    os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
        "/home/z/sysroot/lib/x86_64-linux-gnu:"
        + os.environ.get("LD_LIBRARY_PATH", "")
    )
    os.makedirs(WORK, exist_ok=True)
    os.makedirs(SHOTS, exist_ok=True)
    shutil.copyfile(DISK, DISK_COPY)
    q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)

    try:
        # ---- 01 boot -------------------------------------------------
        assert q.wait_serial_marker("boot complete", 90), "boot never completed"
        log = read_log(q)
        check("01 boot banner v2.7.0", "GLM OS v2.7.0" in log)
        check("02 rtc wall clock line", "rtc: wall clock" in log)
        m = re.search(r"framebuffer (\d+)x(\d+)x(\d+)", log)
        assert m, "framebuffer size not found"
        W, H = int(m.group(1)), int(m.group(2))
        ty = H - TASKBAR_H
        my = ty - MENU_H - 4
        cur = Cur(q, W // 2, H // 2)
        print(f"  framebuffer {W}x{H}")

        # ---- 03 MALLOC regression ------------------------------------
        n0 = log_len(q)
        q.type_text("run MALLOC.ELF\n")
        code = exit_code_for(q, "MALLOC.ELF", n0, 90)
        check("04 MALLOC regression", code == 0, f"exit {code}")

        # ---- 05 console: pipeline baseline (oracle = 5) ---------------
        n0 = log_len(q)
        q.type_text("run ECHO.ELF hi15 | run UPPER.ELF\n")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("05 console pipeline baseline", code == 5, f"exit {code}, want 5 (hi15\\n)")

        # ---- 06 console: UP-recall + HOME + RIGHT*15 + insert = 6 -----
        n0 = log_len(q)
        send_key(q, "up")            # recall the baseline line
        send_key(q, "home")
        for _ in range(15):
            send_key(q, "right")     # caret before the '1' of hi15
        q.type_text("1")             # insert -> hi115
        send_key(q, "end")
        q.type_text("\n")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("06 console recall+home+right+insert", code == 6, f"exit {code}, want 6 (hi115\\n)")
        q.screendump(os.path.join(SHOTS, "06-console-after-edit"))

        # ---- 07 console: recall + DEL = 5 -----------------------------
        n0 = log_len(q)
        send_key(q, "up")            # recall 'run ECHO.ELF hi115 | run UPPER.ELF'
        send_key(q, "home")
        for _ in range(15):
            send_key(q, "right")     # caret before the first '1'
        send_key(q, "delete")        # -> hi15 | run UPPER.ELF
        q.type_text("\n")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("07 console recall+DEL", code == 5, f"exit {code}, want 5 (hi15\\n)")

        # ---- 08 console: end-of-line backspace + append = 5 -----------
        n0 = log_len(q)
        q.type_text("run ECHO.ELF hi158")
        send_key(q, "backspace")     # drop the trailing 8 -> hi15
        q.type_text(" | run UPPER.ELF")
        q.type_text("\n")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("08 console backspace+append", code == 5, f"exit {code}, want 5 (hi15\\n)")

        # ---- 09 enter the desktop -------------------------------------
        q.type_text("gui\n")
        assert q.wait_serial_marker("gui: enter", 20), "gui never entered"
        time.sleep(1.2)
        check("09 gui entered", True)

        # ---- 10 start menu -> terminal (item 0) -----------------------
        cur.moveto(34, ty + 14)
        cur.click()
        assert q.wait_serial_marker("gui: start menu open", 15), "menu missing"
        time.sleep(0.4)
        cur.moveto(100, my + 4 + 0 * 20 + 10)   # item 0: terminal
        cur.click()
        deadline = time.time() + 15
        term_id = None
        while time.time() < deadline:
            m = re.search(r"gui: terminal window id=(\d+)", read_log(q)[log_len(q) - 3000:])
            if m:
                term_id = int(m.group(1))
                break
            time.sleep(0.2)
        check("10 terminal window opened", term_id is not None, f"id={term_id}")
        time.sleep(0.8)

        # terminal geometry: parse the real on-screen position from klog
        # (cascade placement from the open system monitor window)
        m = re.search(r"gui: terminal window id=(\d+) opened for pid \d+ at \((\d+),(\d+)\) (\d+)x(\d+)", read_log(q))
        assert m, "terminal window position not logged"
        tx, tyy = int(m.group(2)), int(m.group(3))
        in_x0, in_y0 = tx + 8, tyy + TITLE_H + 5
        in_w = (TERM_W - 16) // 8 * 8
        rows = (TERM_H - TITLE_H - 2 - 10) // 14
        live_y = in_y0 + (rows - 1) * 14
        live_box = (in_x0, live_y - 3, in_x0 + in_w, live_y + 13)
        content_box = (in_x0, in_y0, in_x0 + in_w, live_y + 13)

        # ---- 11 kterm: pipeline baseline (oracle = 5) ------------------
        n0 = log_len(q)
        q.type_text("run ECHO.ELF hi05 | run UPPER.ELF")
        time.sleep(0.5)
        send_key(q, "ret")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("11 kterm pipeline baseline", code == 5, f"exit {code}, want 5 (hi05\\n)")
        time.sleep(1.0)

        # ---- 12 kterm: recall renders the line (pixel) ------------------
        # the live line sits directly under the last closed line, so we
        # measure the whole content area instead of guessing the row
        shot_a = q.screendump(os.path.join(SHOTS, "12-term-fresh-prompt"))
        send_key(q, "up")
        time.sleep(0.5)
        shot_b = q.screendump(os.path.join(SHOTS, "12-term-recalled"))
        ia, ib = Image.open(shot_a).convert("RGB"), Image.open(shot_b).convert("RGB")
        fresh, recalled = bright_px(ia, content_box), bright_px(ib, content_box)
        check("12 kterm recall rendered on the live line",
              recalled > fresh + 100, f"{fresh} -> {recalled} px")

        # ---- 13 kterm: recall + HOME + RIGHT*16 + insert = 6 ------------
        n0 = log_len(q)
        send_key(q, "home")
        for _ in range(16):
            send_key(q, "right")     # caret before the '5' of hi05
        q.type_text("5")             # -> hi055
        send_key(q, "ret")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("13 kterm recall+home+right+insert", code == 6, f"exit {code}, want 6 (hi055\\n)")

        # ---- 14 kterm: recall + LEFT + DEL + END + append = 6 -----------
        n0 = log_len(q)
        send_key(q, "up")            # recall 'run ECHO.ELF hi055 | run UPPER.ELF'
        send_key(q, "left")          # caret before the final 'F'
        send_key(q, "delete")        # -> ...UPPER.EL
        send_key(q, "end")
        q.type_text("F")             # restore -> ...UPPER.ELF
        send_key(q, "ret")
        code = exit_code_for(q, "UPPER.ELF", n0, 60)
        check("14 kterm recall+left+DEL+end+append", code == 6, f"exit {code}, want 6 (hi055\\n)")

        # ---- 15 start menu -> text editor (item 4) ----------------------
        cur.moveto(34, ty + 14)
        cur.click()
        assert q.wait_serial_marker("gui: start menu open", 15), "menu missing"
        time.sleep(0.4)
        cur.moveto(6 + 94, my + 4 + 4 * 20 + 10)
        cur.click()
        assert q.wait_serial_marker("gui: text editor spawned as pid", 20), \
            "text editor not spawned"
        time.sleep(1.0)
        check("15 EDIT spawned from start menu", True)

        cx, cy = EDIT_X + 2, EDIT_Y + 23
        cw, ch = EDIT_W - 4, EDIT_H - 24
        text_box = (cx, cy + 20, cx + cw, cy + ch - 18)
        status_box = (cx, cy + ch - 18, cx + cw, cy + ch)

        # ---- 16 type three lines -----------------------------------------
        cur.moveto(cx + 60, cy + 24 + 7)
        cur.click()
        time.sleep(0.4)
        q.type_text("AAA")
        send_key(q, "ret")
        q.type_text("BBB")
        send_key(q, "ret")
        q.type_text("CCC")
        time.sleep(0.6)
        s1 = q.screendump(os.path.join(SHOTS, "16-edit-3-lines"))
        i1 = Image.open(s1).convert("RGB")
        check("16 EDIT three lines typed", bright_px(i1, text_box) > 150,
              f"{bright_px(i1, text_box)} px")

        # ---- 17 UP UP HOME: caret to line 1 col 0 -------------------------
        # proof of movement = the LN/COL digits in the status bar change
        # (the caret itself may be in an off-blink phase on either shot)
        send_key(q, "up")
        send_key(q, "up")
        send_key(q, "home")
        time.sleep(0.5)
        s2 = q.screendump(os.path.join(SHOTS, "17-edit-upup-home"))
        i2 = Image.open(s2).convert("RGB")
        st_diff = px_diff(i1, i2, status_box)
        check("17 EDIT UP/UP/HOME moved caret", st_diff > 3, f"status diff {st_diff}")

        # ---- 18 RIGHT + BACKSPACE: decisive glyph-count oracle ------------
        # caret col1 + backspace removes 'A' (3 glyphs -> 2); if RIGHT was
        # ignored, the caret sits at col0 and backspace is a no-op (3)
        send_key(q, "right")
        send_key(q, "backspace")
        time.sleep(0.5)
        s3 = q.screendump(os.path.join(SHOTS, "18-edit-right-backspace"))
        i3 = Image.open(s3).convert("RGB")
        t2, t3 = bright_px(i2, text_box), bright_px(i3, text_box)
        check("18 EDIT RIGHT + backspace deleted a char", t3 < t2 - 15, f"{t2} -> {t3} px")
        q.type_text("A")            # restore -> AAA (caret col1)
        send_key(q, "end")          # caret col3 (also exercises END)
        time.sleep(0.4)

        # ---- 19 HOME + RIGHT*2 + mid-line insert '> ' ----------------------
        send_key(q, "home")
        send_key(q, "right")
        send_key(q, "right")
        q.type_text("> ")         # -> AA> A, caret col4
        time.sleep(0.5)
        s4 = q.screendump(os.path.join(SHOTS, "19-edit-insert-midline"))
        i4 = Image.open(s4).convert("RGB")
        t4 = bright_px(i4, text_box)
        check("19 EDIT mid-line insert", t4 > t3 + 5, f"{t3} -> {t4} px")

        # ---- 20 DEL forward-deletes the char at the caret ------------------
        send_key(q, "delete")
        time.sleep(0.5)
        s5 = q.screendump(os.path.join(SHOTS, "20-edit-forward-del"))
        i5 = Image.open(s5).convert("RGB")
        t5 = bright_px(i5, text_box)
        check("20 EDIT forward delete", t5 < t4 - 10, f"{t4} -> {t5} px")

        # ---- 21 PgDn / PgUp page jumps -------------------------------------
        send_key(q, "pgdn")
        time.sleep(0.5)
        s6 = q.screendump(os.path.join(SHOTS, "21-edit-pgdn"))
        i6 = Image.open(s6).convert("RGB")
        d65 = px_diff(i6, i5, status_box)
        send_key(q, "pgup")
        time.sleep(0.5)
        s7 = q.screendump(os.path.join(SHOTS, "21-edit-pgup"))
        i7 = Image.open(s7).convert("RGB")
        d76 = px_diff(i7, i6, status_box)
        check("21 EDIT PgDn/PgUp move caret", d65 > 3 and d76 > 3,
              f"pgdn diff {d65}, pgup diff {d76}")

        # ---- 22 [x] closes EDIT -> exit 0 ----------------------------------
        n1 = log_len(q)
        cur.moveto(EDIT_X + EDIT_W - 24 + 9, EDIT_Y + 4 + 7)
        cur.click()
        code = None
        deadline = time.time() + 20
        while time.time() < deadline:
            tail = read_log(q)[n1:]
            idx = tail.find("gui: close text editor window")
            if idx >= 0:
                mm = re.search(r"task (\d+) exited with code (-?\d+)", tail[idx:])
                if mm:
                    code = int(mm.group(2))
                    break
            time.sleep(0.2)
        check("22 EDIT [x] -> exit 0", code == 0, f"exit {code}")

        # ---- 23 esc -> console, FILES regression ---------------------------
        send_key(q, "esc", pause=0.3)
        assert q.wait_serial_marker("gui: exit reason=esc", 20), "esc did not exit gui"
        time.sleep(0.8)
        n0 = log_len(q)
        q.type_text("run FILES.ELF /HOME\n")
        code = exit_code_for(q, "FILES.ELF", n0, 60)
        check("23 FILES /HOME regression (no save -> 1 entry)", code == 1, f"exit {code}")

    finally:
        q.quit()

    passed = sum(1 for _, ok in results if ok)
    print(f"\n== v2.5: {passed}/{len(results)} checks passed ==")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
