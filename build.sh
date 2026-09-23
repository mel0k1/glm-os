#!/bin/bash
# GLM OS build: userland ELF64s + kernel ELF -> bootable ISO (Limine BIOS+UEFI)
set -e
cd "$(dirname "$0")"
source /home/z/my-project/scripts/env.sh

echo "[0/4] userland (ring-3 ELF64 programs)"
(cd user && cargo build --release)
mkdir -p ramdisk/BIN
cp user/target/x86_64-unknown-none/release/hello ramdisk/BIN/HELLO.ELF
cp user/target/x86_64-unknown-none/release/fault ramdisk/BIN/FAULT.ELF
cp user/target/x86_64-unknown-none/release/loop ramdisk/BIN/LOOP.ELF
cp user/target/x86_64-unknown-none/release/busy ramdisk/BIN/BUSY.ELF
cp user/target/x86_64-unknown-none/release/sigtest ramdisk/BIN/SIGTEST.ELF
cp user/target/x86_64-unknown-none/release/ping ramdisk/BIN/PING.ELF
cp user/target/x86_64-unknown-none/release/pong ramdisk/BIN/PONG.ELF
cp user/target/x86_64-unknown-none/release/forktest ramdisk/BIN/FORKTEST.ELF
cp user/target/x86_64-unknown-none/release/threadtest ramdisk/BIN/THREADTEST.ELF
cp user/target/x86_64-unknown-none/release/udpserv ramdisk/BIN/UDPSERV.ELF
cp user/target/x86_64-unknown-none/release/udpcli ramdisk/BIN/UDPCLI.ELF
cp user/target/x86_64-unknown-none/release/guidemo ramdisk/BIN/GUIDEMO.ELF
cp user/target/x86_64-unknown-none/release/tcpserv ramdisk/BIN/TCPSERV.ELF
cp user/target/x86_64-unknown-none/release/tcpcli ramdisk/BIN/TCPCLI.ELF
cp user/target/x86_64-unknown-none/release/counter ramdisk/BIN/COUNTER.ELF
cp user/target/x86_64-unknown-none/release/files ramdisk/BIN/FILES.ELF
cp user/target/x86_64-unknown-none/release/args ramdisk/BIN/ARGS.ELF
cp user/target/x86_64-unknown-none/release/argdump ramdisk/BIN/ARGDUMP.ELF
cp user/target/x86_64-unknown-none/release/runit ramdisk/BIN/RUNIT.ELF

echo "[1/4] cargo build (kernel)"
cargo build --release
mkdir -p build/isoroot/boot/limine

echo "[2/4] staging iso root"
cp target/x86_64-unknown-none/release/glm-os build/isoroot/boot/kernel.elf
cp limine.conf build/isoroot/boot/limine.conf

# FAT32 ramdisk (module), 32 MB, rebuilt fresh every time so new userland
# binaries and docs always land in the image
mkdir -p build/isoroot/boot/ramdisk
rm -f build/isoroot/boot/ramdisk.img
mkfs.vfat -C build/isoroot/boot/ramdisk.img -F 32 32768 >/dev/null
export MTOOLS_SKIP_CHECK=1
mmd -i build/isoroot/boot/ramdisk.img ::/BIN 2>/dev/null || true
for f in ramdisk/BIN/*; do
    mcopy -i build/isoroot/boot/ramdisk.img "$f" ::/BIN/ >/dev/null 2>&1
done
mmd -i build/isoroot/boot/ramdisk.img ::/DOCS 2>/dev/null || true
for f in ramdisk/DOCS/*; do
    mcopy -i build/isoroot/boot/ramdisk.img "$f" ::/DOCS/ >/dev/null 2>&1
done
for f in ramdisk/*.TXT; do
    mcopy -i build/isoroot/boot/ramdisk.img "$f" ::/ >/dev/null 2>&1
done

cp /home/z/limine-src/limine-binary/limine-bios.sys build/isoroot/boot/limine/
cp /home/z/limine-src/limine-binary/limine-bios-cd.bin build/isoroot/boot/limine/
cp /home/z/limine-src/limine-binary/limine-uefi-cd.bin build/isoroot/boot/limine/

echo "[3/4] xorriso -> glm-os.iso"
xorriso -as mkisofs -R -r -J \
    -b boot/limine/limine-bios-cd.bin \
    -no-emul-boot -boot-load-size 4 -boot-info-table \
    -hfsplus -apm-block-size 2048 \
    --modification-date=$(date +%Y%m%d%H%M%S00) \
    -o build/glm-os.iso build/isoroot

# v1.5: persistent FAT32 disk image (64 MB), seeded with the same BIN set
# plus a text file for dcat. Fresh every build; tests copy it when they
# need to verify cross-boot persistence.
echo "[3.5/4] persistent disk image (64 MB, seeded)"
rm -f build/disk.img
mkfs.vfat -C build/disk.img -F 32 65536 >/dev/null
MTOOLS_SKIP_CHECK=1 mmd -i build/disk.img ::/BIN 2>/dev/null || true
for f in ramdisk/BIN/*; do
    MTOOLS_SKIP_CHECK=1 mcopy -i build/disk.img "$f" ::/BIN/ >/dev/null 2>&1
done
printf 'GLM OS v1.7 - this file lives on the persistent AHCI disk.\nIf dcat shows this after a reboot, storage works.\n' > build/seed-readme.txt
MTOOLS_SKIP_CHECK=1 mcopy -i build/disk.img build/seed-readme.txt ::/README.TXT >/dev/null 2>&1

echo "[4/4] limine bios-install"
/home/z/limine-src/limine-binary/limine bios-install --force build/glm-os.iso
echo "OK: build/glm-os.iso"
