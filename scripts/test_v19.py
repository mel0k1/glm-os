#!/usr/bin/env python3
"""v1.9 hierarchical filesystem — full automated verification.

Machine-readable channel: fat32/sysfile klogs ("fat32: disk: mkdir ... ok",
"file: chdir pid N -> PATH ok", "file: flush ...") plus task exit codes.
Everything below verifies the directory dimension end-to-end without
needing framebuffer OCR.

Covered:
  1.  seeded nested read:       dcat /HOME/DOCS/WELCOME.TXT (klog, 42 bytes)
  2.  shell mkdir:              mkdir /TEST9           (klog ok)
  3.  NESTED shell mkdir:       mkdir /TEST9/SUB       (klog ok)
  4.  shell cd:                 cd /TEST9/SUB          (klog chdir ok)
  5.  cwd-relative redirect:    run WRITER.ELF > REL.TXT lands in /TEST9/SUB
                                (open + flush klogs name the ABSOLUTE path)
  6.  FILES with no argv lists  exit = 1 entry (REL.TXT) in the cwd
      THE CWD (v1.9 list ABI):
  7.  ring-3 tree build:        run MKTREE.ELF -> exit 10 (all checks)
  8.  ring-3 tree walk:         run TREE.ELF /HOME -> exit 2 (DOCS + WELCOME.TXT)
  9.  absolute dcat after cd:   dcat /TEST9/SUB/REL.TXT (klog, 35 bytes)
 10.  negative mkdir:           mkdir /TEST9/SUB again -> "name exists" klog
 11.  negative rmdir:           rmdir /HOME (non-empty) -> "not empty" klog
 12.  ddel in subdir:           ddel /TEST9/SUB/REL.TXT (klog deleted)
 13.  rmdir cleanup:            rmdir /TEST9/SUB + /TEST9 (klog ok x2)
 14.  persistence across boot:  second boot on the SAME disk image:
                                /HOME/DOCS/WELCOME.TXT still readable,
                                /TEST9 gone (file: list error klog),
                                FILES.ELF / exits 3 (BIN, HOME, README.TXT)
"""
import os, re, shutil, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from glmq import QemuSession

ISO = "/home/z/glm-os/build/glm-os.iso"
WORK = "/home/z/glm-os/build/test19"
DISK = "/home/z/glm-os/build/disk.img"
DISK_COPY = os.path.join(WORK, "disk19.img")

os.environ["PATH"] = "/home/z/sysroot/usr/bin:" + os.environ.get("PATH", "")
os.environ["LD_LIBRARY_PATH"] = ("/home/z/sysroot/usr/lib/x86_64-linux-gnu:"
    "/home/z/sysroot/lib/x86_64-linux-gnu:" + os.environ.get("LD_LIBRARY_PATH", ""))

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

TASK_EXIT = re.compile(r"task (\d+) exited with code (-?\d+)")
EXIT_LINE = re.compile(r"sched: task \d+ exited with code (-?\d+)")

def run_line(q, line, settle=1.5):
    n = len(read_log(q))
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

shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK, exist_ok=True)
shutil.copyfile(DISK, DISK_COPY)

q = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
try:
    assert q.wait_serial_marker("boot complete", 120), "boot failed"
    print("boot ok")
    time.sleep(1)

    # 1. seeded nested read (the disk ships with /HOME/DOCS/WELCOME.TXT)
    n = run_line(q, "dcat /HOME/DOCS/WELCOME.TXT")
    ok = wait_for(q, "disk: cat /HOME/DOCS/WELCOME.TXT (42 bytes)", n)
    check("seeded nested read via dcat", ok)

    # 2. shell mkdir at the root
    n = run_line(q, "mkdir /TEST9")
    ok = wait_for(q, "fat32: disk: mkdir /TEST9 ok", n)
    check("shell mkdir /TEST9", ok)

    # 3. NESTED shell mkdir
    n = run_line(q, "mkdir /TEST9/SUB")
    ok = wait_for(q, "fat32: disk: mkdir /TEST9/SUB ok", n)
    check("shell mkdir /TEST9/SUB (nested)", ok)

    # 4. cd into the fresh directory
    n = run_line(q, "cd /TEST9/SUB")
    ok = wait_for(q, "file: chdir pid 1 -> /TEST9/SUB ok", n)
    check("cd /TEST9/SUB", ok)

    # 5. cwd-relative stdout redirect lands in the cwd (absolute proof)
    n = run_line(q, "run WRITER.ELF > REL.TXT", settle=3)
    ok1 = wait_for(q, "file: open fd=", n) and \
          "/TEST9/SUB/REL.TXT" in read_log(q)[n:]
    ok2 = wait_for(q, "file: flush /TEST9/SUB/REL.TXT (28 bytes)", n)
    check("relative > REL.TXT -> /TEST9/SUB/REL.TXT", ok1 and ok2)
    code = last_exit_after(q, n)
    check("WRITER exit = 0 (fixed program)", code == 0, f"code={code}")

    # 6. FILES.ELF with no argv lists the cwd and exits with the count
    n = run_line(q, "run FILES.ELF", settle=3)
    code = last_exit_after(q, n)
    check("FILES(cwd) exit = 1 entry", code == 1, f"code={code}")

    # 7. MKTREE: the whole ring-3 directory contract
    n = run_line(q, "run MKTREE.ELF", settle=6)
    code = last_exit_after(q, n)
    check("MKTREE exit = 10 checks", code == 10, f"code={code}")

    # 8. TREE: recursive walk from ring 3 (DOCS + WELCOME.TXT)
    n = run_line(q, "run TREE.ELF /HOME", settle=4)
    code = last_exit_after(q, n)
    check("TREE /HOME exit = 2 entries", code == 2, f"code={code}")

    # 9. absolute dcat of the file written through the relative redirect
    n = run_line(q, "dcat /TEST9/SUB/REL.TXT")
    ok = wait_for(q, "disk: cat /TEST9/SUB/REL.TXT (28 bytes)", n)
    check("dcat /TEST9/SUB/REL.TXT", ok)

    # 10. negative: mkdir over an existing name
    n = run_line(q, "mkdir /TEST9/SUB")
    ok = wait_for(q, "file: mkdir /TEST9/SUB: mkdir: name exists", n)
    check("duplicate mkdir refused", ok)

    # 11. negative: rmdir of a non-empty directory
    n = run_line(q, "rmdir /HOME")
    ok = wait_for(q, "file: rmdir /HOME: directory not empty", n)
    check("rmdir /HOME refused (not empty)", ok)

    # 12. delete a file inside a subdirectory
    n = run_line(q, "ddel /TEST9/SUB/REL.TXT")
    ok = wait_for(q, "fat32: disk: deleted /TEST9/SUB/REL.TXT", n)
    check("ddel /TEST9/SUB/REL.TXT", ok)

    # 13. rmdir the emptied directory and its parent, then cd home
    n = run_line(q, "cd /", settle=1)
    ok = wait_for(q, "file: chdir pid 1 -> / ok", n)
    check("cd / back", ok)
    n = run_line(q, "rmdir /TEST9/SUB")
    ok = wait_for(q, "fat32: disk: rmdir /TEST9/SUB ok", n)
    check("rmdir /TEST9/SUB", ok)
    n = run_line(q, "rmdir /TEST9")
    ok = wait_for(q, "fat32: disk: rmdir /TEST9 ok", n)
    check("rmdir /TEST9", ok)
finally:
    q.quit()

# 14. persistence: a fresh boot on the SAME disk image
q2 = QemuSession(ISO, WORK, smp="4", nic="user,model=e1000", disk=DISK_COPY)
try:
    assert q2.wait_serial_marker("boot complete", 120), "second boot failed"
    time.sleep(1)
    n = run_line(q2, "dcat /HOME/DOCS/WELCOME.TXT")
    ok = wait_for(q2, "disk: cat /HOME/DOCS/WELCOME.TXT (42 bytes)", n)
    check("persistence: nested seed readable after reboot", ok)

    n = run_line(q2, "run FILES.ELF /TEST9", settle=3)
    ok = wait_for(q2, "file: list /TEST9: no such file or directory", n)
    check("persistence: /TEST9 tree fully removed", ok)
    code = last_exit_after(q2, n)
    check("FILES /TEST9 exit = -1 (gone)", code == -1, f"code={code}")

    n = run_line(q2, "run FILES.ELF /", settle=3)
    code = last_exit_after(q2, n)
    check("FILES / exit = 5 (BIN, HOME, README.TXT, INDEX.HTM, HELLO.TXT since v2.4)", code == 5, f"code={code}")
finally:
    q2.quit()

print(f"\n=== v1.9: {PASS} pass / {FAIL} fail ===")
sys.exit(1 if FAIL else 0)
