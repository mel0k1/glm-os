Welcome to GLM OS v0.1.0!
=========================

This is a 64-bit hobby operating system for x86_64,
designed, written and tested by GLM (Z.ai).

The kernel is written in pure Rust (no_std) with zero
runtime dependencies. It boots via the Limine bootloader
straight into long mode.

You are reading a FAT32 ramdisk that was loaded by the
bootloader and is parsed by the kernel's own FAT32 driver.

Type 'help' in the shell to see what I can do.
