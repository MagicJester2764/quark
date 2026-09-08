#ifndef _UNISTD_H
#define _UNISTD_H
#include <stddef.h>
#include <sys/types.h>

#define STDIN_FILENO  0
#define STDOUT_FILENO 1
#define STDERR_FILENO 2

/* Descriptors 0, 1 and 2 are the kernel's, wired to services by whoever
   spawned us. Anything above them is this library's: a Quark file is a handle
   held by the VFS, not a kernel object, so open() invents a number for it and
   remembers what it stands for. */
ssize_t read(int fd, void *buf, size_t n);
ssize_t write(int fd, const void *buf, size_t n);
int     close(int fd);
pid_t   getpid(void);
unsigned int sleep(unsigned int seconds);
int     usleep(unsigned int usec);
#endif
