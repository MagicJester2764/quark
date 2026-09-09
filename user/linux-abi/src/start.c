/* Handing musl the process it expects to wake up in.
 *
 * A Linux program starts with argc, argv, the environment and the auxiliary
 * vector laid out on its stack, and musl's `_start` passes the stack pointer
 * straight to `_start_c`. Quark starts a task at its entry point with a stack
 * and nothing else: the arguments are on a page the spawner mapped, in a
 * layout of its own.
 *
 * So the two protocols are different and neither is going to change. This
 * builds the one musl reads out of the one Quark provides — the only part of
 * the port that is about processes rather than system calls.
 */

#include <quark/layout.h>
#include <quark/syscall.h>

#define ARGS_PAGE QUARK_ARGS_PAGE

#define MAX_ARGS   32
#define ARGS_BYTES 1024

/* argc, the argv pointers, argv's terminator, an empty environment, and one
   auxiliary entry. */
static unsigned long start_block[1 + MAX_ARGS + 1 + 1 + 4];
static char argv_bytes[ARGS_BYTES];

#define AT_NULL   0
#define AT_PAGESZ 6

unsigned long *__quark_start_args(void);

unsigned long *__quark_start_args(void) {
    const unsigned char *page = (const unsigned char *)ARGS_PAGE;
    unsigned long count = *(const unsigned long *)page;
    unsigned long off = sizeof(unsigned long);
    unsigned long used = 0;
    unsigned long argc = 0;

    if (count > MAX_ARGS) {
        count = MAX_ARGS;
    }

    for (unsigned long i = 0; i < count; i++) {
        unsigned long len = *(const unsigned long *)(page + off);
        off += sizeof(unsigned long);

        /* A length running past the page is a malformed list rather than a
           long argument. Stop instead of reading whatever follows. */
        if (off + len > 4096 || used + len + 1 > ARGS_BYTES) {
            break;
        }
        char *dst = &argv_bytes[used];
        for (unsigned long j = 0; j < len; j++) {
            dst[j] = (char)page[off + j];
        }
        dst[len] = '\0';
        start_block[1 + argc] = (unsigned long)dst;
        argc++;
        used += len + 1;
        off += len;
    }

    unsigned long i = 0;
    start_block[0] = argc;
    i = 1 + argc;
    start_block[i++] = 0;          /* end of argv */
    start_block[i++] = 0;          /* end of an empty environment */
    /* One auxiliary entry, because musl takes the page size from here and a
       zero page size makes its allocator do arithmetic on nothing. */
    start_block[i++] = AT_PAGESZ;
    start_block[i++] = 4096;
    start_block[i++] = AT_NULL;
    start_block[i++] = 0;

    return start_block;
}
