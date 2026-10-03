#!/usr/bin/env python3
"""v2.0 DNS + HTTP — full automated verification.

The whole v2.0 milestone lives in ring 3: WGET.ELF resolves names over
the UDP socket syscalls (glm_user::dns), speaks plain HTTP/1.0 over the
v1.3 TCP syscalls (glm_user::http) and saves bodies through the v1.6
file syscalls. The kernel gained: DNS_IP net-info subop, a 512-byte
datagram budget, and the default route (next_hop via the gateway).

VERIFICATION CHANNEL: serial carries klogs only (task exit codes +
fat32 byte counts); program console output goes to the framebuffer and
is intentionally never parsed here. File contents are proven by the
fat32 klog byte counts: MARKER + newline = 25 bytes exactly.

Covered (host python server = 10.0.2.2:8020, slirp DNS = 10.0.2.3):
  1.  boot banner says v2.0.0
  2.  IP-literal fetch:  wget http://10.0.2.2:8020/GLM20.TXT PAGE.HTM
      -> exit 0, flush klog (25 bytes)
  3.  body readback:     dcat /PAGE.HTM -> klog (25 bytes)
  4.  REAL DNS:          wget http://example.com/ EXAMPLE.HTM -> exit 0
      (slirp forwards to the host resolver; requires host internet;
      needs the v2.0 default route - off-subnet via the gateway)
  5.  body readback:     dcat /EXAMPLE.HTM -> klog (N > 40 bytes)
  6.  redirect follow:   /redir/GLM20.TXT answers 301 -> /GLM20.TXT,
      wget hops, exits 0, REDIR.TXT is 25 bytes
  7.  negative DNS:      wget http://glm-nx-z7q.invalid/ -> exit 2 FAST
      (bounded: NXDOMAIN, no hang)
  8.  shell alive after all of it: run ARGS.ELF a b c -> exit 4
  9.  inline regressions: pipe (echo|upper, exit = byte count),
      mkdir klog
 10.  persistence:       reboot on the SAME disk -> PAGE.HTM is still
      there (25 bytes)
"""
import os, re, shutil, sys, tempfile, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/build/test20"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = os.path.join(WORK, "disk20.img")

os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = ("/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:" + os.environ.get("LD_LIBRARY_PATH", ""))

MARKER = "GLM-OS-V2-HTTP-OK-7f3d9a"
MARKER_LEN = len(MARKER) + 1  # the trailing newline the file carries
HOST_PORT = 8020

PASS, FAIL = 0, 0
def check(name, ok, detail=""):
    global PASS, FAIL
    mark = "PASS" if ok else "FAIL"
    print(f"[{mark}] {name}" + (f"  ({detail})" if detail else ""))
    if ok: PASS += 1
    else: FAIL += 1

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
FLUSH_LINE = re.compile(r"file: flush (\S+) \((\d+) bytes\)")
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

def last_flush_after(q, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = FLUSH_LINE.findall(read_log(q)[start:])
        if m:
            return m[-1]
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

# ---------------------------------------------------------------- server
DOCROOT = tempfile.mkdtemp(prefix="glm20-www-")
with open(os.path.join(DOCROOT, "GLM20.TXT"), "w") as f:
    f.write(MARKER + "\n")

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):  # keep the test console clean
        pass
    def do_GET(self):
        if self.path.startswith("/redir/"):
            self.send_response(301)
            self.send_header("Location", self.path[len("/redir"):])
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
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
print(f"host http server on 127.0.0.1:{HOST_PORT} (docroot {DOCROOT})")

shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK, exist_ok=True)
shutil.copyfile(DISK, DISK_COPY)

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
try:
    assert q.wait_serial_marker("boot complete", 120), "boot failed"
    print("boot ok")
    time.sleep(1)

    # 1. banner
    check("1. boot banner (current)", "GLM OS v2.3.0" in read_log(q))

    # 2. IP-literal fetch through slirp NAT
    n = run_line(q, f"wget http://10.0.2.2:{HOST_PORT}/GLM20.TXT PAGE.HTM", settle=4)
    code = last_exit_after(q, n)
    fl = last_flush_after(q, n)
    ok_flush = bool(fl) and fl[0] == "/PAGE.HTM" and int(fl[1]) == MARKER_LEN
    check("2. wget 10.0.2.2 (ip literal) exit 0", code == 0 and ok_flush,
          f"code={code} flush={fl}")

    # 3. body readback
    n = run_line(q, "dcat /PAGE.HTM")
    ct = last_cat_after(q, n)
    ok = bool(ct) and ct[0] == "/PAGE.HTM" and int(ct[1]) == MARKER_LEN
    check("3. dcat /PAGE.HTM is exactly the marker file", ok, f"cat={ct}")

    # 4. REAL DNS through slirp's forwarder + default route
    n = run_line(q, "wget http://example.com/ EXAMPLE.HTM", settle=6)
    code = last_exit_after(q, n, 75)
    check("4. wget example.com (real dns) exit 0", code == 0, f"code={code}")

    # 5. body readback of the real page (example.com is ~500 bytes)
    n = run_line(q, "dcat /EXAMPLE.HTM")
    ct = last_cat_after(q, n)
    ok = bool(ct) and ct[0] == "/EXAMPLE.HTM" and int(ct[1]) > 40
    check("5. dcat /EXAMPLE.HTM has the real page", ok, f"cat={ct}")

    # 6. redirect following (301 -> /GLM20.TXT)
    n = run_line(q, f"wget http://10.0.2.2:{HOST_PORT}/redir/GLM20.TXT REDIR.TXT", settle=4)
    code = last_exit_after(q, n)
    n2 = run_line(q, "dcat /REDIR.TXT")
    ct = last_cat_after(q, n2)
    ok = code == 0 and bool(ct) and ct[0] == "/REDIR.TXT" and int(ct[1]) == MARKER_LEN
    check("6. 301 followed, body lands intact", ok, f"code={code} cat={ct}")

    # 7. negative DNS: bounded failure, no hang
    t0 = time.time()
    n = run_line(q, "wget http://glm-nx-z7q.invalid/", settle=0)
    code = last_exit_after(q, n, 45)
    dt = time.time() - t0
    check("7. nxdomain -> exit 2 (bounded, <25s)", code == 2 and dt < 25,
          f"code={code} dt={dt:.1f}s")

    # 8. the shell is alive and argv still flows after everything
    n = run_line(q, "run ARGS.ELF a b c")
    code = last_exit_after(q, n)
    check("8. shell alive, argc pipeline intact (exit 4)", code == 4, f"code={code}")

    # 9. inline regressions: v1.8 pipes + v1.9 dirs
    n = run_line(q, "run ECHO.ELF hi | run UPPER.ELF", settle=3)
    code = last_exit_after(q, n)
    ok = code in (2, 3)  # upper exits with the byte count it forwarded
    check("9a. pipe regression (echo|upper)", ok, f"code={code}")
    n = run_line(q, "mkdir /T20")
    ok = wait_for(q, "fat32: disk: mkdir /T20 ok", n, 15)
    check("9b. mkdir regression", ok)

    # 10. reboot: persistence of the fetched files
    n = run_line(q, "reboot", settle=1)
    q.proc.wait(timeout=30)
finally:
    try: q.quit()
    except Exception: pass

q2 = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
try:
    assert q2.wait_serial_marker("boot complete", 120), "second boot failed"
    time.sleep(1)
    n = run_line(q2, "dcat /PAGE.HTM")
    ct = last_cat_after(q2, n)
    ok = bool(ct) and ct[0] == "/PAGE.HTM" and int(ct[1]) == MARKER_LEN
    check("10. PAGE.HTM survives the reboot (persistent fetch)", ok, f"cat={ct}")
    n = run_line(q2, "dcat /EXAMPLE.HTM")
    ct = last_cat_after(q2, n)
    ok = bool(ct) and ct[0] == "/EXAMPLE.HTM" and int(ct[1]) > 40
    check("10b. EXAMPLE.HTM survives too", ok, f"cat={ct}")
finally:
    try: q2.quit()
    except Exception: pass

srv.shutdown()
print(f"\n==== v2.0 summary: {PASS} pass, {FAIL} fail ====")
sys.exit(1 if FAIL else 0)
