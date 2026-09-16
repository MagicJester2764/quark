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
   theirs, and five auxiliary entries.
 *
 * A static, which is what makes it writable — and musl needs that. Its
 * `unsetenv` shuffles this array in place. It never writes through the
 * pointers, so the strings themselves may stay wherever they are. */
static unsigned long start_block[1 + MAX_ARGS + 1 + MAX_ENV + 1 + 10];
static char argv_bytes[ARGS_BYTES];
static char env_bytes[ENV_BYTES];

#define AT_NULL   0
#define AT_PHDR   3
#define AT_PHENT  4
#define AT_PHNUM  5
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
    /* The page size, because musl takes it from here and a zero page size
       makes its allocator do arithmetic on nothing. */
    start_block[i++] = AT_PAGESZ;
    start_block[i++] = 4096;

    /* Where the program headers are, which is how musl finds the thread-local
       template of a static program. Without them it gave every thread a TLS
       block with no room for the program's own thread-locals — and on x86-64
       those sit *below* the thread pointer, so every write to one landed on
       whatever came before the block. musl itself never noticed, keeping
       errno and the rest in its thread structure; pixman did, because its
       composite cache is a thread-local, and every lookup in it came back as
       some other operator's function.
     *
     * The spawner copies the table to the end of this page, since a program's
       own headers are not in any segment it loads. A spawner older than that
       leaves the count zero, and the program starts as it always did. */
    const unsigned long *phdrs = (const unsigned long *)(page + QUARK_PHDRS_AT);
    unsigned long phent = phdrs[0];
    unsigned long phnum = phdrs[1];
    if (phnum != 0 && phnum <= QUARK_MAX_PHDRS && phent == QUARK_PHDR_SIZE) {
        start_block[i++] = AT_PHDR;
        start_block[i++] = ARGS_PAGE + QUARK_PHDRS_AT + 2 * sizeof(unsigned long);
        start_block[i++] = AT_PHENT;
        start_block[i++] = phent;
        start_block[i++] = AT_PHNUM;
        start_block[i++] = phnum;
    }

    start_block[i++] = AT_NULL;
    start_block[i++] = 0;

    return start_block;
}
