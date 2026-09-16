#ifndef _FCNTL_H
#define _FCNTL_H
/* The flags the VFS acts on, with Linux's values so a program ported from
   there means what it says. It has no notion of append, so neither has this. */
#define O_RDONLY    0
#define O_WRONLY    1
#define O_RDWR      2
#define O_ACCMODE   3
#define O_CREAT     0100
#define O_EXCL      0200
#define O_TRUNC     01000
#define O_DIRECTORY 0200000
int open(const char *path, int flags, ...);
#endif
