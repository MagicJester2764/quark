/* Declaring what a program needs.
 *
 * A Quark program does not inherit authority from being run by root; it gets
 * exactly what a `.quark.manifest` section in its own image asks for, which
 * the spawner finds by scanning for the magic. That section is a plain
 * structure, so C declares one the same way Rust's `manifest!` does.
 *
 * Use it once, at file scope:
 *
 *     QUARK_MANIFEST(QUARK_CAP_PHYS_ALLOC(8));
 *
 * A program that asks for nothing needs no manifest at all: growing its own
 * heap and talking to services it was given descriptors for require none.
 */
#ifndef _QUARK_MANIFEST_H
#define _QUARK_MANIFEST_H

/* "QRKMANIF", little endian. */
#define QUARK_MANIFEST_MAGIC   0x46494E414D4B5251UL
#define QUARK_MANIFEST_VERSION 1UL

#define QUARK_CAP_IOPORT     1UL
#define QUARK_CAP_PHYS_RANGE 2UL
#define QUARK_CAP_IRQ        3UL
#define QUARK_CAP_TASK_MGMT  4UL
#define QUARK_CAP_PHYS_ALLOC 5UL
#define QUARK_CAP_SET_UID    6UL

/* Each request is three words: what kind, and up to two parameters. */
#define QUARK_CAP_IOPORT_RANGE(first, last) QUARK_CAP_IOPORT, (first), (last)
#define QUARK_CAP_IRQ_LINE(line)            QUARK_CAP_IRQ, (line), 0UL
/* Physical memory the allocator never owned — device registers, a
   framebuffer. Frames a program allocates for itself need no capability. */
#define QUARK_CAP_PHYS_WINDOW(start, end)   QUARK_CAP_PHYS_RANGE, (start), (end)
/* Permission to allocate physical frames, up to this many pages. */
#define QUARK_CAP_PHYS_ALLOC_N(pages)       QUARK_CAP_PHYS_ALLOC, (pages), 0UL
/* Create and start tasks; 0 means any target. */
#define QUARK_CAP_TASKS(target)             QUARK_CAP_TASK_MGMT, (target), 0UL

/* Emit the section. The count is worked out from what was passed, so adding a
   request cannot leave the header disagreeing with the body. */
#define QUARK_MANIFEST(...)                                                          \
    __attribute__((section(".quark.manifest"), used, aligned(8)))                    \
    static const unsigned long __quark_manifest[] = {                                \
        QUARK_MANIFEST_MAGIC,                                                        \
        QUARK_MANIFEST_VERSION,                                                      \
        (sizeof((const unsigned long[]){ __VA_ARGS__ }) / sizeof(unsigned long)) / 3, \
        __VA_ARGS__                                                                  \
    }

#endif
