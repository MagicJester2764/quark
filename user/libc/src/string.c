/* The string and memory functions.
 *
 * Written out rather than left to the compiler: gcc turns a struct copy or a
 * zeroed array into a call to `memcpy` or `memset` whether or not the source
 * mentions one, so a freestanding program that never calls them still needs
 * them to exist.
 */

#include <string.h>

size_t strlen(const char *s) {
    const char *p = s;
    while (*p) {
        p++;
    }
    return (size_t)(p - s);
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return (int)(unsigned char)*a - (int)(unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    for (size_t i = 0; i < n; i++) {
        unsigned char ca = (unsigned char)a[i], cb = (unsigned char)b[i];
        if (ca != cb) {
            return (int)ca - (int)cb;
        }
        if (ca == '\0') {
            break;
        }
    }
    return 0;
}

char *strcpy(char *d, const char *s) {
    char *out = d;
    while ((*d++ = *s++) != '\0') {
    }
    return out;
}

char *strncpy(char *d, const char *s, size_t n) {
    size_t i = 0;
    for (; i < n && s[i]; i++) {
        d[i] = s[i];
    }
    /* The standard says pad to n, not merely terminate. Callers rely on it. */
    for (; i < n; i++) {
        d[i] = '\0';
    }
    return d;
}

char *strchr(const char *s, int c) {
    char want = (char)c;
    for (;; s++) {
        if (*s == want) {
            return (char *)s;
        }
        if (*s == '\0') {
            return 0;
        }
    }
}

char *strrchr(const char *s, int c) {
    char want = (char)c;
    const char *found = 0;
    for (;; s++) {
        if (*s == want) {
            found = s;
        }
        if (*s == '\0') {
            return (char *)found;
        }
    }
}

void *memset(void *d, int c, size_t n) {
    unsigned char *p = d;
    while (n--) {
        *p++ = (unsigned char)c;
    }
    return d;
}

void *memcpy(void *d, const void *s, size_t n) {
    unsigned char *dst = d;
    const unsigned char *src = s;
    while (n--) {
        *dst++ = *src++;
    }
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    unsigned char *dst = d;
    const unsigned char *src = s;
    /* Copy backwards when the regions overlap the wrong way, or the tail of
       the source is overwritten before it is read. */
    if (dst > src && dst < src + n) {
        for (size_t i = n; i > 0; i--) {
            dst[i - 1] = src[i - 1];
        }
        return d;
    }
    while (n--) {
        *dst++ = *src++;
    }
    return d;
}

int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *x = a, *y = b;
    for (size_t i = 0; i < n; i++) {
        if (x[i] != y[i]) {
            return (int)x[i] - (int)y[i];
        }
    }
    return 0;
}
