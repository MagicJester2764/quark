ENTRY(_entry)

SECTIONS
{
    . = 0;

    .text : {
        *(.text.entry)
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.data .data.*) }
    .bss    : { *(.bss .bss.*) *(COMMON) }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.comment)
        *(.note*)
        *(.got*)
        *(.dynsym)
        *(.dynstr)
        *(.gnu.hash)
        *(.hash)
        *(.dynamic)
    }
}
