#!/usr/bin/env python3
"""v1.8 pipes + I/O redirection — full automated verification.

Machine-readable channel: the kernel exit klog ("sched: task N exited
with code C", one per pipeline stage) plus pipe lifecycle klogs. The
pipeline programs are PURE filters whose exit code = the byte count they
forwarded, so every check below verifies the payload end-to-end without
needing framebuffer OCR.

Covered:
  1.  two-stage pipe:            ECHO | READER        (reader exits 17)
  2.  three-stage pipe:          ECHO | UPPER | READER (reader exits 8)
  3.  stdout -> file:            WRITER > OUT.TXT; CAT dumps 35 bytes
  4.  stdin <- file:             READER < OUT.TXT     (exits 35)
  5.  pipe + redirect combined:  ECHO | UPPER > UP.TXT; CAT dumps 15
  6.  EOF chain:                 ECHO | READER | READER (14 / 14 / 0)
  7.  broken pipe:               SLEEPY | ARGS -> SLEEPY exit 1 + klog
  8.  background pipeline:       spawn ECHO | READER (exits 16)
  9.  pipe table command:        `pipe` prints an empty table at the end
 10. regressions:                argv (4), exec chain (3), exec fail (127)
"""
import os, re, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/build/test18"
DISK = "/home/z/glm-os/build/disk.img"

os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = ("/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:" + os.environ.get("LD_LIBRARY_PATH", ""))

PASS, FAIL = 0, 0
def check(name, ok, detail=""):
    global PASS, FAIL
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}" + (f"  ({detail})" if detail else ""))
    if ok: PASS += 1
    else: FAIL += 1

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

TASK_EXIT = re.compile(r"sched: task (\d+) exited with code (-?\d+)")

def collect_exits(q, start, nstages, timeout=90):
    """Wait until every stage's exit klog arrived (or timeout). Returns
    the list of exit codes in stage-spawn order."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if q.proc.poll() is not None:
            break
        codes = [int(c) for (_, c) in TASK_EXIT.findall(read_log(q)[start:])]
        if len(codes) >= nstages:
            return codes[:nstages]
        time.sleep(0.2)
    return [int(c) for (_, c) in TASK_EXIT.findall(read_log(q)[start:])][:nstages]

def run_line(q, line):
    n = len(read_log(q))
    q.type_text(line + "\n")
    return n

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK)
try:
    assert q.wait_serial_marker("boot complete", 120), "boot failed"
    print("boot ok")
    time.sleep(0.5)

    # --- 1. two-stage pipeline -------------------------------------------
    n = run_line(q, "run ECHO.ELF hello pipe world | run READER.ELF")
    codes = collect_exits(q, n, 2)
    check("echo|reader reader got 17 bytes", codes[-1:] == [17], f"got {codes}")
    check("echo|reader writer exit 0", codes[:1] == [0], f"got {codes}")
    ok_klog = wait_for(q, "pipe: slot 0 freed (17 bytes written, 17 read)", n)
    check("echo|reader byte accounting", ok_klog)

    # --- 2. three-stage with UPPER ---------------------------------------
    n = run_line(q, "run ECHO.ELF abc def | run UPPER.ELF | run READER.ELF")
    codes = collect_exits(q, n, 3)
    check("echo|upper|reader exits [0, 8, 8]", codes == [0, 8, 8], f"got {codes}")

    # --- 3. stdout to file, then cat --------------------------------------
    n = run_line(q, "run WRITER.ELF alpha beta > OUT.TXT")
    codes = collect_exits(q, n, 1)
    check("writer>file exit 0", codes == [0], f"got {codes}")
    n2 = run_line(q, "run CAT.ELF OUT.TXT")
    codes = collect_exits(q, n2, 1)
    check("cat dumps 38 bytes from file", codes == [38], f"got {codes}")

    # --- 4. stdin from file ------------------------------------------------
    n = run_line(q, "run READER.ELF < OUT.TXT")
    codes = collect_exits(q, n, 1)
    check("reader<file exits 38", codes == [38], f"got {codes}")

    # --- 5. pipe + file redirect combined ---------------------------------
    n = run_line(q, "run ECHO.ELF mixed pipeline | run UPPER.ELF > UP.TXT")
    codes = collect_exits(q, n, 2)
    check("echo|upper>file exits [0, 15]", codes == [0, 15], f"got {codes}")
    n2 = run_line(q, "run CAT.ELF UP.TXT")
    codes = collect_exits(q, n2, 1)
    check("cat dumps 15 piped+redirected bytes", codes == [15], f"got {codes}")

    # --- 6. EOF chain: reader feeding reader ------------------------------
    n = run_line(q, "run ECHO.ELF end-of-stream | run READER.ELF | run READER.ELF")
    codes = collect_exits(q, n, 3)
    check("eof chain exits [0, 14, 14]", codes == [0, 14, 14], f"got {codes}")
    ok_free = wait_for(q, "pipe: slot 1 freed (14 bytes written, 14 read)", n)
    check("eof chain second pipe accounting", ok_free)

    # --- 7. broken pipe -----------------------------------------------------
    n = run_line(q, "run SLEEPY.ELF | run ARGS.ELF one")
    codes = collect_exits(q, n, 2)
    check("broken pipe exits [2, 1]", codes == [2, 1], f"got {codes}")
    ok_klog = wait_for(q, "broken pipe on slot", n)
    check("broken pipe klog", ok_klog)

    # --- 8. background pipeline ---------------------------------------------
    n = run_line(q, "spawn ECHO.ELF background data | run READER.ELF")
    codes = collect_exits(q, n, 2)
    check("background pipeline reader got 16", codes[-1:] == [16], f"got {codes}")

    # --- 9. pipe table after everything drained ----------------------------
    n = run_line(q, "pipe")
    ok_empty = wait_for(q, "pipe: cmd: 0 open slot(s)", n)
    check("pipe table drains", ok_empty)

    # --- 10. regressions ----------------------------------------------------
    n = run_line(q, "run ARGS.ELF one two three")
    codes = collect_exits(q, n, 1)
    check("argv regression exit 4", codes == [4], f"got {codes}")

    n = run_line(q, "drun RUNIT.ELF ARGS.ELF x y")
    codes = collect_exits(q, n, 2)
    check("exec chain regression exits [3, 3]", codes == [3, 3], f"got {codes}")

    n = run_line(q, "drun RUNIT.ELF NOPE.ELF")
    codes = collect_exits(q, n, 2)
    check("exec fail regression exits [127, 127]", codes == [127, 127], f"got {codes}")

    print(f"\n===== v1.8 result: {PASS} passed, {FAIL} failed =====")
finally:
    q.quit()
