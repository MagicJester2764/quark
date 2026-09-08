#ifndef _FCNTL_H
#define _FCNTL_H
/* Only the modes the VFS distinguishes. It has no notion of append or of
   opening a directory for writing, so neither has this. */
#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR   2
#define O_CREAT  0100
#define O_TRUNC  01000
int open(const char *path, int flags, ...);
#endif
