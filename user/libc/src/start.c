/* Program entry, and the pieces of the environment C expects to find set up.
 *
 * Quark starts a task at its ELF entry point with a stack and nothing else:
 * no argv on the stack, no environment, no auxiliary vector. Arguments arrive
 * on a page the spawner mapped, in a layout `quark_rt::args` also reads. So
 * `_start` is where that page becomes the `argc`/`argv` a C main() wants.
 */

#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <quark/layout.h>
#include <quark/syscall.h>

/* The argument page, mapped by the spawner by convention rather than by any
   mechanism — the same constant `quark_rt::spawn` uses.

   Layout: argc, then for each argument a length followed by its bytes. The
   strings are not NUL-terminated there, so they are copied out. */
#define ARGS_PAGE QUARK_ARGS_PAGE

/* Longest argument list this will assemble. A program wanting more than this
   is being called in a way the shell cannot express anyway. */
#define MAX_ARGS 32
/* Bytes for the copied, NUL-terminated argument strings. */
#define ARGS_BYTES 1024

static char *argv_slots[MAX_ARGS + 1];
static char argv_bytes[ARGS_BYTES];

int errno;

extern int main(int argc, char **argv);

/* Turn the spawner's page into a NULL-terminated argv. */
static int build_argv(void) {
    const unsigned char *page = (const unsigned char *)ARGS_PAGE;
    unsigned long count = *(const unsigned long *)page;
    unsigned long off = sizeof(unsigned long);
    size_t used = 0;
    int argc = 0;

    if (count > MAX_ARGS) {
        count = MAX_ARGS;
    }

    for (unsigned long i = 0; i < count; i++) {
        unsigned long len = *(const unsigned long *)(page + off);
        off += sizeof(unsigned long);

        /* A length that would run past the page is a malformed argument list,
           not a long argument; stop rather than read whatever follows. */
        if (off + len > 4096 || used + len + 1 > ARGS_BYTES) {
            break;
        }

        memcpy(&argv_bytes[used], page + off, len);
        argv_bytes[used + len] = '\0';
        argv_slots[argc++] = &argv_bytes[used];
        used += len + 1;
        off += len;
    }

    argv_slots[argc] = NULL;
    return argc;
}

/* The ELF entry point.
 *
 * Nothing returns here: `main` returning means exiting with its value, and
 * there is no caller to go back to. */
__attribute__((noreturn, used)) void _start(void) {
    int argc = build_argv();
    exit(main(argc, argv_slots));
}

void exit(int status) {
    __syscall1(SYS_EXIT_CODE, (unsigned long)(long)status);
    __builtin_unreachable();
}

void abort(void) {
    static const char msg[] = "abort\n";
    write(STDERR_FILENO, msg, sizeof(msg) - 1);
    exit(127);
}
