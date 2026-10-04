#!/usr/bin/env python3
"""v2.8 smoke: boot dhcp + tab completion + ctrl+d eof (console only)."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/work-smoke28"

def log():
    return open(q.serial_log, errors="replace").read()

q = QemuSession(iso=ISO, workdir=WORK, nic="user,model=e1000")
try:
    ok = q.wait_serial_marker("GLM OS v2.8.0 ready", timeout=60)
    print("boot banner:", ok)
    time.sleep(3)  # let the boot lease finish

    L = log()
    print("boot dhcp spawn klog:", "boot dhcp: client pid" in L)
    print("auto setconf klog:", "net: config set from ring 3: ip=10.0.2.15" in L.split("handing over to glmsh")[-1])
    print("dhcp applied line:", "the address was LEARNED, exit 0" in L)

    # ---- TAB: unique command completion ----
    q.type_text("neo")
    q.hmp("sendkey tab"); time.sleep(0.3)
    q.type_text("\n")
    ok = q.wait_serial_marker("OS:", timeout=10)
    L = log()
    print("tab neofetch worked:", ok and "glm@glm-os" in L)

    # ---- TAB: unique ELF completion (run HEL -> HELLO.ELF) ----
    q.type_text("run hel")
    q.hmp("sendkey tab"); time.sleep(0.3)
    q.type_text("\n")
    ok = q.wait_serial_marker("Hello", timeout=10)
    print("tab run HELLO.ELF:", ok)

    # ---- TAB: ambiguous -> candidates listed, LCP kept ----
    q.type_text("pi")
    q.hmp("sendkey tab"); time.sleep(0.4)
    q.type_text("\n")  # submit "pi" (unknown command) just to see the listing
    time.sleep(0.5)
    L = log()
    tail = L[-3000:]
    print("tab candidates listed:", "ping" in tail and "pipe" in tail)

    # ---- Ctrl+D at empty prompt: hint, console survives ----
    q.hmp("sendkey ctrl-d"); time.sleep(0.4)
    q.type_text("echo alive\n")
    ok = q.wait_serial_marker("alive", timeout=10)
    L = log()
    print("console eof hint:", "eof: this console stays up" in L)
    print("console survives eof:", ok)

    # ---- CAT + Ctrl+D as EOF ----
    q.type_text("run CAT\n")
    time.sleep(0.5)
    q.type_text("hi")
    q.hmp("sendkey ctrl-d"); time.sleep(0.6)
    L = log()
    tail = L[-2500:]
    got = ("exited with code 2" in tail) or ("code 2" in tail)
    print("cat eof exit 2:", got, "| tail:", tail[-400:].replace("\n", " | ") if not got else "")
finally:
    q.quit()
