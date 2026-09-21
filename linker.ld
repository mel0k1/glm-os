/* GLM OS linker script
 * Kernel linked in the higher half at 0xFFFFFFFF80000000 (classic x86_64 kernel base).
 * Limine loads the ELF segments and maps them at these virtual addresses. */

ENTRY(_start)

KERNEL_BASE = 0xFFFFFFFF80000000;

SECTIONS
{
    . = KERNEL_BASE;

    __kernel_virt_start = .;

    .text ALIGN(0x1000) : {
        __text_start = .;
        *(.text .text.*)
        __text_end = .;
    }

    /* Limine boot protocol request region.
     * Order is CRITICAL: start marker -> requests -> end marker.
     * The linker may otherwise reorder input sections freely. */
    .limine_requests ALIGN(0x1000) : {
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
    }

    .rodata ALIGN(0x1000) : {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    }

    .data ALIGN(0x1000) : {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
        *(.data.rel.ro .data.rel.ro.*)
        __data_end = .;
    }

    .bss ALIGN(0x1000) : {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    }

    __kernel_virt_end = .;

    /DISCARD/ : {
        *(.eh_frame .eh_frame.*)
        *(.note .note.*)
        *(.comment)
        *(.interp)
    }
}
