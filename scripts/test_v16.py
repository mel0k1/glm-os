#!/usr/bin/env python3
"""GLM OS v1.6 test session — ring-3 file syscalls (userland owns the disk).

Key idea: COUNTER.ELF is a userland program whose exit code IS its boot
count, stored in /COUNTER.DAT via int 0x80 #36-42. Two boots over the SAME
disk image prove RING-3 data persistence:

  boot 1:
    01 boot: ahci probe + disk mounted read-write (klog)
    02 run /BIN/COUNTER.ELF  -> exit 1 (file miss -> boot #1, write+flush)
    03 run /BIN/FILES.ELF    -> exit = C (root listing via SYS_FILE_LIST)
    04 dsave /BIN/HELLO.ELF MARKER.ELF (kernel shell)
    05 run /BIN/FILES.ELF    -> exit = C+1  (kernel write visible to ring 3!)
    06 dstat: 0 / 16 fds leaked
    07 dcat /COUNTER.DAT     -> 2 bytes (the "1\\n" the PROGRAM wrote)
    08 regressions: FORKTEST / UDP pair / THREADTEST / ping
    09 gui enter + esc
  boot 2 (same persist16.img):
    10 run /BIN/COUNTER.ELF  -> exit 2  (read "1" from disk -> boot #2) THE PROOF
    11 run /BIN/FILES.ELF    -> exit = C+1 (listing survived the reboot)
    12 ddel /MARKER.ELF + run FILES -> exit = C (kernel delete visible to ring 3)
    13 dcat /COUNTER.DAT     -> 2 bytes ("2\\n")
    14 version 1.6.0
"""
import os
import re
import shutil
import sys
import time

from PIL import Image, ImageChops

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession  # noqa: E402

ISO = "/home/z/glm-os/build/glm-os.iso"
SEED = "/home/z/glm-os/build/disk.img"
SHOTS = "/home/z/glm-os/shots-v16"
DISK = "/home/z/glm-os/build/persist16.img"

os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = (
    "/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:"
    + os.environ.get("LD_LIBRARY_PATH", "")
)
os.makedirs(SHOTS, exist_ok=True)


def read_log(q):
    return open(q.serial_log, errors="replace").read()


def log_len(q):
    try:
        return len(read_log(q))
    except FileNotFoundError:
        return 0


def wait_marker(q, marker, start, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if marker in read_log(q)[start:]:
            return True
        if q.proc.poll() is not None:
            print(f"    [dbg] qemu died; tail: {read_log(q)[-300:]!r}")
            return False
        time.sleep(0.2)
    print(f"    [dbg] marker '{marker}' not found; tail: {read_log(q)[-400:]!r}")
    return False


def wait_exit_code(q, start, code, timeout=30):
    return wait_marker(q, f"exited with code {code}", start, timeout)


def parse_exit_code(q, start, timeout=30):
    """Wait for the next 'exited with code N' and return N (int) or None."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        m = re.search(r"exited with code (-?\d+)", read_log(q)[start:])
        if m:
            return int(m.group(1))
        if q.proc.poll() is not None:
            print(f"    [dbg] qemu died; tail: {read_log(q)[-300:]!r}")
            return None
        time.sleep(0.2)
    print(f"    [dbg] no exit code; tail: {read_log(q)[-400:]!r}")
    return None


def diff_count(a, b):
    ia = Image.open(a).convert("RGB")
    ib = Image.open(b).convert("RGB")
    if ia.size != ib.size:
        return -1
    d = ImageChops.difference(ia, ib).convert("L")
    return sum(d.histogram()[10:])


passed = 0
failed = 0


def check(name, cond):
    global passed, failed
    if cond:
        passed += 1
        print(f"[ok] {name}")
    else:
        failed += 1
        print(f"[FAIL] {name}")


def boot_session(shots_prefix):
    return QemuSession(ISO, SHOTS, smp="4", nic="user,model=e1000", disk=DISK)


def main():
    global passed, failed
    rc = 0

    # make sure no leftover QEMU from a previous run still holds the disk
    deadline = time.time() + 10
    while time.time() < deadline:
        r = os.system("pgrep -f glm-os.iso >/dev/null 2>&1")
        if r != 0:
            break
        time.sleep(0.5)

    # fresh disk copy: seeded by build.sh (BIN/ + README.TXT)
    shutil.copyfile(SEED, DISK)

    # ================= BOOT 1 =================
    q = boot_session("b1")
    try:
        assert q.wait_serial_marker("boot complete", 90), "boot1 never completed"
        log = read_log(q)
        check("01 ahci disk probed", 'ahci: sata disk on port 0: "QEMUHARDDISK"' in log)
        check("01 disk mounted rw", re.search(
            r"fat32: disk mounted 64 MiB, \d+ files in root \(sata disk.*read-write\)", log) is not None)
        time.sleep(1.0)
        q.screendump("b1-01-boot")

        # ---- 02 COUNTER boot #1: miss -> create -> write -> flush ----
        n0 = log_len(q)
        q.type_text("run /BIN/COUNTER.ELF\n")
        code = parse_exit_code(q, n0, 40)
        check("02a COUNTER exit code = 1 (boot #1)", code == 1)
        check("02b file_open miss (no O_CREATE)",
              "file: open COUNTER.DAT: miss (no O_CREATE)" in read_log(q)[n0:])
        check("02c file_write 1 byte by ring 3",
              re.search(r"file: write fd=\d+ n=1 \(\d+ bytes\)", read_log(q)[n0:]) is not None)
        check("02d flush -> fat32 wrote COUNTER.DAT",
              "fat32: disk: wrote /COUNTER.DAT (2 bytes)" in read_log(q)[n0:])
        time.sleep(0.5)
        q.screendump("b1-02-counter1")

        # ---- 03 FILES: root listing via SYS_FILE_LIST ----
        n0 = log_len(q)
        q.type_text("run /BIN/FILES.ELF\n")
        c1 = parse_exit_code(q, n0, 40)
        check("03a FILES exit = entry count (>0)", c1 is not None and c1 > 0)
        check("03b file_list klog matches", f"file: list -> {c1} entries" in read_log(q)[n0:])
        time.sleep(0.4)
        q.screendump("b1-03-files1")

        # ---- 04 kernel shell writes the same disk ----
        n0 = log_len(q)
        q.type_text("dsave /BIN/HELLO.ELF MARKER.ELF\n")
        check("04 dsave MARKER.ELF", wait_marker(q, "fat32: disk: wrote /MARKER.ELF (", n0, 20))

        # ---- 05 ring 3 SEES the kernel's write ----
        n0 = log_len(q)
        q.type_text("run /BIN/FILES.ELF\n")
        c2 = parse_exit_code(q, n0, 40)
        check("05 FILES sees MARKER.ELF: count C+1", c2 is not None and c1 is not None and c2 == c1 + 1)
        time.sleep(0.4)
        q.screendump("b1-05-files2")

        # ---- 06 no fd leaks after all programs exited ----
        n0 = log_len(q)
        q.type_text("dstat\n")
        ok = wait_marker(q, "dstat: 0 / 16 open fd slots", n0, 15)
        check("06 dstat klogs fd snapshot", ok)
        m = re.search(r"dstat: (\d+) / 16 open fd slots", read_log(q)[n0:])
        check("06b zero fd leaks after exits", m is not None and m.group(1) == "0")

        # ---- 07 the program's file is byte-visible to the shell ----
        n0 = log_len(q)
        q.type_text("dcat /COUNTER.DAT\n")
        check("07 dcat COUNTER.DAT = 2 bytes (ring-3 wrote it)",
              wait_marker(q, "disk: cat /COUNTER.DAT (2 bytes)", n0, 20))

        # ---- 08 regressions ----
        n0 = log_len(q)
        q.type_text("run /BIN/FORKTEST.ELF\n")
        check("08a forktest child 42", wait_marker(q, "exited with code 42", n0, 25))
        check("08b forktest parent 0", wait_marker(q, "exited with code 0", n0, 25))
        n0 = log_len(q)
        q.type_text("spawn /BIN/UDPSERV.ELF\n")
        check("08c udpserv bound", wait_marker(q, "sock: bound id=", n0, 20))
        n0 = log_len(q)
        q.type_text("run /BIN/UDPCLI.ELF\n")
        check("08d udpcli roundtrip", wait_marker(q, "exited with code 0", n0, 25))
        n0 = log_len(q)
        q.type_text("spawn /BIN/THREADTEST.ELF\n")
        check("08e threadtest 101", wait_marker(q, "exited with code 101", n0, 25))
        check("08f threadtest 102", wait_marker(q, "exited with code 102", n0, 25))
        n0 = log_len(q)
        q.type_text("ping\n")
        deadline = time.time() + 30
        while read_log(q)[n0:].count("icmp echo reply seq=") < 4 and time.time() < deadline:
            time.sleep(0.3)
        check("08g ping gateway", read_log(q)[n0:].count("icmp echo reply seq=") >= 4)

        # ---- 09 gui round trip (files subsystem untouched) ----
        q.type_text("gui\n")
        check("09a gui enter", wait_marker(q, "gui: enter", n0, 20))
        time.sleep(1.5)
        q.screendump("b1-09-gui")
        q.hmp("sendkey esc")
        time.sleep(1.5)
        check("09b gui exit", wait_marker(q, "gui: exit", log_len(q) - 2000, 10))
    finally:
        q.quit()

    # ================= BOOT 2 (SAME DISK) =================
    q = boot_session("b2")
    try:
        assert q.wait_serial_marker("boot complete", 90), "boot2 never completed"
        check("10 boot2 disk mounted again",
              "fat32: disk mounted 64 MiB" in read_log(q))
        time.sleep(1.0)

        # ---- 11 THE PROOF: COUNTER reads its own boot-1 file ----
        n0 = log_len(q)
        q.type_text("run /BIN/COUNTER.ELF\n")
        code = parse_exit_code(q, n0, 40)
        check("11a COUNTER exit code = 2 (boot #2, ring-3 data PERSISTED)", code == 2)
        check("11b file_open hit",
              re.search(r"file: open fd=\d+ /COUNTER\.DAT \(2 bytes, hit\)", read_log(q)[n0:]) is not None)
        time.sleep(0.5)
        q.screendump("b2-11-counter2")

        # ---- 12 listing survived the reboot ----
        n0 = log_len(q)
        q.type_text("run /BIN/FILES.ELF\n")
        c3 = parse_exit_code(q, n0, 40)
        check("12 FILES boot2 = C+1 (MARKER.ELF still there)", c3 is not None and c2 is not None and c3 == c2)

        # ---- 13 kernel delete visible to ring 3 ----
        n0 = log_len(q)
        q.type_text("ddel /MARKER.ELF\n")
        check("13a ddel MARKER.ELF", wait_marker(q, "fat32: disk: deleted /MARKER.ELF (", n0, 20))
        n0 = log_len(q)
        q.type_text("run /BIN/FILES.ELF\n")
        c4 = parse_exit_code(q, n0, 40)
        check("13b FILES sees the delete: count C", c4 is not None and c1 is not None and c4 == c1)

        # ---- 14 counter file now holds "2" ----
        n0 = log_len(q)
        q.type_text("dcat /COUNTER.DAT\n")
        check("14 dcat COUNTER.DAT = 2 bytes", wait_marker(q, "disk: cat /COUNTER.DAT (2 bytes)", n0, 20))

        # ---- version ----
        check("15 version 1.6.0", "GLM OS v1.6.0" in read_log(q))
        q.screendump("b2-15-final")
    finally:
        q.quit()

    print(f"\nv1.6: {passed} passed, {failed} failed")
    if failed:
        rc = 1
    return rc


if __name__ == "__main__":
    sys.exit(main())
