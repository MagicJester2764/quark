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
#include <fcntl.h>
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

int sprintf(char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    /* No size to respect, which is why nothing here should use it — it is
       provided because ported code does. */
    int n = vsnprintf(buf, (size_t)-1, fmt, ap);
    va_end(ap);
    return n;
}

/* The streams.
 *
 * A FILE is a descriptor plus the two flags a caller can ask about. There is
 * no buffer, so there is nothing for fflush to do and nothing to lose if a
 * program exits without calling it.
 */
struct _IO_FILE {
    int fd;
    int eof;
    int err;
};

static FILE streams[3] = {
    { 0, 0, 0 },
    { 1, 0, 0 },
    { 2, 0, 0 },
};

FILE *stdin = &streams[0];
FILE *stdout = &streams[1];
FILE *stderr = &streams[2];

/* Room for the open streams a program may have at once, beyond the three it
   starts with. The descriptor table underneath is the real limit. */
#define MAX_STREAMS 16
static FILE open_streams[MAX_STREAMS];
static char stream_used[MAX_STREAMS];

int vfprintf(FILE *f, const char *fmt, va_list ap) {
    if (!f) {
        return -1;
    }
    char buf[PRINT_BUF];
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    size_t len = (size_t)n < sizeof buf - 1 ? (size_t)n : sizeof buf - 1;
    if (write(f->fd, buf, len) < 0) {
        f->err = 1;
    }
    return n;
}

int vprintf(const char *fmt, va_list ap) {
    return vfprintf(stdout, fmt, ap);
}

int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return n;
}

int fprintf(FILE *f, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(f, fmt, ap);
    va_end(ap);
    return n;
}

size_t fwrite(const void *ptr, size_t size, size_t n, FILE *f) {
    if (!f || size == 0) {
        return 0;
    }
    ssize_t w = write(f->fd, ptr, size * n);
    if (w < 0) {
        f->err = 1;
        return 0;
    }
    return (size_t)w / size;
}

size_t fread(void *ptr, size_t size, size_t n, FILE *f) {
    if (!f || size == 0) {
        return 0;
    }
    ssize_t r = read(f->fd, ptr, size * n);
    if (r < 0) {
        f->err = 1;
        return 0;
    }
    if (r == 0) {
        f->eof = 1;
    }
    return (size_t)r / size;
}

int fputs(const char *s, FILE *f) {
    size_t len = strlen(s);
    return fwrite(s, 1, len, f) == len ? 0 : EOF;
}

int fputc(int c, FILE *f) {
    char ch = (char)c;
    return fwrite(&ch, 1, 1, f) == 1 ? c : EOF;
}

int fgetc(FILE *f) {
    char ch;
    if (fread(&ch, 1, 1, f) != 1) {
        return EOF;
    }
    return (unsigned char)ch;
}

int getchar(void) {
    return fgetc(stdin);
}

char *fgets(char *s, int size, FILE *f) {
    if (size <= 0) {
        return NULL;
    }
    int i = 0;
    while (i < size - 1) {
        int c = fgetc(f);
        if (c == EOF) {
            break;
        }
        s[i++] = (char)c;
        if (c == '\n') {
            break;
        }
    }
    if (i == 0) {
        return NULL;
    }
    s[i] = '\0';
    return s;
}

FILE *fopen(const char *path, const char *mode) {
    int flags = O_RDONLY;
    if (mode && (mode[0] == 'w' || mode[0] == 'a')) {
        flags = O_WRONLY | O_CREAT;
    }
    int fd = open(path, flags);
    if (fd < 0) {
        return NULL;
    }
    for (int i = 0; i < MAX_STREAMS; i++) {
        if (!stream_used[i]) {
            stream_used[i] = 1;
            open_streams[i].fd = fd;
            open_streams[i].eof = 0;
            open_streams[i].err = 0;
            return &open_streams[i];
        }
    }
    close(fd);
    return NULL;
}

int fclose(FILE *f) {
    if (!f) {
        return EOF;
    }
    int r = close(f->fd);
    for (int i = 0; i < MAX_STREAMS; i++) {
        if (&open_streams[i] == f) {
            stream_used[i] = 0;
        }
    }
    return r;
}

/* Nothing is buffered, so there is nothing to write out. Reported as success
   because it is: everything this stream was given has already gone. */
int fflush(FILE *f) {
    (void)f;
    return 0;
}

int feof(FILE *f) { return f ? f->eof : 0; }
int ferror(FILE *f) { return f ? f->err : 0; }
void clearerr(FILE *f) { if (f) { f->eof = 0; f->err = 0; } }
int fileno(FILE *f) { return f ? f->fd : -1; }

int puts(const char *s) {
    fputs(s, stdout);
    fputc('\n', stdout);
    return 0;
}

int putchar(int c) {
    return fputc(c, stdout);
}
