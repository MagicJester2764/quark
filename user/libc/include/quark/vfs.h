/* The VFS protocol, in C.
 *
 * A file on Quark is a handle held by a server, not an object the kernel
 * knows about, so `open` is a message and `read` moves bytes through a page
 * the client owns and the server maps. That is the same protocol whichever C
 * library is on top of it — ours, or musl through the Linux translation
 * layer — so it is written down once here rather than once per library. Two
 * copies of a wire format drift.
 *
 * Nothing in here needs a C library: only Quark's own system calls.
 */
#ifndef _QUARK_VFS_H
#define _QUARK_VFS_H

#include <quark/syscall.h>

#define QUARK_VFS_TAG_OPEN    1
#define QUARK_VFS_TAG_READ    2
#define QUARK_VFS_TAG_CLOSE   3
#define QUARK_VFS_TAG_READDIR 4
#define QUARK_VFS_TAG_STAT    5
#define QUARK_VFS_TAG_WRITE   6
#define QUARK_VFS_TAG_CREATE  7

/* What the server reports. Its own small integers, not anybody's errno —
   each library maps them to whatever it calls those conditions. */
#define QUARK_VFS_NOT_FOUND      1
#define QUARK_VFS_INVALID_HANDLE 2
#define QUARK_VFS_IO             3
#define QUARK_VFS_TOO_MANY_OPEN  4
#define QUARK_VFS_INVALID_PATH   5
#define QUARK_VFS_NOT_DIR        6
#define QUARK_VFS_IS_DIR         7
#define QUARK_VFS_PERMISSION     8
#define QUARK_VFS_READ_ONLY      9

/* The server has no separate protocol for it, so a path travels in the six
   data words of one message, with room for a terminator. */
#define QUARK_VFS_MAX_PATH 47

/* Returned when the call itself could not be made — no VFS, or the message
   did not go. Distinct from every code the server reports. */
#define QUARK_VFS_UNREACHABLE 255

/* `access` is what *this* caller may do with the file — the rwx bits
   `access(2)` asks about, 4 read, 2 write, 1 execute — answered by the server
   rather than worked out here. It depends on the file's owner and on who is
   asking, and the server is the only party that knows both; a client deriving
   it from `mode` would be keeping a second copy of the permission policy. */
#define QUARK_VFS_R_OK 4
#define QUARK_VFS_W_OK 2
#define QUARK_VFS_X_OK 1

struct quark_vfs_file {
    unsigned long handle;
    unsigned long size;
    int is_dir;
    unsigned int mode;   /* permission bits, without the file type */
    unsigned int access; /* QUARK_VFS_{R,W,X}_OK, for the caller */
};

/* Every one of these returns 0, or a positive error code from the list
   above. Counts come back through the out-parameter, so that a short read is
   never confused with a small error number. */
int quark_vfs_open(const char *path, int create, struct quark_vfs_file *out);
int quark_vfs_stat(unsigned long handle, struct quark_vfs_file *out);
int quark_vfs_read(unsigned long handle, unsigned long phys, unsigned long offset,
                   unsigned long len, unsigned long *got);
int quark_vfs_write(unsigned long handle, unsigned long phys, unsigned long offset,
                    unsigned long len, unsigned long *put);
int quark_vfs_close(unsigned long handle);

#endif
