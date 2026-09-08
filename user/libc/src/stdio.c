/* Formatted output.
 *
 * One formatter, used by everything: `printf` renders into a buffer and writes
 * it, rather than writing a character at a time. On Quark a write to fd 1 is
 * an IPC round trip to the console server, so a character at a time would be
 * one message per character.
 */

#include <stdarg.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/* Where a formatted line is assembled before it is written. Bigger than a
   terminal line so ordinary output is one write. */
#define PRINT_BUF 512

/* Somewhere to put the digits of the widest number, in the shortest base. */
#define NUM_BUF 24

struct out {
    char *buf;
    size_t size; /* 0 means "no limit", for the printf family */
    size_t len;  /* what would have been written, standard-style */
};

static void put(struct out *o, char c) {
    if (o->size == 0 || o->len + 1 < o->size) {
        o->buf[o->len] = c;
    }
    o->len++;
}

static void puts_n(struct out *o, const char *s, size_t n) {
    for (size_t i = 0; i < n; i++) {
        put(o, s[i]);
    }
}

static void number(struct out *o, unsigned long v, unsigned base, int upper, int width,
                   int zero) {
    static const char lower_digits[] = "0123456789abcdef";
    static const char upper_digits[] = "0123456789ABCDEF";
    const char *digits = upper ? upper_digits : lower_digits;
    char tmp[NUM_BUF];
    int n = 0;

    if (v == 0) {
        tmp[n++] = '0';
    }
    while (v > 0 && n < NUM_BUF) {
        tmp[n++] = digits[v % base];
        v /= base;
    }
    for (int i = n; i < width; i++) {
        put(o, zero ? '0' : ' ');
    }
    while (n > 0) {
        put(o, tmp[--n]);
    }
}

static void format(struct out *o, const char *fmt, va_list ap) {
    for (const char *p = fmt; *p; p++) {
        if (*p != '%') {
            put(o, *p);
            continue;
        }
        p++;
        if (*p == '\0') {
            break;
        }

        int zero = 0, width = 0, longarg = 0;
        if (*p == '0') {
            zero = 1;
            p++;
        }
        while (*p >= '0' && *p <= '9') {
            width = width * 10 + (*p - '0');
            p++;
        }
        while (*p == 'l') {
            longarg = 1;
            p++;
        }
        /* size_t and friends. Everything here is 64-bit, so they are longs. */
        if (*p == 'z') {
            longarg = 1;
            p++;
        }

        switch (*p) {
        case 'd': {
            long v = longarg ? va_arg(ap, long) : (long)va_arg(ap, int);
            if (v < 0) {
                put(o, '-');
                /* Negating the most negative value overflows; go via
                   unsigned, where the bit pattern is already what is wanted. */
                number(o, (unsigned long)(-(v + 1)) + 1, 10, 0, width, zero);
            } else {
                number(o, (unsigned long)v, 10, 0, width, zero);
            }
            break;
        }
        case 'u': {
            unsigned long v =
                longarg ? va_arg(ap, unsigned long) : (unsigned long)va_arg(ap, unsigned);
            number(o, v, 10, 0, width, zero);
            break;
        }
        case 'x':
        case 'X': {
            unsigned long v =
                longarg ? va_arg(ap, unsigned long) : (unsigned long)va_arg(ap, unsigned);
            number(o, v, 16, *p == 'X', width, zero);
            break;
        }
        case 'p':
            puts_n(o, "0x", 2);
            number(o, (unsigned long)va_arg(ap, void *), 16, 0, 0, 0);
            break;
        case 'c':
            put(o, (char)va_arg(ap, int));
            break;
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) {
                s = "(null)";
            }
            size_t n = strlen(s);
            for (size_t i = n; i < (size_t)width; i++) {
                put(o, ' ');
            }
            puts_n(o, s, n);
            break;
        }
        case '%':
            put(o, '%');
            break;
        default:
            /* Keep an unknown specifier visible rather than swallowing it:
               the caller has a bug and should see it. */
            put(o, '%');
            put(o, *p);
            break;
        }
    }
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap) {
    struct out o = { buf, size, 0 };
    format(&o, fmt, ap);
    if (size > 0) {
        buf[o.len < size ? o.len : size - 1] = '\0';
    }
    return (int)o.len;
}

int snprintf(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return n;
}

static int vdprintf(int fd, const char *fmt, va_list ap) {
    char buf[PRINT_BUF];
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    size_t len = (size_t)n < sizeof buf - 1 ? (size_t)n : sizeof buf - 1;
    write(fd, buf, len);
    return n;
}

int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vdprintf(STDOUT_FILENO, fmt, ap);
    va_end(ap);
    return n;
}

int fprintf(int fd, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vdprintf(fd, fmt, ap);
    va_end(ap);
    return n;
}

int puts(const char *s) {
    write(STDOUT_FILENO, s, strlen(s));
    write(STDOUT_FILENO, "\n", 1);
    return 0;
}

int putchar(int c) {
    char ch = (char)c;
    write(STDOUT_FILENO, &ch, 1);
    return c;
}
