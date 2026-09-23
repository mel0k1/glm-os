#!/usr/bin/env python3
"""GLM OS v1.7 test session — exec: the Unix process model closes in ring 3.

SYS_EXEC (43) lets a ring-3 process BECOME another program. Composed with
fork (v0.6) and wait (v0.3) this is the classic launcher pattern, exercised
by RUNIT.ELF: fork, child execs, parent waits and propagates the exit code.

  boot 1:
    01 boot: ahci disk + rw mount + version 1.7.0
    02 run ARGS.ELF            -> exit 1  (argv[0] only, from ramdisk)
    03 run ARGS.ELF one two three -> exit 4  (kernel shell passes argv)
    04 klog argc=4 in the spawn record
    05 drun ARGS.ELF a b c d   -> exit 5  (same argv path, image from DISK)
    06 drun RUNIT.ELF ARGS.ELF x y -> exit 3  (fork+exec+wait, exec from disk)
       + klog exec: pid N became ARGS.ELF (3 args, from disk, ...)
       + klog exec: old image destroyed (N frames reclaimed)
    07 drun RUNIT.ELF NOPE.ELF -> exit 127 (exec failure, shell convention)
    08 /BIN/HELLO.ELF removed from the disk image host-side; the ramdisk
       still has it -> drun RUNIT.ELF HELLO.ELF hi -> exit 0 with
       klog "from ramdisk" (exec ramdisk fallback after disk miss)
    09 dsave /BIN/RUNIT.ELF LAUNCH.ELF (the launcher itself persists)
  boot 2 (same disk):
    10 drun LAUNCH.ELF ARGS.ELF p -> exit 2 (exec works after reboot)
    11 COUNTER boot count round-trips (data persistence untouched)
    12 regressions: FORKTEST (42 then 0), THREADTEST (101 then 102),
       UDP pair (0), ping gateway (4 replies), dstat 0 leaked fds,
       ps, gui enter/esc
    13 version 1.7.0
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
SHOTS = "/home/z/glm-os/shots-v17"
DISK = "/home/z/glm-os/build/persist17.img"

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


def parse_exit_code(q, start, timeout=40):
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


def parse_last_exit_code(q, start, timeout=40):
    """Wait until no NEW exit line appears for a while; return the LAST N.
    Only GROWING match counts extend the deadline (old matches linger in
    the log slice forever, so an unconditional extend would never end)."""
    deadline = time.time() + timeout
    last = None
    seen = 0
    while time.time() < deadline:
        ms = re.findall(r"exited with code (-?\d+)", read_log(q)[start:])
        if len(ms) > seen:
            seen = len(ms)
            last = int(ms[-1])
            deadline = time.time() + 4
        if q.proc.poll() is not None:
            break
        time.sleep(0.3)
    return last


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
    phase = sys.argv[1] if len(sys.argv) > 1 else "all"

    # make sure no leftover QEMU from a previous run still holds the disk
    deadline = time.time() + 10
    while time.time() < deadline:
        r = os.system("pgrep -f glm-os.iso >/dev/null 2>&1")
        if r != 0:
            break
        time.sleep(0.5)

    # disk prep happens ONLY in phase b1 ("all" runs b1 first): b2 must
    # boot from the disk state b1 left behind, that is the whole point
    if phase in ("all", "b1"):
        # fresh disk copy: seeded by build.sh (BIN/ + README.TXT).
        # Then remove /BIN/HELLO.ELF from the DISK image only (host-side
        # mdel): the ramdisk still has it, so the exec ramdisk-fallback
        # path is the only way to run HELLO.ELF in the RUNIT chain below.
        shutil.copyfile(SEED, DISK)
        os.system("MTOOLS_SKIP_CHECK=1 mdel -i %s ::/BIN/HELLO.ELF" % DISK)

    # ================= BOOT 1 =================
    if phase in ("all", "b1"):
        rc = boot1() or rc
    if phase in ("all", "b2"):
        rc = boot2() or rc
    print(f"\n==== v1.7 [{phase}]: {passed} passed, {failed} failed ====")
    return rc


def boot1():
    global passed, failed
    rc = 0
    q = boot_session("b1")
    try:
        assert q.wait_serial_marker("boot complete", 90), "boot1 never completed"
        log = read_log(q)
        check("01a ahci disk probed", 'ahci: sata disk on port 0: "QEMUHARDDISK"' in log)
        check("01b disk mounted rw", re.search(
            r"fat32: disk mounted 64 MiB, \d+ files in root \(sata disk.*read-write\)", log) is not None)
        check("01c version 1.7.0", "GLM OS v1.7.0" in log)
        time.sleep(1.0)
        q.screendump("b1-01-boot")

        # ---- 02 argv[0]-only spawn from the ramdisk ----
        n0 = log_len(q)
        q.type_text("run ARGS.ELF\n")
        code = parse_exit_code(q, n0)
        check("02 run ARGS.ELF exit 1 (argc=1)", code == 1)
        n0 = log_len(q)

        # ---- 03 kernel shell passes argv ----
        q.type_text("run ARGS.ELF one two three\n")
        code = parse_exit_code(q, n0)
        check("03 run ARGS.ELF one two three exit 4 (argc=4)", code == 4)
        log = read_log(q)[n0:]
        check("03b spawn record carries argc=4",
              re.search(r"user: pid \d+ ready:.*argc=4", log) is not None)
        n0 = log_len(q)

        # ---- 05 same argv path with the image on the DISK ----
        q.type_text("drun ARGS.ELF a b c d\n")
        code = parse_exit_code(q, n0)
        check("05 drun ARGS.ELF a b c d exit 5 (argc=5)", code == 5)
        n0 = log_len(q)

        # ---- 06 THE PROOF: fork+exec+wait entirely in ring 3 ----
        q.type_text("drun RUNIT.ELF ARGS.ELF x y\n")
        code = parse_exit_code(q, n0)
        check("06a RUNIT chain exit 3 (== child argc)", code == 3)
        log = read_log(q)[n0:]
        check("06b exec klog: became ARGS.ELF (3 args, from disk)",
              re.search(r"exec: pid \d+ became ARGS.ELF \(3 args, from disk, \d+ bytes\)", log) is not None)
        check("06c exec klog: old image destroyed", "exec: old image destroyed" in log)
        n0 = log_len(q)

        # ---- 07 exec failure -> 127 (command not found convention) ----
        q.type_text("drun RUNIT.ELF NOPE.ELF\n")
        # target 127 specifically: the 06 chain's parent-exit line may land
        # inside this window a moment late
        check("07 RUNIT NOPE.ELF exit 127 (exec failed)",
              wait_marker(q, "exited with code 127", n0, 40))
        check("07b klog: no such file on disk or ramdisk",
              "exec: pid" in read_log(q)[n0:] and "no such file on disk or ramdisk" in read_log(q)[n0:])
        n0 = log_len(q)

        # ---- 08 ramdisk fallback: /BIN/HELLO.ELF was removed from the
        # disk image host-side before boot, so the exec must miss on the
        # disk and hit the ramdisk ----
        q.type_text("drun RUNIT.ELF HELLO.ELF hi\n")
        code = parse_exit_code(q, n0)
        check("08a exec ramdisk fallback: RUNIT exit 0 (HELLO exits 0)", code == 0)
        check("08b klog: became HELLO.ELF (2 args, from ramdisk)",
              re.search(r"exec: pid \d+ became HELLO.ELF \(2 args, from ramdisk", read_log(q)[n0:]) is not None)
        n0 = log_len(q)

        # ---- 09 the launcher itself persists to the disk root ----
        q.type_text("dsave /BIN/RUNIT.ELF LAUNCH.ELF\n")
        check("09 dsave wrote LAUNCH.ELF",
              wait_marker(q, "fat32: disk: wrote /LAUNCH.ELF", n0, 20))
        n0 = log_len(q)

        # ---- quick CoW regression before reboot (the refcount fix!) ----
        q.type_text("run FORKTEST.ELF\n")
        time.sleep(2.5)
        codes = re.findall(r"exited with code (-?\d+)", read_log(q)[n0:])
        check("10a FORKTEST codes 42 then 0", codes[:2] == ["42", "0"])
        n0 = log_len(q)

        # ---- gui round trip ----
        q.type_text("gui\n")
        check("11a gui enter", wait_marker(q, "gui: enter", n0, 20))
        time.sleep(1.5)
        q.screendump("b1-11-gui")
        q.hmp("sendkey esc")
        check("11b gui exit (esc)", wait_marker(q, "gui: exit", log_len(q) - 2000, 10))
        time.sleep(0.5)

        # leave the disk state for phase b2 (no reboot here; the phase
        # runner exits QEMU via the finally below)
    finally:
        try:
            q.hmp("quit")
        except Exception:
            pass
        time.sleep(0.5)
        if q.proc.poll() is None:
            q.proc.terminate()
            try:
                q.proc.wait(timeout=5)
            except Exception:
                q.proc.kill()
    return rc


def boot2():
    global passed, failed
    rc = 0
    q = boot_session("b2")
    try:
        assert q.wait_serial_marker("boot complete", 90), "boot2 never completed"
        time.sleep(3.0)
        n0 = log_len(q)

        # ---- 12 exec works for a launcher that was SAVED last boot ----
        q.type_text("drun LAUNCH.ELF ARGS.ELF p\n")
        code = parse_exit_code(q, n0)
        check("12 drun LAUNCH.ELF (saved in boot1) exit 2 (== argc)", code == 2)
        n0 = log_len(q)

        # ---- 13 COUNTER: ring-3 data persistence untouched ----
        q.type_text("run /BIN/COUNTER.ELF\n")
        code = parse_exit_code(q, n0)
        check("13 COUNTER boot count round-trips (exit 1 or 2)", code in (1, 2))
        n0 = log_len(q)

        # ---- 14 regressions ----
        q.type_text("run FORKTEST.ELF\n")
        time.sleep(2.5)
        codes = re.findall(r"exited with code (-?\d+)", read_log(q)[n0:])
        check("14a FORKTEST codes 42 then 0", codes[:2] == ["42", "0"])
        n0 = log_len(q)

        time.sleep(2.0)  # drain the previous command's late exit lines
        n0 = log_len(q)
        q.type_text("run THREADTEST.ELF\n")
        time.sleep(5)
        tcodes = re.findall(r"exited with code (-?\d+)", read_log(q)[n0:])
        check("14b THREADTEST threads 101,102 then process 0",
              "101" in tcodes and "102" in tcodes and tcodes[-1] == "0")
        n0 = log_len(q)

        q.type_text("spawn UDPSERV.ELF\n")
        time.sleep(1.5)
        q.type_text("run UDPCLI.ELF hello-v17\n")
        code = parse_exit_code(q, n0)
        check("14c UDP pair exit 0", code == 0)
        n0 = log_len(q)

        q.type_text("ping\n")
        time.sleep(6)
        check("14d ping gateway 4 replies",
              read_log(q)[n0:].count("icmp echo reply seq=") >= 4)
        n0 = log_len(q)

        q.type_text("dstat\n")
        time.sleep(1.5)
        check("14e dstat: no leaked fds (0 / 16 open)",
              re.search(r"dstat: 0 / 16 open fd slots", read_log(q)[n0:]) is not None)
        n0 = log_len(q)

        q.type_text("gui\n")
        check("14f gui enter", wait_marker(q, "gui: enter", n0, 20))
        time.sleep(1.2)
        q.screendump("b2-14-gui")
        q.hmp("sendkey esc")
        check("14g gui exit (esc)", wait_marker(q, "gui: exit", log_len(q) - 2000, 10))
        n0 = log_len(q)

        # ---- 15 version ----
        q.type_text("about\n")
        time.sleep(1.0)
        check("15 about reports GLM OS v1.7.0", "GLM OS v1.7.0" in read_log(q)[n0:] or "GLM OS v1.7.0" in read_log(q))
    finally:
        try:
            q.hmp("quit")
        except Exception:
            pass
        time.sleep(0.5)
        if q.proc.poll() is None:
            q.proc.terminate()
            try:
                q.proc.wait(timeout=5)
            except Exception:
                q.proc.kill()

    return rc


if __name__ == "__main__":
    sys.exit(main())
