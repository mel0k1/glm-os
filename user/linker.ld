/* GLM OS userland linker script: static ET_EXEC at 0x400000.
   Each section gets its own 4 KiB-aligned page so lld emits separate
   PT_LOAD segments with clean R/W/X protection (W^X for the loader). */

ENTRY(_start)

SECTIONS
{
    . = 0x400000;

    .text : ALIGN(4K)
    {
        *(.text .text.*)
    }

    .rodata : ALIGN(4K)
    {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K)
    {
        *(.data .data.*)
    }

    .bss : ALIGN(4K)
    {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ :
    {
        *(.eh_frame*)
        *(.note*)
        *(.comment)
        *(.debug*)
    }
}
