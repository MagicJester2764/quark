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
#define MAX_ENV    32
#define ENV_BYTES  1024

/* argc, the argv pointers and their terminator, the environment pointers and
   theirs, and two auxiliary entries.
 *
 * A static, which is what makes it writable — and musl needs that. Its
 * `unsetenv` shuffles this array in place. It never writes through the
 * pointers, so the strings themselves may stay wherever they are. */
static unsigned long start_block[1 + MAX_ARGS + 1 + MAX_ENV + 1 + 4];
static char argv_bytes[ARGS_BYTES];
static char env_bytes[ENV_BYTES];

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

    /* The environment follows the arguments on the same page. A program built
       before it existed reads the count as zero, which is why it is there and
       not in front. */
    unsigned long envc = 0;
    unsigned long env_used = 0;
    unsigned long env_ptrs[MAX_ENV];
    if (off + sizeof(unsigned long) <= 4096) {
        unsigned long declared = *(const unsigned long *)(page + off);
        off += sizeof(unsigned long);
        for (unsigned long e = 0; e < declared && envc < MAX_ENV; e++) {
            if (off + sizeof(unsigned long) > 4096) {
                break;
            }
            unsigned long len = *(const unsigned long *)(page + off);
            off += sizeof(unsigned long);
            if (off + len > 4096 || env_used + len + 1 > ENV_BYTES) {
                break;
            }
            char *dst = &env_bytes[env_used];
            for (unsigned long j = 0; j < len; j++) {
                dst[j] = (char)page[off + j];
            }
            dst[len] = '\0';
            env_ptrs[envc++] = (unsigned long)dst;
            env_used += len + 1;
            off += len;
        }
    }

    unsigned long i = 0;
    start_block[0] = argc;
    i = 1 + argc;
    start_block[i++] = 0;          /* end of argv */
    for (unsigned long e = 0; e < envc; e++) {
        start_block[i++] = env_ptrs[e];
    }
    start_block[i++] = 0;          /* end of the environment */
    /* One auxiliary entry, because musl takes the page size from here and a
       zero page size makes its allocator do arithmetic on nothing. */
    start_block[i++] = AT_PAGESZ;
    start_block[i++] = 4096;
    start_block[i++] = AT_NULL;
    start_block[i++] = 0;

    return start_block;
}
