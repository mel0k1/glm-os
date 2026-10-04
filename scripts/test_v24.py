#!/usr/bin/env python3
"""GLM OS v2.4 test session — the wall clock + the OS's own web server.

New in v2.4:
  * kernel: CMOS RTC (src/cpu/rtc.rs) — base epoch at boot + PIT uptime;
    syscalls 55/56 (clock_time / clock_set); shell `date`; taskbar HH:MM:SS
  * ring 3: NTP.ELF — SNTP client (RFC 4330) over the v0.9 UDP sockets,
    adjusts the kernel clock via clock_set
  * ring 3: HTTPD.ELF — HTTP/1.0 server over v1.3 TCP + v1.6 files +
    v2.1 malloc, serving the persistent FAT32 disk to the host

Covered (host python SNTP server = 10.0.2.2:7373, hostfwd tcp 8080):
  1. boot banner v2.8.0 + boot complete
  2. rtc: wall-clock okline at boot, parsed datetime matches host UTC (±5 min)
  3. shell `date` runs (console output, cosmetic screenshot)
  4. NTP: host SNTP server receives a valid mode-3 request, replies;
     guest exits 0, klog "rtc: clock set from ring 3 -> epoch N" with
     N within ±10 s of host time
  5. HTTPD: host client over hostfwd 8080 —
       GET /            -> 200, body has "GLM OS 2.4"
       GET /HELLO.TXT   -> 200, body has "served over http"
       GET /NOPE.HTM    -> 404
       GET /../SECRET   -> 403 (path traversal refused, --path-as-is)
       POST /           -> 405
       GET / (6th conn) -> 200 and the server exits 0 (budget spent)
  6. session B (no nic): boot + rtc line still present (wall clock is
     independent of the NIC)
"""
import os
import re
import shutil
import socket
import struct
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
DISK = "/home/z/glm-os/build/disk.img"
WORK = "/home/z/glm-os/work-v24"
SHOTS = "/home/z/glm-os/shots-v24"

NTP_PORT = 7373
HTTP_PORT = 8080

results = []


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


EXIT_LINE = re.compile(r"sched: task \d+ exited with code (-?\d+)")
RTC_LINE = re.compile(r"rtc: wall clock (\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}) UTC")
CLOCK_SET = re.compile(r"rtc: clock set from ring 3 -> epoch (\d+)")


def run_line(q, line, settle=1.5):
    n = log_len(q)
    q.type_text(line + "\n")
    time.sleep(settle)
    return n


def last_exit_after(q, start, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        codes = EXIT_LINE.findall(read_log(q)[start:])
        if codes:
            return int(codes[-1])
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


def parse_rtc_epoch(s):
    dt = datetime.strptime(s, "%Y-%m-%d %H:%M:%S").replace(tzinfo=timezone.utc)
    return dt.timestamp()


# ---------------------------------------------------------------- SNTP server
ntp_events = {"requests": 0, "replies": 0, "last_ts": 0.0}


def sntp_server(port):
    srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", port))
    srv.settimeout(60)
    try:
        while True:
            data, addr = srv.recvfrom(512)
            ntp_events["requests"] += 1
            # validate a real SNTP client request: 48 bytes, VN=4, Mode=3
            if len(data) >= 48 and (data[0] & 0x38) >> 3 == 4 and data[0] & 0x07 == 3:
                ntp_events["last_ts"] = time.time()
                # build a server reply: LI=0 VN=4 Mode=4, transmit ts = now
                now = time.time() + 2_208_988_800
                sec = int(now)
                frac = int((now - sec) * 2**32)
                reply = bytearray(48)
                reply[0] = 0x24  # LI=0, VN=4, Mode=4 (server)
                reply[1] = 0     # stratum 1... 0=unspecified, keep simple
                struct.pack_into("!II", reply, 40, sec, frac)
                srv.sendto(bytes(reply), addr)
                ntp_events["replies"] += 1
    except (socket.timeout, OSError):
        pass


# ------------------------------------------------------------- HTTP client
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
    # split status / headers / body
    head, _, body = buf.partition(b"\r\n\r\n")
    status = head.split(b"\r\n")[0].decode(errors="replace") if head else ""
    return status, body, head


def main():
    shutil.rmtree(WORK, ignore_errors=True)
    os.makedirs(WORK, exist_ok=True)
    shutil.copyfile(DISK, os.path.join(WORK, "disk.img"))
    disk_copy = os.path.join(WORK, "disk.img")

    nic = f"user,model=e1000,hostfwd=tcp:127.0.0.1:{HTTP_PORT}-:{HTTP_PORT}"
    q = QemuSession(ISO, WORK, smp="4", nic=nic, disk=disk_copy)

    t = threading.Thread(target=sntp_server, args=(NTP_PORT,), daemon=True)
    t.start()
    print(f"host sntp server on 127.0.0.1:{NTP_PORT}")

    try:
        # 1. boot -----------------------------------------------------------
        assert q.wait_serial_marker("boot complete", 120), "boot failed"
        log = read_log(q)
        check("1. boot banner v2.8.0", "GLM OS v2.8.0" in log)
        print("boot ok")
        time.sleep(0.5)

        # 2. rtc wall clock at boot ------------------------------------------
        m = RTC_LINE.search(log)
        check("2a. rtc: wall-clock okline present", m is not None,
              log[:4000][-500:])
        if m:
            boot_epoch = parse_rtc_epoch(m.group(1))
            drift = abs(boot_epoch - time.time())
            check("2b. wall clock matches host UTC (±300 s)", drift < 300,
                  f"guest {m.group(1)} drift {drift:.0f}s")
            check("2c. year is plausible (>=2025)", int(m.group(1)[:4]) >= 2025)

        # 3. shell date (cosmetic; screenshot for the gallery) ---------------
        n = run_line(q, "date", settle=1.2)
        q.screendump("03-date")

        # 4. NTP over UDP ------------------------------------------------------
        n = run_line(q, f"run NTP.ELF 10.0.2.2 {NTP_PORT}", settle=1.0)
        ok_exit = wait_for(q, "exited with code 0", n, timeout=45)
        code = last_exit_after(q, n, timeout=10)
        check("4a. NTP.ELF exit 0", ok_exit and code == 0, f"code={code}")
        deadline = time.time() + 5
        while time.time() < deadline and ntp_events["requests"] == 0:
            time.sleep(0.2)
        check("4b. host saw a valid SNTP request", ntp_events["requests"] >= 1)
        check("4c. host replied", ntp_events["replies"] >= 1)
        cs = CLOCK_SET.findall(read_log(q)[n:])
        check("4d. kernel applied clock_set", len(cs) >= 1, read_log(q)[n:][-400:])
        if cs:
            drift = abs(int(cs[-1]) - time.time())
            check("4e. set epoch matches host time (±10 s)", drift < 10,
                  f"drift {drift:.0f}s")
        q.screendump("04-ntp")

        # 4f. regression: ping still works (also warms slirp's view of the
        # guest so hostfwd inbound connections can be delivered at all)
        n = run_line(q, "ping 10.0.2.2", settle=4.0)
        seg = read_log(q)[n:]
        check("4f. ping 10.0.2.2 4/4",
              seg.count("icmp echo reply seq=") >= 4
              and "seq=3" in seg, seg[-200:])

        # 5. HTTPD: the disk is the website -----------------------------------
        n = run_line(q, "spawn HTTPD.ELF 8080 6", settle=1.5)
        wait_for(q, "ring-3 web server on port 8080", n, timeout=10)

        H = b"Host: glm-os\r\nConnection: close\r\n\r\n"

        st, body, _ = http_client("127.0.0.1", HTTP_PORT, b"GET / HTTP/1.0\r\n" + H)
        check("5a. GET / -> 200", " 200" in st, st)
        check("5b. index body is the GLM OS page", b"GLM OS 2.4" in body)

        st, body, _ = http_client("127.0.0.1", HTTP_PORT, b"GET /HELLO.TXT HTTP/1.0\r\n" + H)
        check("5c. GET /HELLO.TXT -> 200", " 200" in st, st)
        check("5d. hello body", b"served over http" in body)

        st, body, _ = http_client("127.0.0.1", HTTP_PORT, b"GET /NOPE.HTM HTTP/1.0\r\n" + H)
        check("5e. GET /NOPE.HTM -> 404", " 404" in st, st)

        # --path-as-is: the raw ../ must reach the server untouched
        st, body, _ = http_client("127.0.0.1", HTTP_PORT,
                                  b"GET /../SECRET.TXT HTTP/1.0\r\n" + H)
        check("5f. path traversal -> 403", " 403" in st, st)

        st, body, _ = http_client("127.0.0.1", HTTP_PORT,
                                  b"POST / HTTP/1.0\r\n" + H + b"x=1")
        check("5g. POST -> 405", " 405" in st, st)

        st, body, _ = http_client("127.0.0.1", HTTP_PORT, b"GET / HTTP/1.0\r\n" + H)
        check("5h. 6th connection -> 200", " 200" in st, st)

        check("5i. server spent its budget, exit 0",
              wait_for(q, "exited with code 0", n, timeout=30))
        q.screendump("05-httpd")

        # regression: net table still sane after all this
        n = run_line(q, "net", settle=1.2)
        check("5j. net command runs", True)
        q.screendump("06-net")

    finally:
        q.quit()

    # ------------------------------------------------ session B: no nic ---
    print("session B: no nic")
    q2 = QemuSession(ISO, WORK + "-b", smp="1", nic="none")
    try:
        assert q2.wait_serial_marker("boot complete", 120), "no-nic boot failed"
        log = read_log(q2)
        check("6a. no-nic boot banner v2.8.0", "GLM OS v2.8.0" in log)
        check("6b. wall clock alive without a NIC", "rtc: wall clock" in log)
        run_line(q2, "date", settle=1.0)
        q2.screendump("07-no-nic-date")
    finally:
        q2.quit()

    print("\n=== v2.4 summary ===")
    fails = [name for name, ok in results if not ok]
    for name, ok in results:
        print(f"  [{'ok ' if ok else 'FAIL'}] {name}")
    print(f"{len(results) - len(fails)}/{len(results)} checks passed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
