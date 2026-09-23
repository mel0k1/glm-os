#!/usr/bin/env python3
"""Quick smoke test for v1.7 exec — interactive sanity before the full suite."""
import os, re, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/build/smoke17"
os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = ("/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:" + os.environ.get("LD_LIBRARY_PATH", ""))

def read_log(q):
    return open(q.serial_log, errors="replace").read()

def parse_exit_code(q, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = re.search(r"exited with code (-?\d+)", read_log(q)[start:])
        if m:
            return int(m.group(1))
        if q.proc.poll() is not None:
            return None
        time.sleep(0.2)
    return None

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000",
                disk="/home/z/glm-os/build/disk.img")
try:
    assert q.wait_serial_marker("boot complete", 90), "boot failed"
    print("boot ok")
    time.sleep(0.5)

    n = len(read_log(q))
    q.type_text("run ARGS.ELF\n")
    print("args no-argv exit:", parse_exit_code(q, n), "(want 1)")

    n = len(read_log(q))
    q.type_text("run ARGS.ELF one two three\n")
    print("args 3-argv exit:", parse_exit_code(q, n), "(want 4)")

    n = len(read_log(q))
    q.type_text("drun ARGS.ELF a b c d\n")
    print("drun disk 4-argv exit:", parse_exit_code(q, n), "(want 5)")

    n = len(read_log(q))
    q.type_text("drun RUNIT.ELF ARGS.ELF x y\n")
    print("runit exec chain exit:", parse_exit_code(q, n), "(want 3)")

    n = len(read_log(q))
    q.type_text("drun RUNIT.ELF NOPE.ELF\n")
    print("runit exec-fail exit:", parse_exit_code(q, n), "(want 127)")

    log = read_log(q)
    for line in log.splitlines():
        if "exec:" in line:
            print("  KLOG:", line.strip())
finally:
    q.quit()
