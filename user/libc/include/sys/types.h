/* The types a system call surface is described in.
 *
 * Split out of unistd.h because that is where everyone else keeps them, and
 * because libgcc includes this header directly — a hosted target is expected
 * to have it, and a target that does not cannot build its own support library.
 */
#ifndef _SYS_TYPES_H
#define _SYS_TYPES_H

#include <stddef.h>
/* The fixed-width and pointer-sized types come with these on every system
   anyone ports from, and code that includes only this header expects them —
   libgcc's coverage support is one such. */
#include <stdint.h>

typedef long           ssize_t;
typedef int            pid_t;
typedef long           off_t;
typedef unsigned int   mode_t;
typedef unsigned int   uid_t;
typedef unsigned int   gid_t;
typedef unsigned long  ino_t;
typedef unsigned long  dev_t;
typedef unsigned long  nlink_t;
typedef long           blksize_t;
typedef long           blkcnt_t;
typedef long           time_t;
typedef long           suseconds_t;
typedef unsigned long  clock_t;

#endif
