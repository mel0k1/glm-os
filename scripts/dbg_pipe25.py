#!/usr/bin/env python3
"""Isolate the kterm pipe 139: run the same pipeline N times in one terminal."""
import os
import re
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/work-dbg-pipe"
TASKBAR_H, MENU_H = 28, 172
SPAWN = re.compile(r"sched: spawned '([^']+)' pid (\d+)")
EXITL = re.compile(r"sched: task (\d+) exited with code (-?\d+)")


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def code_for(q, name, start, timeout=30):
    log = read_log(q)[start:]
    pids = [int(m.group(2)) for m in SPAWN.finditer(log) if m.group(1) == name]
    if not pids:
        return None
    pid = pids[-1]
    deadline = time.time() + timeout
    while time.time() < deadline:
        for m in EXITL.finditer(read_log(q)[start:]):
            if int(m.group(1)) == pid:
                return int(m.group(2))
        time.sleep(0.15)
    return None


q = QemuSession(ISO, WORK, smp="4", nic="user")
try:
    assert q.wait_serial_marker("boot complete", 90)
    time.sleep(1.5)

    # --- replicate the console phase of test_v25 (history + editing) ---
    def sk(k, p=0.09):
        q.hmp(f"sendkey {k}"); time.sleep(p)

    n0 = len(read_log(q))
    q.type_text("run ECHO.ELF hi15 | run UPPER.ELF\n")
    print("console baseline:", code_for(q, "UPPER.ELF", n0, 40))
    n0 = len(read_log(q))
    sk("up"); sk("home")
    for _ in range(15): sk("right")
    q.type_text("1")
    sk("end"); q.type_text("\n")
    print("console edited:", code_for(q, "UPPER.ELF", n0, 40))
    n0 = len(read_log(q))
    sk("up"); sk("home")
    for _ in range(15): sk("right")
    sk("delete"); q.type_text("\n")
    print("console recall+del:", code_for(q, "UPPER.ELF", n0, 40))
    n0 = len(read_log(q))
    q.type_text("run ECHO.ELF hi158")
    sk("backspace")
    q.type_text(" | run UPPER.ELF\n")
    print("console bs+append:", code_for(q, "UPPER.ELF", n0, 40))

    q.type_text("gui\n")
    assert q.wait_serial_marker("gui: enter", 20)
    time.sleep(1.2)
    W, H = 1280, 800
    ty = H - TASKBAR_H
    my = ty - MENU_H - 4
    q.hmp(f"mouse_move {34 - W // 2} 0"); time.sleep(0.2)
    q.hmp(f"mouse_move 0 {ty + 14 - H // 2}"); time.sleep(0.2)
    q.hmp("mouse_button 1"); time.sleep(0.15); q.hmp("mouse_button 0"); time.sleep(0.6)
    q.hmp(f"mouse_move {100 - 34} 0"); time.sleep(0.3)
    q.hmp(f"mouse_move 0 {my + 14 - (ty + 14)}"); time.sleep(0.3)
    q.hmp("mouse_button 1"); time.sleep(0.15); q.hmp("mouse_button 0"); time.sleep(1.2)

    for i in range(6):
        n0 = len(read_log(q))
        q.type_text(f"run ECHO.ELF hi05 | run UPPER.ELF\n")
        code = code_for(q, "UPPER.ELF", n0, 40)
        print(f"run {i+1}: UPPER exit = {code}")
        time.sleep(0.5)
    # and the same 6x on the text console after leaving the GUI
    q.hmp("sendkey esc"); time.sleep(2.0)
    for i in range(3):
        n0 = len(read_log(q))
        q.type_text(f"run ECHO.ELF hi05 | run UPPER.ELF\n")
        code = code_for(q, "UPPER.ELF", n0, 40)
        print(f"console run {i+1}: UPPER exit = {code}")
        time.sleep(0.5)
finally:
    q.quit()
