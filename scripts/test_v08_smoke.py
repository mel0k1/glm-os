#!/usr/bin/env python3
"""GLM OS v0.8 first net smoke: boot with e1000, net, arp, ping 10.0.2.2."""
import os, sys, time
sys.path.insert(0, "/home/z/my-project/scripts")
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v08-smoke"

def read_log(q):
    return open(q.serial_log, errors="replace").read()

def main():
    os.makedirs(SHOTS, exist_ok=True)
    qemu = QemuSession(ISO, SHOTS, smp="4", nic="user,model=e1000")
    rc = 0
    try:
        assert qemu.wait_serial_marker("netd: online", 90), "netd never came online"
        time.sleep(1.2)
        log = read_log(qemu)
        for line in log.splitlines():
            if "e1000" in line or "pci" in line or "netd" in line:
                print("BOOT:", line)
        # net status
        qemu.type_text("net\n"); time.sleep(1.0)
        qemu.screendump("01-net")
        # arp empty
        qemu.type_text("arp\n"); time.sleep(0.6)
        qemu.screendump("02-arp-empty")
        # ping the gateway
        qemu.type_text("ping\n"); time.sleep(6.0)
        qemu.screendump("03-ping")
        # arp populated
        qemu.type_text("arp\n"); time.sleep(0.6)
        qemu.screendump("04-arp-after")
        log = read_log(qemu)
        n_replies = log.count("icmp echo reply seq=")
        arp_ok = "netd: arp learned 10.0.2.2" in log
        print("ICMP replies seen in klog:", n_replies)
        print("ARP learned gateway:", arp_ok)
        if n_replies < 4 or not arp_ok:
            print("TAIL:", log[-1500:])
            rc = 1
    finally:
        qemu.quit()
    sys.exit(rc)

main()
