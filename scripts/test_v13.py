#!/usr/bin/env python3
"""GLM OS v1.3 TCP test session -> deliverable screenshots.

Сценарий (smp 4 + e1000 + hostfwd 7301, одна сессия):
  01 boot (network up, boot complete)
  02 OUTBOUND: host echo-server (python, 127.0.0.1:7300) + guest TCPCLI.ELF
     -> полный handshake, данные, byte-for-byte эхо, exit 0 (klog: established)
  03 INBOUND: guest TCPSERV.ELF слушает 7301 (hostfwd), host-клиент шлёт,
     гость эхо-отвечает, host проверяет байты; 2 раунда -> сервер exit 0
  04 net: TCP-таблица видна (скриншот)
  05 регрессии: UDP-пара, ping 4/4, FORKTEST, THREADTEST
  06 gui enter/esc (регрессия GUI после сетевых тестов)
"""
import os
import re
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SHOTS = "/home/z/glm-os/shots-v13"

GUEST_MSG = b"hello from glm-os tcp - ring 3 speaking"  # what TCPCLI sends
MSG1 = b"hello over tcp from the HOST side - v1.3 test payload 12345"
MSG2 = b"second round: the listener survives a close and accepts again"


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def log_len(q):
    try:
        return len(open(q.serial_log, errors="replace").read())
    except FileNotFoundError:
        return 0


def wait_new_marker(q, marker, start, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def host_echo_server(port, events):
    """TCP echo server on the host: replies byte-for-byte until EOF."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.listen(2)
    events["ready"] = True
    while not events["stop"]:
        try:
            srv.settimeout(1.0)
            conn, addr = srv.accept()
        except socket.timeout:
            continue
        except OSError:
            break
        conn.settimeout(10)
        data = b""
        try:
            while True:
                chunk = conn.recv(1400)
                if not chunk:
                    break
                data += chunk
                conn.sendall(chunk)
            events["echoed"].append(data)
        except OSError as e:
            events["echo_err"] = str(e)
        finally:
            conn.close()
    srv.close()


def main():
    os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
    os.environ["LD_LIBRARY_PATH"] = (
        "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
        "/home/z/sysroot/lib/x86_64-linux-gnu:"
        + os.environ.get("LD_LIBRARY_PATH", "")
    )
    os.makedirs(SHOTS, exist_ok=True)
    nic = "user,model=e1000,hostfwd=tcp:127.0.0.1:7301-:7301"
    qemu = QemuSession(ISO, SHOTS, smp="4", nic=nic)
    rc = 0
    events = {"stop": False, "ready": False, "echoed": [], "echo_err": None}
    try:
        # 01 boot
        assert qemu.wait_serial_marker("boot complete", 90), "boot never completed"
        log = read_log(qemu)
        assert "netd: online" in log or "e1000" in log, "network not up"
        print("[ok] boot, network online")

        # 02 OUTBOUND: guest connects to the host echo server via slirp
        th = threading.Thread(target=host_echo_server, args=(7300, events), daemon=True)
        th.start()
        deadline = time.time() + 5
        while not events["ready"] and time.time() < deadline:
            time.sleep(0.05)
        assert events["ready"], "host echo server did not start"

        n0 = log_len(qemu)
        qemu.type_text("run TCPCLI.ELF\n")
        assert wait_new_marker(qemu, "tcp: syn sent id=", n0, 15), "no syn sent"
        assert wait_new_marker(qemu, "tcp: established id=", n0, 20), "handshake never established"
        assert wait_new_marker(qemu, "exited with code 0", n0, 25), "tcpcli did not exit 0"
        assert events["echoed"] and events["echoed"][0] == GUEST_MSG, \
            f"host echo mismatch: {events['echoed'][:1]}"
        line = [l for l in read_log(qemu).splitlines() if "tcp: closed" in l and "id=" in l][-1]
        print(f"[ok] outbound TCP: {line.strip()[:70]}")
        time.sleep(0.4)
        qemu.screendump("02-outbound-tcp")

        # 03 INBOUND: guest listens on 7301, host connects via hostfwd
        qemu.type_text("spawn TCPSERV.ELF\n")
        assert wait_new_marker(qemu, "tcp: listen id=", log_len(qemu) - 2000, 15), \
            "guest listener never bound"
        time.sleep(0.5)

        for rnd, msg in ((1, MSG1), (2, MSG2)):
            c = socket.create_connection(("127.0.0.1", 7301), timeout=15)
            c.settimeout(15)
            n_before = log_len(qemu)
            c.sendall(msg)
            got = b""
            while len(got) < len(msg):
                chunk = c.recv(1400)
                if not chunk:
                    break
                got += chunk
            c.close()
            assert got == msg, f"round {rnd}: guest echo mismatch ({len(got)}/{len(msg)} bytes)"
            assert wait_new_marker(qemu, "tcp: accepted id=", n_before, 15), \
                f"round {rnd}: guest never logged accept"
            print(f"[ok] inbound round {rnd}: {len(msg)} bytes echoed by the guest")
        # two rounds done -> tcpserv exits 0 on its own
        assert wait_new_marker(qemu, "exited with code 0", log_len(qemu) - 3000, 20), \
            "tcpserv did not exit after 2 rounds"
        print("[ok] tcpserv exited cleanly after 2 connections")
        time.sleep(0.4)
        qemu.screendump("03-inbound-tcp")

        # 04 net status shows the TCP table (empty now, but the header prints)
        qemu.type_text("net\n")
        time.sleep(1.0)
        qemu.screendump("04-net-status")

        # 05 regressions
        qemu.type_text("spawn UDPSERV.ELF\n")
        assert qemu.wait_serial_marker("sock: bound id=", 20), "udp server did not bind"
        n_udp = log_len(qemu)
        qemu.type_text("run UDPCLI.ELF\n")
        assert wait_new_marker(qemu, "exited with code 0", n_udp), "udpcli failed"
        print("[ok] userland udp regression")

        qemu.type_text("ping\n")
        deadline = time.time() + 25
        while read_log(qemu).count("icmp echo reply seq=") < 4 and time.time() < deadline:
            time.sleep(0.2)
        assert read_log(qemu).count("icmp echo reply seq=") >= 4, "ping did not get 4 replies"
        print("[ok] ping 4/4")

        qemu.type_text("run FORKTEST.ELF\n")
        assert qemu.wait_serial_marker("exited with code 42", 40), "forktest child failed"
        qemu.wait_serial_marker("exited with code 0", 20)
        print("[ok] forktest regression")

        qemu.type_text("spawn THREADTEST.ELF\n")
        n_thread = log_len(qemu)
        assert qemu.wait_serial_marker("exited with code 101", 40), "thread worker 101"
        assert wait_new_marker(qemu, "exited with code 0", n_thread), "threadtest main 0"
        print("[ok] threadtest regression")

        # 06 gui regression (quick enter/esc)
        qemu.type_text("gui\n")
        assert qemu.wait_serial_marker("gui: enter (double buffered", 15), "gui enter failed"
        time.sleep(1.0)
        qemu.screendump("06-gui")
        qemu.hmp("sendkey esc")
        assert qemu.wait_serial_marker("gui: exit reason=esc", 20), "gui esc failed"
        print("[ok] gui enter/esc regression")

        os.replace(qemu.serial_log, os.path.join(SHOTS, "kernel.log"))
        print("\nALL v1.3 CHECKS PASSED")
    except AssertionError as e:
        print(f"FAIL: {e}")
        try:
            qemu.screendump("FAIL-state")
        except Exception:
            pass
        rc = 1
    finally:
        events["stop"] = True
        qemu.proc.terminate()
        try:
            qemu.proc.wait(timeout=5)
        except Exception:
            qemu.proc.kill()
    sys.exit(rc)


if __name__ == "__main__":
    main()
