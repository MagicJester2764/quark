#ifndef _STDLIB_H
#define _STDLIB_H
#include <stddef.h>
void  *malloc(size_t n);
void  *calloc(size_t n, size_t size);
void  *realloc(void *p, size_t n);
void   free(void *p);
void   exit(int status) __attribute__((noreturn));
void   abort(void) __attribute__((noreturn));
int    atoi(const char *s);
long   strtol(const char *s, char **end, int base);
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1

/* The environment. The strings come from the page a spawner maps and are never
   written through; setenv allocates rather than editing in place. */
extern char **environ;
char *getenv(const char *name);
int setenv(const char *name, const char *value, int overwrite);
int unsetenv(const char *name);

#endif
