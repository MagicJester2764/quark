#ifndef _STDIO_H
#define _STDIO_H
#include <stddef.h>
#include <stdarg.h>

#define EOF (-1)

int printf(const char *fmt, ...);
int fprintf(int fd, const char *fmt, ...);
int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);
int snprintf(char *buf, size_t size, const char *fmt, ...);
int puts(const char *s);
int putchar(int c);

/* Whole-file reads, which is what a program on Quark actually wants: the VFS
   hands over a page at a time and there is no seek. */
long readfile(const char *path, char *buf, long size);
#endif
