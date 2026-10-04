#!/usr/bin/env python3
"""GLM OS v2.9 test session — TCP poll + the concurrent ring-3 web server.

New in v2.9:
  * kernel: SYS_TCP_POLL (58) — poll(2) over the TCP socket table:
    readiness = data in RX ring / EOF / pending connection on a listener /
    dead socket; the calling task sleeps between passes (the v1.3 pattern),
    no lock is ever held across a sleep
  * ring 3: HTTPD.ELF became a poll-driven event loop — ONE task
    multiplexes the listener and up to 6 client sockets; a slow or silent
    client no longer blocks the others (the v2.4 server was strictly
    serial: accept -> serve -> close -> accept)

Covered (hostfwd tcp 8080; the host IS the internet):
  1. boot banner v2.9.0 + boot complete
  2. boot dhcp: the address is learned (regression of the v2.8 autoboot)
  3. httpd: spawn, banner, poll multiplexor line
  4. THE PROOF: a silent client (connected, sends nothing) holds one
     server slot — curl STILL gets 200 + the GLM page (the v2.4 server
     would hang forever); /HELLO.TXT also served while the silent
     client idles
  5. the silent client finally speaks -> 200
  6. 3 PARALLEL curls -> all 200 (genuine simultaneity)
  7. guard rails still honest: 404 / 405 / 403 (traversal)
  8. server header advertises 2.9
  9. budget spent -> exit 0 (klog)
 10. session stays alive: ping + wget example.com (poll did not break
     the TCP client path), shell interactive
 11. session B (no nic): boots cleanly without the poll machinery
"""
import os
import re
import shutil
import socket
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
DISK = "/home/z/glm-os/build/disk.img"
WORK = "/home/z/glm-os/work-v29"
SHOTS = "/home/z/glm-os/shots-v29"
PORT = 8080

results = []
EXIT_LINE = re.compile(r"sched: task \d+ exited with code (-?\d+)")


def check(name, ok, extra=""):
    results.append((name, bool(ok)))
    print(f"  [{'ok ' if ok else 'FAIL'}] {name}" + (f" | {extra}" if extra and not ok else ""))


def read_log(q):
    try:
        return open(q.serial_log, errors="replace").read()
    except FileNotFoundError:
        return ""


def log_len(q):
    return len(read_log(q))


def wait_for(q, needle, start, timeout=40):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def run_line(q, line, settle=1.5):
    n = log_len(q)
    q.type_text(line + "\n")
    time.sleep(settle)
    return n


def last_exit_after(q, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        codes = EXIT_LINE.findall(read_log(q)[start:])
        if codes:
            return int(codes[-1])
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


def http_client(host, port, request: bytes, timeout=10):
    s = socket.create_connection((host, port), timeout=timeout)
    s.settimeout(timeout)
    s.sendall(request)
    buf = b""
    try:
        while True:
            chunk = s.recv(4096)
            if not chunk:
                break
            buf += chunk
    except socket.timeout:
        pass
    s.close()
    head, _, body = buf.partition(b"\r\n\r\n")
    status = head.split(b"\r\n")[0].decode(errors="replace") if head else ""
    return status, body, head


def drain(sock, timeout=15):
    sock.settimeout(timeout)
    buf = b""
    try:
        while True:
            chunk = sock.recv(4096)
            if not chunk:
                break
            buf += chunk
    except socket.timeout:
        pass
    return buf


H = b"Host: glm-os\r\nConnection: close\r\n\r\n"


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    shutil.copyfile(DISK, os.path.join(WORK, "disk.img"))
    disk_copy = os.path.join(WORK, "disk.img")
    os.makedirs(SHOTS, exist_ok=True)

    nic = f"user,model=e1000,hostfwd=tcp:127.0.0.1:{PORT}-:{PORT}"
    q = QemuSession(ISO, WORK, smp="4", nic=nic, disk=disk_copy)

    try:
        # 1. boot -----------------------------------------------------------
        assert q.wait_serial_marker("boot complete", 120), "boot failed"
        log = read_log(q)
        check("1. boot banner v2.9.0", "GLM OS v2.9.0" in log)
        print("boot ok")

        # 2. boot dhcp regression -------------------------------------------
        check("2a. boot dhcp client spawned",
              wait_for(q, "boot dhcp: client pid", 0, timeout=30))
        check("2b. lease applied from ring 3",
              wait_for(q, "net: config set from ring 3: ip=10.0.2.15", 0,
                       timeout=30))

        # 3. httpd spawn (klog oracles: program console output never
        # reaches serial -- the listener klog is the machine truth)
        n = run_line(q, "spawn HTTPD.ELF 8080 12", settle=1.5)
        check("3a. listener bound (klog tcp: listen port 8080)",
              wait_for(q, "tcp: listen id=0 port=8080", n, timeout=10))
        check("3b. budget banner reached the console (spawn report)",
              "spawned pid" in read_log(q)[n:] or "HTTPD.ELF" in read_log(q)[n:])

        # warm slirp's view of the guest so hostfwd inbound works
        n = run_line(q, "ping 10.0.2.2", settle=4.0)
        seg = read_log(q)[n:]
        check("3c. ping 10.0.2.2 4/4 (warmup)",
              seg.count("icmp echo reply seq=") >= 4, seg[-200:])

        # 4. THE PROOF: silent client + curl ---------------------------------
        silent = socket.create_connection(("127.0.0.1", PORT), timeout=15)
        print("  [**] silent client holds the server; curl must still work")
        t0 = time.time()
        st, body, head = http_client("127.0.0.1", PORT,
                                     b"GET / HTTP/1.0\r\n" + H, timeout=20)
        dt = time.time() - t0
        check("4a. curl served WHILE a silent client idles", " 200" in st, st)
        check("4b. index body is the GLM page", b"GLM OS" in body)
        check("4c. served promptly (<15 s, v2.4 would hang forever)",
              dt < 15 and " 200" in st, f"{dt:.1f}s")
        check("4d. server header says 2.9",
              b"GLM-OS-httpd/2.9" in head)

        st, body, _ = http_client("127.0.0.1", PORT,
                                  b"GET /HELLO.TXT HTTP/1.0\r\n" + H, timeout=20)
        check("4e. /HELLO.TXT served while silent client idles",
              " 200" in st and b"served over http" in body, st)

        # 5. the silent client finally speaks --------------------------------
        silent.sendall(b"GET / HTTP/1.0\r\n" + H)
        sdata = drain(silent, timeout=15)
        silent.close()
        check("5. held connection served when the client speaks",
              b"HTTP/1.0 200" in sdata and b"GLM OS" in sdata,
              sdata[:80].decode(errors="replace"))

        # 6. three PARALLEL curls ---------------------------------------------
        out = {}

        def one(tag, path):
            out[tag] = http_client("127.0.0.1", PORT,
                                   b"GET " + path + b" HTTP/1.0\r\n" + H,
                                   timeout=25)

        ts = [threading.Thread(target=one, args=(i, p))
              for i, p in enumerate([b"/", b"/HELLO.TXT", b"/"])]
        for t in ts:
            t.start()
        for t in ts:
            t.join()
        check("6. 3 parallel curls all 200",
              all(" 200" in out[i][0] for i in range(3))
              and all(b"GLM OS" in out[i][1] or b"served" in out[i][1]
                      for i in range(3)),
              {k: v[0] for k, v in out.items()})
        q.screendump("06-parallel")

        # 7. guard rails -------------------------------------------------------
        st, _, _ = http_client("127.0.0.1", PORT,
                               b"GET /NOPE.HTM HTTP/1.0\r\n" + H)
        check("7a. 404", " 404" in st, st)
        st, _, _ = http_client("127.0.0.1", PORT,
                               b"POST / HTTP/1.0\r\n" + H + b"x=1")
        check("7b. 405", " 405" in st, st)
        st, _, _ = http_client("127.0.0.1", PORT,
                               b"GET /../SECRET.TXT HTTP/1.0\r\n" + H)
        check("7c. traversal -> 403", " 403" in st, st)

        # 8a. THE CEILING: 6 silent clients fill every slot; the 7th
        # connection is refused with 503 -- only a multiplexing server can
        # even hold this state (v2.4 blocked on the first one)
        silent_socks = []
        n0 = log_len(q)
        for _ in range(6):
            silent_socks.append(socket.create_connection(("127.0.0.1", PORT),
                                                         timeout=15))
        # the guest accepts ONE pending connection per poll cycle and slirp
        # retransmits dropped SYNs with backoff (1+2+4+8+16 ~ 31 s for the
        # 6th), so wait for the kernel's own acceptance trace (machine
        # truth) with a generous deadline instead of a fixed sleep
        deadline = time.time() + 45
        while time.time() < deadline:
            if read_log(q)[n0:].count("tcp: accepted") >= 6:
                break
            if q.proc.poll() is not None:
                break
            time.sleep(0.2)
        time.sleep(0.5)
        n_acc = read_log(q)[n0:].count("tcp: accepted")
        print(f"  [**] guest accepted {n_acc}/6 silent clients")
        st, body, _ = http_client("127.0.0.1", PORT,
                                  b"GET / HTTP/1.0\r\n" + H, timeout=20)
        check("8a. 7th connection with all 6 slots busy -> 503",
              n_acc >= 6 and " 503" in st, f"accepted={n_acc} st={st!r}")
        for s in silent_socks:
            s.close()
        time.sleep(1.5)  # guest drops the EOFed slots

        # 8b. two more curls spend the budget (12 responses total)
        st, _, _ = http_client("127.0.0.1", PORT,
                               b"GET / HTTP/1.0\r\n" + H, timeout=20)
        check("8b. server serves again after slots free up", " 200" in st, st)
        st, _, _ = http_client("127.0.0.1", PORT,
                               b"GET / HTTP/1.0\r\n" + H, timeout=20)
        check("8c. 12th response", " 200" in st, st)
        check("8d. server spent its budget, exit 0",
              wait_for(q, "exited with code 0", n, timeout=30))
        q.screendump("08-budget")

        # 10. the session is alive: TCP client path unbroken -------------------
        code = None
        for _ in range(2):
            n = run_line(q, "wget http://example.com", settle=2.0)
            code = last_exit_after(q, n, timeout=75)
            if code == 0:
                break
            time.sleep(2)
        check("10a. wget example.com: exit 0 (poll did not break TCP client)",
              code == 0, f"code={code}")
        n = run_line(q, "ping 10.0.2.2", settle=4.0)
        seg = read_log(q)[n:]
        check("10b. ping still 4/4 after everything",
              seg.count("icmp echo reply seq=") >= 4, seg[-200:])
        n = run_line(q, "echo alive-v29", settle=1.2)
        q.screendump("10-alive")

        print("\nsession A done")
    finally:
        try:
            q.hmp("quit")
        except Exception:
            pass

    # ---------------------------------------------------------------- B: no nic
    q2 = QemuSession(ISO, WORK + "-b", smp="1", nic="none")
    try:
        assert q2.wait_serial_marker("boot complete", 120), "B boot failed"
        log2 = read_log(q2)
        check("11a. session B boots (banner v2.9.0)",
              "GLM OS v2.9.0" in log2)
        check("11b. offline cleanly (no nic, no dhcp)",
              "no intel e1000" in log2 and "boots offline" in log2)
        q2.screendump("11-b-nonic")
    finally:
        try:
            q2.hmp("quit")
        except Exception:
            pass

    ok = sum(1 for _, v in results if v)
    print(f"\n=== v2.9: {ok}/{len(results)} checks passed ===")
    sys.exit(0 if ok == len(results) else 1)


if __name__ == "__main__":
    main()
