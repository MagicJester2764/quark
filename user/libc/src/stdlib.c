/* Allocation, and the number parsing that comes with it.
 *
 * The allocator is a bump allocator over pages taken from the kernel, with a
 * free list for reuse. It is the same shape as the Rust runtime's and for the
 * same reason: there is no `brk` on Quark, so growing the heap means asking
 * for more pages at an address of our choosing.
 */

#include <errno.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <quark/layout.h>
#include <quark/syscall.h>

#define PAGE_SIZE 4096

#define HEAP_START QUARK_HEAP_START
#define HEAP_LIMIT QUARK_HEAP_LIMIT

/* A block's size and where it came from, kept just below the pointer handed
   out — the only place a caller cannot pass but `free` can find. */
struct header {
    size_t size; /* the whole block, this header included */
    struct header *next;
};

/* Every function that fails sets this, so it lives with the library rather
   than with the entry point. */
int errno;

static struct header *free_list;
static unsigned long heap_top = HEAP_START;

#define ALIGN 16
#define HDR sizeof(struct header)

static size_t round_up(size_t v, size_t a) {
    return (v + a - 1) & ~(a - 1);
}

/* Take more pages from the kernel.
 *
 * `heap_top` is where to look next, not memory already held: another
 * allocator in the same program may have taken the address, and the kernel
 * says so rather than handing over what is already in use. */
static int grow(size_t bytes) {
    size_t size = round_up(bytes, PAGE_SIZE);
    unsigned long pages = size / PAGE_SIZE;
    unsigned long at = heap_top;

    while (__syscall2(SYS_MMAP, at, pages) == QUARK_ERR) {
        at += size;
        if (at + size > HEAP_LIMIT) {
            return -1;
        }
    }

    struct header *block = (struct header *)at;
    block->size = size;
    block->next = free_list;
    free_list = block;
    heap_top = at + size;
    return 0;
}

void *malloc(size_t n) {
    if (n == 0) {
        n = 1;
    }
    size_t need = round_up(n + HDR, ALIGN);

    for (int attempt = 0; attempt < 2; attempt++) {
        struct header **prev = &free_list;
        for (struct header *b = free_list; b; prev = &b->next, b = b->next) {
            if (b->size < need) {
                continue;
            }
            /* Split when what is left could hold something; otherwise hand
               over the whole block, since a fragment too small to allocate
               from is one that can never be used again. */
            if (b->size - need >= HDR + ALIGN) {
                struct header *rest = (struct header *)((char *)b + need);
                rest->size = b->size - need;
                rest->next = b->next;
                *prev = rest;
                b->size = need;
            } else {
                *prev = b->next;
            }
            b->next = 0;
            return (char *)b + HDR;
        }
        if (grow(need) != 0) {
            break;
        }
    }

    errno = ENOMEM;
    return 0;
}

void free(void *p) {
    if (!p) {
        return;
    }
    struct header *b = (struct header *)((char *)p - HDR);
    /* No coalescing: blocks are not kept in address order, and a filesystem
       tool's allocation pattern does not need it. Worth revisiting when
       something long-running does. */
    b->next = free_list;
    free_list = b;
}

void *calloc(size_t n, size_t size) {
    /* A multiplication that wraps would allocate less than the caller thinks
       it asked for, and they would write past it. */
    if (size != 0 && n > (size_t)-1 / size) {
        errno = ENOMEM;
        return 0;
    }
    size_t total = n * size;
    void *p = malloc(total);
    if (p) {
        memset(p, 0, total);
    }
    return p;
}

void *realloc(void *p, size_t n) {
    if (!p) {
        return malloc(n);
    }
    if (n == 0) {
        free(p);
        return 0;
    }
    struct header *b = (struct header *)((char *)p - HDR);
    size_t have = b->size - HDR;
    if (have >= n) {
        return p;
    }
    void *q = malloc(n);
    if (q) {
        memcpy(q, p, have);
        free(p);
    }
    return q;
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

long strtol(const char *s, char **end, int base) {
    while (*s == ' ' || *s == '\t' || *s == '\n') {
        s++;
    }
    int neg = 0;
    if (*s == '-') {
        neg = 1;
        s++;
    } else if (*s == '+') {
        s++;
    }
    if ((base == 0 || base == 16) && s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
        s += 2;
        base = 16;
    } else if (base == 0) {
        base = s[0] == '0' ? 8 : 10;
    }

    long v = 0;
    for (;; s++) {
        int d;
        if (*s >= '0' && *s <= '9') {
            d = *s - '0';
        } else if (*s >= 'a' && *s <= 'z') {
            d = *s - 'a' + 10;
        } else if (*s >= 'A' && *s <= 'Z') {
            d = *s - 'A' + 10;
        } else {
            break;
        }
        if (d >= base) {
            break;
        }
        v = v * base + d;
    }
    if (end) {
        *end = (char *)s;
    }
    return neg ? -v : v;
}

int atoi(const char *s) {
    return (int)strtol(s, 0, 10);
}
