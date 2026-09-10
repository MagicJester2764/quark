/* The environment.
 *
 * The strings live on the page a spawner maps, which is read-only, and that is
 * fine: nothing here writes through them. `environ` is an array of pointers in
 * our own memory, and `setenv` allocates a new string rather than editing one
 * in place. That is the same arrangement musl needs over the same page — its
 * `unsetenv` shuffles the pointer array and never touches a string.
 */

#include <quark/layout.h>
#include <stdlib.h>

#define ARGS_PAGE  QUARK_ARGS_PAGE
#define PAGE_BYTES 4096
#define MAX_ENV    64

static char *slots[MAX_ENV + 1];
char **environ = slots;
static int ready;

/* Storage for entries: the ones copied off the page at startup, and the ones
   set at run time. A program that runs out gets a refusal rather than a
   corrupted array. */
static char pool[2048];
static unsigned long pool_used;

static unsigned long read_word(const unsigned char *p) {
    return *(const unsigned long *)p;
}

static unsigned long length(const char *s) {
    unsigned long n = 0;
    while (s[n]) {
        n++;
    }
    return n;
}

/* Does `entry` begin with `name` followed by '='?
 *
 * The '=' is the point. Without it, HOM matches HOME=/home/root. */
static int same_name(const char *entry, const char *name, unsigned long n) {
    for (unsigned long i = 0; i < n; i++) {
        if (entry[i] != name[i]) {
            return 0;
        }
    }
    return entry[n] == '=';
}

static char *pool_take(unsigned long bytes) {
    if (pool_used + bytes > sizeof pool) {
        return 0;
    }
    char *at = pool + pool_used;
    pool_used += bytes;
    return at;
}

/* Point `environ` at the entries on the args page.
 *
 * The page holds lengths and bytes with no terminators, and `getenv` returns a
 * C string, so each entry is copied into the pool with a null on the end. The
 * page itself is never written. */
static void init(void) {
    if (ready) {
        return;
    }
    ready = 1;
    slots[0] = 0;

    const unsigned char *page = (const unsigned char *)ARGS_PAGE;
    unsigned long off = 0;
    unsigned long argc = read_word(page);
    off += sizeof(unsigned long);
    for (unsigned long i = 0; i < argc; i++) {
        if (off + sizeof(unsigned long) > PAGE_BYTES) {
            return;
        }
        unsigned long len = read_word(page + off);
        off += sizeof(unsigned long) + len;
        if (off > PAGE_BYTES) {
            return;
        }
    }
    if (off + sizeof(unsigned long) > PAGE_BYTES) {
        return;
    }
    unsigned long envc = read_word(page + off);
    off += sizeof(unsigned long);

    int n = 0;
    for (unsigned long i = 0; i < envc && n < MAX_ENV; i++) {
        if (off + sizeof(unsigned long) > PAGE_BYTES) {
            break;
        }
        unsigned long len = read_word(page + off);
        off += sizeof(unsigned long);
        if (off + len > PAGE_BYTES) {
            break;
        }
        char *dst = pool_take(len + 1);
        if (!dst) {
            break;
        }
        for (unsigned long j = 0; j < len; j++) {
            dst[j] = (char)page[off + j];
        }
        dst[len] = '\0';
        off += len;
        slots[n++] = dst;
    }
    slots[n] = 0;
}

char *getenv(const char *name) {
    init();
    if (!name) {
        return 0;
    }
    unsigned long n = length(name);
    if (n == 0) {
        return 0;
    }
    for (int i = 0; environ[i]; i++) {
        if (same_name(environ[i], name, n)) {
            return environ[i] + n + 1;
        }
    }
    return 0;
}

int setenv(const char *name, const char *value, int overwrite) {
    init();
    if (!name || !value) {
        return -1;
    }
    unsigned long n = length(name);
    unsigned long v = length(value);
    if (n == 0) {
        return -1;
    }

    int at = -1;
    int end = 0;
    for (; environ[end]; end++) {
        if (at < 0 && same_name(environ[end], name, n)) {
            at = end;
        }
    }
    if (at >= 0 && !overwrite) {
        return 0;
    }
    if (at < 0 && end >= MAX_ENV) {
        return -1;
    }

    char *entry = pool_take(n + v + 2);
    if (!entry) {
        return -1;
    }
    for (unsigned long i = 0; i < n; i++) {
        entry[i] = name[i];
    }
    entry[n] = '=';
    for (unsigned long i = 0; i < v; i++) {
        entry[n + 1 + i] = value[i];
    }
    entry[n + 1 + v] = '\0';

    if (at >= 0) {
        environ[at] = entry;
    } else {
        environ[end] = entry;
        environ[end + 1] = 0;
    }
    return 0;
}

int unsetenv(const char *name) {
    init();
    if (!name) {
        return -1;
    }
    unsigned long n = length(name);
    if (n == 0) {
        return -1;
    }
    int w = 0;
    for (int r = 0; environ[r]; r++) {
        if (same_name(environ[r], name, n)) {
            continue;
        }
        environ[w++] = environ[r];
    }
    environ[w] = 0;
    return 0;
}
