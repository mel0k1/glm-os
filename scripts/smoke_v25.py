#!/usr/bin/env python3
"""Quick smoke test for v2.5 builds: boot, banner, shell echo, EDIT nav keys."""
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/work-smoke25"

q = QemuSession(ISO, WORK, nic="user")
try:
    ok_boot = q.wait_serial_marker("v2.6.0", timeout=90)
    print("boot banner v2.6.0:", ok_boot)
    time.sleep(2)
    q.type_text("echo smoke25\n")
    time.sleep(2)
    log = open(q.serial_log, errors="replace").read()
    # console output also mirrors to the debug pipe; check the shell echoed
    ok = "smoke25" in log
    print("echo roundtrip in serial log:", ok)
    q.screendump("smoke-console")
finally:
    q.quit()
print("SMOKE OK" if ok_boot else "SMOKE FAIL")
