#ifndef _ERRNO_H
#define _ERRNO_H
/* The VFS reports small integers of its own; these are the C names for the
   ones it uses, not Linux's numbering. */
#define EPERM   1
#define ENOENT  2
#define EIO     5
#define EBADF   9
#define ENOMEM 12
#define EACCES 13
#define EEXIST 17
#define ENOTDIR 20
#define EISDIR  21
#define EINVAL 22
#define EMFILE 24
#define ENOSPC 28
#define EROFS  30
extern int errno;
#endif
