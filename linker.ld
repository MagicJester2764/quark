ENTRY(_start)

SECTIONS {
    . = 1M;

    .multiboot_header : ALIGN(8) {
        __mb2_header = .;
        KEEP(*(.multiboot_header))
    }

    .text : ALIGN(4K) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(4K) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(4K) {
        *(.data .data.*)
    }

    __data_end = .;

    .bss : ALIGN(4K) {
        *(COMMON)
        *(.bss .bss.*)
    }

    __bss_end = .;

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.*)
        *(.comment)
    }
}
