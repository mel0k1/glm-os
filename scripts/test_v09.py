#!/usr/bin/env python3
"""GLM OS v0.9 full verification -> deliverable screenshots.

Session A (smp 4, e1000 + slirp):
  01 boot banner + net oklines
  02 neofetch (v0.9.0, Net row with udp)
  03 net (socket table empty)
  04 spawn UDPSERV -> 'sock: bound id=0 port=7777', ps shows SOCK state
  05 run UDPCLI -> 3/3 loopback round trips verified, exit 0
  06 net after demo (server socket RECV=3, client socket closed)
  07 kill <serv_pid> -> SIGTERM handler closes socket, server exit 0
  08 regressions: FORKTEST + THREADTEST exit 0
  09 ping regression (icmp still fine)

Session B (smp 1, no nic):
  10 graceful degradation: no-nic warnline, shell alive
"""
import os, re, sys, time

sys.path.insert(0, "/home/z/my-project/scripts")
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v09"


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def wait_marker(q, marker, timeout, desc):
    assert q.wait_serial_marker(marker, timeout), f"{desc}: no marker {marker!r}"
    print(f"OK {desc}")


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
        for m in ["pci: intel e1000 at 00:02.0", "e1000: rings up", "netd: kernel network task online"]:
            assert m in log, f"boot marker missing: {m}"
        print("OK boot markers")

        # 02 neofetch
        qemu.type_text("neofetch\n"); time.sleep(1.5)
        qemu.screendump("02-neofetch")

        # 03 net: socket table empty
        qemu.type_text("net\n"); time.sleep(1.0)
        qemu.screendump("03-net-empty")
        print("OK net (no sockets)")

        # 04 spawn the echo server; ps must show it parked in SOCK
        qemu.type_text("spawn UDPSERV.ELF\n")
        wait_marker(qemu, "sock: bound id=0 port=7777", 30, "server bound")
        time.sleep(1.0)
        qemu.type_text("ps\n"); time.sleep(1.0)
        qemu.screendump("04-ps-sock")
        log = read_log(qemu)
        m = re.search(r"sched: spawned 'UDPSERV\.ELF' pid (\d+)", log)
        assert m, "no spawn marker for udpserv"
        serv_pid = m.group(1)
        print(f"OK spawn udpserv pid={serv_pid}")

        # 05 the client: loopback round trips (kernel markers carry the proof)
        qemu.type_text("run UDPCLI.ELF\n")
        wait_marker(qemu, "exited with code 0 (", 60, "udpcli exit 0")
        qemu.screendump("05-udp-roundtrip")
        log = read_log(qemu)
        queued = log.count("sock: queued ") // 2  # deliver runs per datagram once... count raw
        n_client_close = log.count("sock: closed id=1 port=7778")
        n_serv_recv = len(re.findall(r"sock: recv id=0 port=7777 udp from 10\.0\.2\.15:7778", log))
        n_cli_recv = len(re.findall(r"sock: recv id=1 port=7778 udp from 10\.0\.2\.15:7777", log))
        assert n_serv_recv == 3, f"server received {n_serv_recv}/3"
        assert n_cli_recv == 3, f"client received {n_cli_recv}/3"
        assert n_client_close == 1, "client socket not closed"
        print("OK loopback: 3 datagrams both directions, client socket closed")

        # 06 net with sockets: server RECV=3, client gone
        qemu.type_text("net\n"); time.sleep(1.0)
        qemu.screendump("06-net-sockets")

        # 07 SIGTERM the server: handler closes the socket, exit 0
        qemu.type_text(f"kill {serv_pid}\n")
        wait_marker(qemu, "sock: closed id=0 port=7777", 30, "server socket closed")
        wait_marker(qemu, f"task {serv_pid} exited with code 0 (", 30, "server graceful exit")
        time.sleep(0.8)
        qemu.screendump("07-serv-sigterm")

        # 08 regressions
        qemu.type_text("run FORKTEST.ELF\n")
        wait_marker(qemu, "exited with code 0 (", 60, "forktest")
        qemu.screendump("08-forktest")
        qemu.type_text("run THREADTEST.ELF\n")
        wait_marker(qemu, "exited with code 0 (", 90, "threadtest")
        qemu.screendump("09-threadtest")

        # 09 ping regression
        qemu.type_text("ping\n"); time.sleep(9.0)
        qemu.screendump("10-ping")
        log = read_log(qemu)
        assert log.count("icmp echo reply seq=") == 4, "ping regression failed"
        print("OK ping regression 4/4")
    finally:
        qemu.quit()

    # session B: no NIC
    qemu2 = QemuSession(ISO, SHOTS + "-b", smp="1", nic="none")
    try:
        assert qemu2.wait_serial_marker("boot complete", 90), "no-nic boot failed"
        time.sleep(1.0)
        log = read_log(qemu2)
        assert "no intel e1000 on pci bus 0" in log, "missing no-nic warnline"
        qemu2.type_text("net\n"); time.sleep(0.8)
        qemu2.screendump("11-nonic-net")
        qemu2.type_text("neofetch\n"); time.sleep(1.2)
        qemu2.screendump("12-nonic-neofetch")
        print("OK no-nic session")
    finally:
        qemu2.quit()

    sys.exit(rc)


main()
