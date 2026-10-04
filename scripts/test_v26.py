#!/usr/bin/env python3
"""GLM OS v2.6 test session — DHCP: the address is LEARNED.

New in v2.6:
  * kernel: the IPv4 config (ip/mask/gw/dns) moved from boot-time consts to
    runtime atomics; SYS_NET_SETCONF (57) applies a config from ring 3;
    SYS_NET_INFO gained subops 3 (mask) and 4 (MAC, 48-bit packed)
  * kernel: e1000 RCTL.BAM — broadcast frames were rejected IN HARDWARE
    since v0.8 (nothing ever sent us broadcasts before DHCP)
  * kernel: 255.255.255.255 TX fast path (no ARP) + broadcast RX acceptance
  * ring 3: DHCP.ELF — RFC 2131 client: setconf(0.0.0.0) -> DISCOVER ->
    OFFER -> REQUEST -> ACK -> setconf(lease); shell keyword `dhcp`
  * slirp's built-in DHCP server (10.0.2.2:67) is the counterparty

Covered:
  A. session with the NIC:
     boot banner 2.7.0 + the static default okline (pre-DHCP state),
     `dhcp`: klog setconf(0.0.0.0) -> recv from 10.0.2.2:67 ->
     setconf(ip=10.0.2.15 mask=255.255.255.0 gw=10.0.2.2 dns=10.0.2.3),
     exit 0; ping 10.0.2.2 (4/4 echo replies on the dynamic config);
     a SECOND `dhcp` re-leases cleanly; wget example.com (real DNS via the
     DHCP-learned resolver) + dcat readback; host-server fetch by IP
     literal; pipe/argv regressions
  B. session WITHOUT a NIC: boot stays alive, `dhcp` fails bounded (exit 1)
"""
import os
import re
import shutil
import socket
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
DISK = "/home/z/glm-os/build/disk.img"
WORK = "/home/z/glm-os/work-v26"
SHOTS = "/home/z/glm-os/shots-v26"
HOST_PORT = 8053
MARKER = "GLM26-DHCP-LEASED"

results = []


def check(name, ok, extra=""):
    results.append((name, bool(ok)))
    print(f"  [{'ok ' if ok else 'FAIL'}] {name}" + (f" | {extra}" if extra and not ok else ""))


def read_log(q):
    return open(q.serial_log, errors="replace").read()


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
CAT_LINE = re.compile(r"disk: cat (\S+) \((\d+) bytes\)")


def run_line(q, line, settle=1.5):
    n = len(read_log(q))
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


def last_cat_after(q, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = CAT_LINE.findall(read_log(q)[start:])
        if m:
            return m[-1]
        if q.proc.poll() is not None:
            break
        time.sleep(0.2)
    return None


# ---------------------------------------------------------------- host server
DOCROOT = tempfile.mkdtemp(prefix="glm26-www-")
with open(os.path.join(DOCROOT, "GLM26.TXT"), "w") as f:
    f.write(MARKER + "\n")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        try:
            body = open(os.path.join(DOCROOT, self.path.lstrip("/")), "rb").read()
        except OSError:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


srv = ThreadingHTTPServer(("127.0.0.1", HOST_PORT), Handler)
threading.Thread(target=srv.serve_forever, daemon=True).start()
print(f"host http server on 127.0.0.1:{HOST_PORT}")

shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK, exist_ok=True)
shutil.rmtree(SHOTS, ignore_errors=True)
os.makedirs(SHOTS, exist_ok=True)
DISK_COPY = os.path.join(WORK, "disk.img")
shutil.copyfile(DISK, DISK_COPY)

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
try:
    assert q.wait_serial_marker("boot complete", 120), "boot failed"
    print("boot ok")
    time.sleep(1)

    # 1. banner
    check("1. boot banner 2.7.0", "GLM OS v2.7.0" in read_log(q))

    # 2. the pre-DHCP state is the classic slirp lease (defaults)
    boot = read_log(q)
    ok = ("net: mac " in boot) and ("ip 10.0.2.15/24 via 10.0.2.2" in boot)
    check("2. boot okline shows the static default config", ok)

    # 3..6. the DHCP dance
    n = run_line(q, "dhcp", settle=6)
    check("3. dhcp: dropped to 0.0.0.0 (setconf from ring 3)",
          "net: config set from ring 3: ip=0.0.0.0 mask=0.0.0.0 gw=0.0.0.0 dns=0.0.0.0" in read_log(q)[n:])
    ok_rx = wait_for(q, "sock: recv id=0 port=68 udp from 10.0.2.2:67", n, 30)
    check("4. dhcp: reply received from 10.0.2.2:67 (slirp)", ok_rx)
    lease = ("net: config set from ring 3: ip=10.0.2.15 mask=255.255.255.0 "
             "gw=10.0.2.2 dns=10.0.2.3")
    check("5. dhcp: lease applied (ip/mask/gw/dns all from the server)",
          wait_for(q, lease, n, 20))
    code = last_exit_after(q, n, 30)
    check("6. dhcp: exit 0", code == 0, f"code={code}")
    q.screendump(os.path.join(SHOTS, "26-a-after-dhcp"))

    # 7. the dynamic config actually routes: 4/4 echo replies from the gw
    n = run_line(q, "ping 10.0.2.2", settle=14)
    seg = read_log(q)[n:]
    replies = len(re.findall(r"netd: icmp echo reply seq=\d", seg))
    check("7. ping 10.0.2.2 on the leased config: 4/4 replies", replies == 4,
          f"replies={replies}")

    # 8. the SECOND dhcp re-leases (port 68 free again, dance repeats)
    n = run_line(q, "dhcp", settle=6)
    ok2 = wait_for(q, lease, n, 25)
    code = last_exit_after(q, n, 30)
    check("8. second dhcp re-leases cleanly (exit 0)", ok2 and code == 0,
          f"code={code}")

    # 9. real DNS through the DHCP-learned resolver (10.0.2.3) + default route.
    # Two attempts: example.com sits behind a CDN that occasionally RSTs a
    # fresh HTTP/1.0 connection — that is a network flake, not an OS bug
    # (the OS claim is "DNS+TCP work over the learned config").
    code = None
    for _ in range(2):
        n = run_line(q, "wget http://example.com/ EXAMPLE.HTM", settle=6)
        code = last_exit_after(q, n, 75)
        if code == 0:
            break
        time.sleep(2)
    check("9. wget example.com over the learned dns/gw: exit 0", code == 0,
          f"code={code}")
    n = run_line(q, "dcat /EXAMPLE.HTM")
    ct = last_cat_after(q, n)
    ok = bool(ct) and ct[0] == "/EXAMPLE.HTM" and int(ct[1]) > 40
    check("10. dcat /EXAMPLE.HTM has the real page", ok, f"cat={ct}")

    # 11. host-server fetch by IP literal on the dynamic config
    n = run_line(q, f"wget http://10.0.2.2:{HOST_PORT}/GLM26.TXT PAGE.HTM", settle=4)
    code = last_exit_after(q, n, 40)
    n = run_line(q, "dcat /PAGE.HTM")
    ct = last_cat_after(q, n)
    ok = code == 0 and bool(ct) and ct[0] == "/PAGE.HTM" and ct[1] == str(len(MARKER) + 1)
    check("11. wget by ip literal + body intact", ok, f"code={code} cat={ct}")

    # 12. the learned config visible in `net` (console; screenshot only)
    run_line(q, "net", settle=2)
    q.screendump(os.path.join(SHOTS, "26-a-net-after-dhcp"))

    # 13. inline regressions
    n = run_line(q, "run ECHO.ELF hi | run UPPER.ELF", settle=3)
    code = last_exit_after(q, n)
    check("13a. pipe regression (echo|upper)", code in (2, 3), f"code={code}")
    n = run_line(q, "run ARGS.ELF a b c")
    code = last_exit_after(q, n)
    check("13b. argv regression (exit 4)", code == 4, f"code={code}")

    # power-off for a clean qemu exit
    run_line(q, "halt", settle=2)
except Exception as e:
    print("FATAL session A:", e)
finally:
    try:
        q.hmp("quit")
    except Exception:
        pass
    time.sleep(1)
    if q.proc.poll() is None:
        q.proc.kill()

# ------------------------------------------------------------- session B
WORK_B = WORK + "-b"
os.makedirs(WORK_B, exist_ok=True)
q = QemuSession(ISO, WORK_B, smp="1", nic="none")
try:
    assert q.wait_serial_marker("boot complete", 120), "boot failed (b)"
    print("boot B ok (no nic)")
    log = read_log(q)
    check("14. no-nic boot banner 2.7.0", "GLM OS v2.7.0" in log)
    check("15. no-nic boot: networking offline is honest",
          "net: no intel e1000 on pci bus 0 - networking offline" in log)

    # 16. dhcp without a NIC fails BOUNDED (sendto -> exit 1), no hang
    t0 = time.time()
    n = run_line(q, "dhcp", settle=2)
    code = last_exit_after(q, n, 30)
    dt = time.time() - t0
    check("16. dhcp without nic: exit 1, bounded (<20s)", code == 1 and dt < 20,
          f"code={code} dt={dt:.1f}s")

    n = run_line(q, "run ECHO.ELF ok | run UPPER.ELF", settle=3)
    code = last_exit_after(q, n)
    check("17. no-nic session still runs pipelines", code in (2, 3), f"code={code}")
    run_line(q, "halt", settle=2)
except Exception as e:
    print("FATAL session B:", e)
finally:
    try:
        q.hmp("quit")
    except Exception:
        pass
    time.sleep(1)
    if q.proc.poll() is None:
        q.proc.kill()

# ------------------------------------------------------------- summary
passed = sum(1 for _, ok in results if ok)
print(f"\n== v2.6: {passed}/{len(results)} checks passed ==")
sys.exit(0 if passed == len(results) else 1)
