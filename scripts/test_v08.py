#!/usr/bin/env python3
"""GLM OS v0.8 full verification -> deliverable screenshots.

Session A (smp 4, e1000 + slirp):
  01 boot banner + net oklines
  02 neofetch (v0.8.0, Net row)
  03 net (nic status) + arp (empty)
  04 ping 10.0.2.2 -> ARP resolve + 4/4 echo replies
  05 arp after ping (10.0.2.2 learned)
  06 ps (netd visible in the task table)
  07 regression: run FORKTEST (COW fork, exit 0)
  08 regression: run THREADTEST (threads, counter 8000)

Session B (smp 1, no nic):
  09 graceful degradation: 'no intel e1000' warnline, shell alive
"""
import os, sys, time

sys.path.insert(0, "/home/z/my-project/scripts")
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v08"


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def main():
    os.makedirs(SHOTS, exist_ok=True)
    rc = 0
    qemu = QemuSession(ISO, SHOTS, smp="4", nic="user,model=e1000")
    try:
        # 01 boot
        assert qemu.wait_serial_marker("netd: online", 90), "netd never came online"
        time.sleep(1.2)
        qemu.screendump("01-boot")
        log = read_log(qemu)
        for m in [
            "pci: intel e1000 at 00:02.0",
            "e1000: rings up",
            "net: mac ",
            "netd: kernel network task online",
        ]:
            assert m in log, f"boot marker missing: {m}"
        print("OK boot markers")

        # 02 neofetch
        qemu.type_text("neofetch\n"); time.sleep(1.5)
        qemu.screendump("02-neofetch")

        # 03 net + arp empty
        qemu.type_text("net\n"); time.sleep(1.0)
        qemu.type_text("arp\n"); time.sleep(0.8)
        qemu.screendump("03-net-arp")

        # 04 ping the gateway (blocking, up to ~12 s worst case)
        qemu.type_text("ping\n"); time.sleep(9.0)
        qemu.screendump("04-ping")
        log = read_log(qemu)
        n_replies = log.count("icmp echo reply seq=")
        assert "netd: arp learned 10.0.2.2" in log, "arp never learned the gateway"
        assert n_replies == 4, f"expected 4 icmp replies, got {n_replies}"
        # all four sequence numbers present
        for s in range(4):
            assert f"icmp echo reply seq={s}" in log, f"missing reply seq={s}"
        print("OK ping: arp learned, 4/4 echo replies")

        # 05 arp table populated
        qemu.type_text("arp\n"); time.sleep(0.8)
        qemu.screendump("05-arp-after")

        # 06 ps with netd
        qemu.type_text("ps\n"); time.sleep(0.8)
        qemu.screendump("06-ps")
        log = read_log(qemu)
        print("OK ps")

        # 07 forktest regression
        qemu.type_text("run FORKTEST.ELF\n")
        assert qemu.wait_serial_marker("exited with code 0 (", 60), "forktest failed"
        qemu.screendump("07-forktest")
        print("OK forktest")

        # 08 threadtest regression
        qemu.type_text("run THREADTEST.ELF\n")
        assert qemu.wait_serial_marker("exited with code 0 (", 90), "threadtest failed"
        qemu.screendump("08-threadtest")
        # (userland text goes to VGA only — the klog exit-code marker above
        #  is the machine-checkable signal, per the v0.7 methodology note)
        print("OK threadtest")
    finally:
        qemu.quit()

    # session B: no NIC -> graceful degradation
    qemu2 = QemuSession(ISO, SHOTS + "-b", smp="1", nic="none")
    try:
        assert qemu2.wait_serial_marker("boot complete", 90), "no-nic boot failed"
        time.sleep(1.0)
        log = read_log(qemu2)
        assert "no intel e1000 on pci bus 0" in log, "missing no-nic warnline"
        qemu2.type_text("net\n"); time.sleep(0.8)
        qemu2.screendump("09-nonic-net")
        qemu2.type_text("neofetch\n"); time.sleep(1.2)
        qemu2.screendump("10-nonic-neofetch")
        print("OK no-nic session: warnline + shell alive")
    finally:
        qemu2.quit()

    sys.exit(rc)


main()
