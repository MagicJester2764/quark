#ifndef _STDIO_H
#define _STDIO_H
#include <stddef.h>
#include <stdarg.h>
#include <sys/types.h>

#define EOF (-1)

/* A stream, which here is a descriptor and the two flags a caller can ask
 * about. Unbuffered: every write is a write. On a microkernel that is an IPC
 * round trip rather than a system call, so buffering would be worth having —
 * but an unbuffered stream is one whose output is never lost because nobody
 * flushed it, and that is the better default to start from.
 */
typedef struct _IO_FILE FILE;

extern FILE *stdin;
extern FILE *stdout;
extern FILE *stderr;

int    printf(const char *fmt, ...);
int    fprintf(FILE *f, const char *fmt, ...);
int    vfprintf(FILE *f, const char *fmt, va_list ap);
int    vprintf(const char *fmt, va_list ap);
int    vsnprintf(char *buf, size_t size, const char *fmt, va_list ap);
int    snprintf(char *buf, size_t size, const char *fmt, ...);
int    sprintf(char *buf, const char *fmt, ...);

int    puts(const char *s);
int    putchar(int c);
int    fputs(const char *s, FILE *f);
int    fputc(int c, FILE *f);
#define putc(c, f) fputc((c), (f))
int    fgetc(FILE *f);
#define getc(f) fgetc(f)
int    getchar(void);
char  *fgets(char *s, int size, FILE *f);

FILE  *fopen(const char *path, const char *mode);
int    fclose(FILE *f);
size_t fread(void *ptr, size_t size, size_t n, FILE *f);
size_t fwrite(const void *ptr, size_t size, size_t n, FILE *f);
int    fflush(FILE *f);
int    feof(FILE *f);
int    ferror(FILE *f);
void   clearerr(FILE *f);
int    fileno(FILE *f);

/* Whole-file reads, which is what a program on Quark actually wants: the VFS
   hands over a page at a time and there is no seek. */
long readfile(const char *path, char *buf, long size);
#endif
