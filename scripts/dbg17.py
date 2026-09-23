#!/usr/bin/env python3
"""Debug: screendump ARGS output to see actual argv strings."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/build/dbg17"
os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = ("/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:" + os.environ.get("LD_LIBRARY_PATH", ""))

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000",
                disk="/home/z/glm-os/build/disk.img")
try:
    assert q.wait_serial_marker("boot complete", 90), "boot failed"
    time.sleep(0.5)
    q.type_text("run ARGS.ELF one two three\n")
    time.sleep(2.5)
    print(q.screendump("dbg-args"))
    q.type_text("drun RUNIT.ELF ARGS.ELF x y\n")
    time.sleep(2.5)
    print(q.screendump("dbg-runit"))
finally:
    q.quit()
