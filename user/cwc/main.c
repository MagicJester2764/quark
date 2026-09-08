/* wc(1), in C, on Quark.
 *
 * Deliberately ordinary C: argv, open/read/close, malloc, printf, string
 * handling. Nothing here knows it is running on a microkernel — which is the
 * point. The libc turns each of those into whatever Quark actually wants,
 * which for a file is a conversation with the VFS.
 */

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <quark/manifest.h>

/* Reading a file means handing the VFS a page to put the data in, and that
   page has to be one this program owns. Nothing else here needs authority:
   the heap grows without a capability, and the console is a descriptor the
   shell wired up. */
QUARK_MANIFEST(QUARK_CAP_PHYS_ALLOC_N(8));

struct counts {
    long lines;
    long words;
    long bytes;
};

static void count(const char *data, long n, struct counts *c) {
    int in_word = 0;
    for (long i = 0; i < n; i++) {
        char ch = data[i];
        c->bytes++;
        if (ch == '\n') {
            c->lines++;
        }
        if (ch == ' ' || ch == '\t' || ch == '\n' || ch == '\r') {
            in_word = 0;
        } else if (!in_word) {
            in_word = 1;
            c->words++;
        }
    }
}

/* Read a whole file into memory the caller frees. Returns its length, or -1. */
static long slurp(const char *path, char **out) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }

    long cap = 4096, len = 0;
    char *buf = malloc(cap);
    if (!buf) {
        close(fd);
        return -1;
    }

    for (;;) {
        if (len == cap) {
            /* Double rather than creep: a file read a page at a time would
               otherwise be copied once per page. */
            long grown = cap * 2;
            char *bigger = realloc(buf, grown);
            if (!bigger) {
                free(buf);
                close(fd);
                return -1;
            }
            buf = bigger;
            cap = grown;
        }
        long n = read(fd, buf + len, cap - len);
        if (n <= 0) {
            break;
        }
        len += n;
    }

    close(fd);
    *out = buf;
    return len;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <file> [file ...]\n", argv[0]);
        return 2;
    }

    struct counts total = { 0, 0, 0 };
    int failures = 0;

    for (int i = 1; i < argc; i++) {
        char *data = 0;
        long n = slurp(argv[i], &data);
        if (n < 0) {
            fprintf(stderr, "%s: cannot read %s\n", argv[0], argv[i]);
            failures++;
            continue;
        }

        struct counts c = { 0, 0, 0 };
        count(data, n, &c);
        free(data);

        printf("%7ld %7ld %7ld  %s\n", c.lines, c.words, c.bytes, argv[i]);
        total.lines += c.lines;
        total.words += c.words;
        total.bytes += c.bytes;
    }

    if (argc > 2) {
        printf("%7ld %7ld %7ld  total\n", total.lines, total.words, total.bytes);
    }
    return failures ? 1 : 0;
}
