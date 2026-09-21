#!/bin/bash
# GLM OS build: kernel ELF -> bootable ISO (Limine BIOS+UEFI)
set -e
cd "$(dirname "$0")"
source /home/z/my-project/scripts/env.sh

echo "[1/4] cargo build"
cargo build --release
mkdir -p build/isoroot/boot/limine

echo "[2/4] staging iso root"
cp target/x86_64-unknown-none/release/glm-os build/isoroot/boot/kernel.elf
cp limine.conf build/isoroot/boot/limine.conf

# FAT32 ramdisk (module), 32 MB, filled with GLM OS files
mkdir -p build/isoroot/boot/ramdisk
if [ ! -f build/isoroot/boot/ramdisk.img ]; then
    mkfs.vfat -C build/isoroot/boot/ramdisk.img -F 32 32768 >/dev/null
    export MTOOLS_SKIP_CHECK=1
    for f in ramdisk/*.TXT; do
        mcopy -i build/isoroot/boot/ramdisk.img "$f" ::/ >/dev/null 2>&1
    done
    mmd -i build/isoroot/boot/ramdisk.img ::/DOCS 2>/dev/null || true
    for f in ramdisk/DOCS/*; do
        mcopy -i build/isoroot/boot/ramdisk.img "$f" ::/DOCS/ >/dev/null 2>&1
    done
fi

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

echo "[4/4] limine bios-install"
/home/z/limine-src/limine-binary/limine bios-install --force build/glm-os.iso
echo "OK: build/glm-os.iso"
