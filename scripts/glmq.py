#!/usr/bin/env python3
"""GLM OS QEMU test driver.

Запускает QEMU headless с монитором на unix-сокете, умеет:
  - ждать маркер в serial.log (сигнал от ядра)
  - снимать скриншоты VGA (screendump) -> PNG
  - «печатать» строки в гостя (sendkey)
  - корректно гасить виртуалку

Использование:
  glmq.py --iso build/glm-os.iso --shots-dir shots --marker "glm>" \
          --type "help" --type "about" ...
"""
import argparse
import os
import socket
import subprocess
import sys
import time

from PIL import Image


class QemuSession:
    def __init__(self, iso, workdir, mem="1G", smp="1", nic="none"):
        os.makedirs(workdir, exist_ok=True)
        self.workdir = workdir
        self.serial_log = os.path.join(workdir, "serial.log")
        if os.path.exists(self.serial_log):
            os.remove(self.serial_log)
        self.mon_path = os.path.join(workdir, "monitor.sock")
        if os.path.exists(self.mon_path):
            os.remove(self.mon_path)

        self.proc = subprocess.Popen(
            [
                "qemu-system-x86_64",
                "-L", "/home/z/sysroot/usr/share/seabios",
                "-L", "/home/z/sysroot/usr/share/qemu",
                "-M", "q35",
                "-m", mem,
                "-smp", smp,
                "-nic", nic,
                "-cdrom", iso,
                "-display", "none",
                "-monitor", f"unix:{self.mon_path},server,nowait",
                "-serial", f"file:{self.serial_log}",
                # -no-reboot only: guest reset -> QEMU exits. (-no-shutdown
                # would turn that into a VM pause, so it must NOT be set.)
                "-no-reboot",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
        )

        # ждём мониторы; если процесс умер — покажем stderr
        deadline = time.time() + 20
        while time.time() < deadline:
            if self.proc.poll() is not None:
                err = self.proc.stderr.read().decode(errors="replace")
                raise RuntimeError(f"QEMU exited early: {err.strip()[:500]}")
            try:
                self.mon = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                self.mon.connect(self.mon_path)
                break
            except (ConnectionRefusedError, FileNotFoundError):
                time.sleep(0.2)
        else:
            raise RuntimeError("monitor socket never appeared")
        self.mon.settimeout(2.0)
        self._drain()

    # ------------------------------------------------------------------ HMP
    def _drain(self):
        data = b""
        try:
            while True:
                chunk = self.mon.recv(4096)
                if not chunk:
                    break
                data += chunk
                if b"(qemu)" in data:
                    break
        except socket.timeout:
            pass
        return data.decode(errors="replace")

    def hmp(self, cmd):
        self.mon.sendall((cmd + "\n").encode())
        time.sleep(0.05)
        return self._drain()

    # ------------------------------------------------------------- actions
    def wait_serial_marker(self, marker, timeout=90):
        deadline = time.time() + timeout
        seen = ""
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"QEMU exited early (code {self.proc.returncode})")
            try:
                seen = open(self.serial_log, errors="replace").read()
                if marker in seen:
                    return True
            except FileNotFoundError:
                pass
            time.sleep(0.2)
        return False

    def screendump(self, name):
        ppm = os.path.join(self.workdir, name + ".ppm")
        png = os.path.join(self.workdir, name + ".png")
        self.hmp(f"screendump {ppm}")
        for _ in range(50):
            if os.path.exists(ppm) and os.path.getsize(ppm) > 0:
                break
            time.sleep(0.1)
        img = Image.open(ppm)
        img.save(png)
        os.remove(ppm)
        return png

    @staticmethod
    def keymap(ch):
        special = {
            " ": "spc", "\n": "ret", "-": "minus", "=": "equal", ".": "dot",
            ",": "comma", "/": "slash", ";": "semicolon", "'": "apostrophe",
            "[": "bracket_left", "]": "bracket_right", "\\": "backslash",
            "`": "grave_accent",
        }
        if ch in special:
            return special[ch]
        if ch.isalpha():
            return f"shift-{ch.lower()}" if ch.isupper() else ch
        if ch.isdigit():
            return ch
        raise ValueError(f"unmapped char: {ch!r}")

    def type_text(self, text, key_gap=0.04):
        for ch in text:
            self.hmp(f"sendkey {self.keymap(ch)}")
            time.sleep(key_gap)

    def quit(self):
        try:
            self.hmp("quit")
        except Exception:
            pass
        time.sleep(0.5)
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iso", required=True)
    ap.add_argument("--shots-dir", required=True)
    ap.add_argument("--marker", default=None, help="wait for this text in serial.log")
    ap.add_argument("--marker-timeout", type=float, default=90)
    ap.add_argument("--type", action="append", default=[], help="text lines to type")
    ap.add_argument("--boot-delay", type=float, default=0.5)
    ap.add_argument("--smp", default="1", help="guest CPU count (-smp)")
    ap.add_argument("--type-delay", type=float, default=0.6, help="pause after each line")
    args = ap.parse_args()

    os.makedirs(args.shots_dir, exist_ok=True)
    qemu = QemuSession(args.iso, args.shots_dir, smp=args.smp)
    rc = 0
    try:
        if args.marker:
            ok = qemu.wait_serial_marker(args.marker, args.marker_timeout)
            if not ok:
                print("MARKER NOT SEEN in serial.log", file=sys.stderr)
                log = ""
                try:
                    log = open(qemu.serial_log, errors="replace").read()
                except FileNotFoundError:
                    pass
                print("---- serial.log so far ----")
                print(log[-3000:])
                rc = 2
        time.sleep(args.boot_delay)
        qemu.screendump("00-boot")

        for i, line in enumerate(args.type, start=1):
            qemu.type_text(line + "\n")
            time.sleep(args.type_delay)
            qemu.screendump(f"{i:02d}-cmd")
    finally:
        qemu.quit()
    sys.exit(rc)


if __name__ == "__main__":
    main()
