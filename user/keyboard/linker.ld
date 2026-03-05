ENTRY(_start)

SECTIONS {
    . = 0x8000000000;

    .text : ALIGN(4K) {
        KEEP(*(.text.entry))
        *(.text .text.* .ltext .ltext.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.* .lrodata .lrodata.* .lrodata.str*)
    }

    .got : ALIGN(4K) {
        *(.got .got.plt)
    }

    .data : ALIGN(4K) {
        *(.data .data.* .ldata .ldata.*)
    }

    .bss : ALIGN(4K) {
        *(COMMON)
        *(.bss .bss.* .lbss .lbss.*)
    }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.*)
        *(.comment)
    }
}
