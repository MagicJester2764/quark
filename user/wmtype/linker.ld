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

    .quark.manifest : ALIGN(8) {
        KEEP(*(.quark.manifest))
    }

    .got : ALIGN(4K) {
        *(.got .got.plt)
    }

    .data : ALIGN(4K) {
        *(.data .data.* .ldata .ldata.*)
    }

    /* Thread-local storage template.
     *
     * .tdata is the initialised image and .tbss the zeroed tail; together they
     * are what each thread receives a private copy of. x86-64 uses variant II,
     * where the thread pointer sits at the *end* of a thread's block and the
     * variables live at negative offsets from it — so the runtime places FS at
     * the top, not the bottom.
     *
     * The symbols below hand the runtime the template's address and size. A
     * static binary has no other way to find its own PT_TLS at run time.
     */
    /* Page aligned so a segment boundary here cannot land mid-page. Two
     * PT_LOADs sharing a page defeats the loader, which allocates a frame per
     * page per segment: the second mapping replaces the first, and whatever
     * the first had loaded — the GOT, in the case that found this — is gone.
     */
    /* Advance the counter itself rather than aligning the section: for a
     * program with no thread-locals these sections are empty, and lld
     * collapses them, ignoring a section alignment. Without this the RW file
     * segment ends mid-page and the next one begins in the same page.
     */
    . = ALIGN(4K);

    .tdata : {
        __tdata_start = .;
        *(.tdata .tdata.*)
        __tdata_end = .;
    }

    /* No ALIGN here: .tbss must follow .tdata directly, or the gap inflates
     * __tls_size past the PT_TLS MemSiz the linker used to compute each
     * variable's offset, and every thread-local lands in the wrong place.
     */
    .tbss : {
        __tbss_start = .;
        *(.tbss .tbss.* .tcommon)
        __tbss_end = .;
    }

    /* The alignment the linker gave the TLS segment. Variant II places the
     * block at TP - ALIGN_UP(size, align), so the runtime needs the real
     * value: assuming one puts every thread-local off by the padding.
     */
    __tls_align = ALIGNOF(.tdata);

    __tdata_size = __tdata_end - __tdata_start;
    __tbss_size = __tbss_end - __tbss_start;
    __tls_size = __tbss_end - __tdata_start;

    .bss : ALIGN(4K) {
        *(COMMON)
        *(.bss .bss.* .lbss .lbss.*)
    }

    /* Non-alloc sections, placed explicitly and last.
     *
     * Declaring PHDRS changed how lld places orphans: .symtab was given file
     * offset 0x1000, on top of .text. Address 0 marks these as non-alloc, and
     * naming them keeps them out of the loaded segments.
     */
    .symtab 0 : { *(.symtab) }
    .strtab 0 : { *(.strtab) }
    .shstrtab 0 : { *(.shstrtab) }

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note.*)
        *(.comment)
    }
}
