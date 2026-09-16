/* The VFS protocol, in C. docs/vfs.md is the contract.
 *
 * A file on Quark is a handle held by a server, not an object the kernel
 * knows about, so `open` is a message, and a path or the bytes of a read
 * travel in a buffer lent with it. That is the same protocol whichever C
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
#define QUARK_VFS_TAG_STAT    5
#define QUARK_VFS_TAG_WRITE   6
#define QUARK_VFS_TAG_MKDIR   9
#define QUARK_VFS_TAG_UNLINK  10
#define QUARK_VFS_TAG_RMDIR   11
#define QUARK_VFS_TAG_RENAME  12
#define QUARK_VFS_TAG_TRUNCATE 13
#define QUARK_VFS_TAG_READDIR  8   /* the bulk read; 4 is retired */
#define QUARK_VFS_TAG_STATFS  14

/* A directory record, as QUARK_VFS_TAG_READDIR fills a buffer with them:
   `id`, `next` and `size` (8 bytes each), `reclen` (2), `type` (1) and
   `namelen` (1), then the name and a NUL, padded to 8. `type` uses Linux's
   DT_ values. */
#define QUARK_VFS_DIRENT_HEADER 28

/* What `quark_vfs_open` may be asked to do besides open. */
#define QUARK_VFS_OPEN_CREATE    1UL  /* make the file if the name is free */
#define QUARK_VFS_OPEN_EXCLUSIVE 2UL  /* with CREATE: the name must be free */
#define QUARK_VFS_OPEN_TRUNCATE  4UL  /* empty a regular file */
#define QUARK_VFS_OPEN_DIRECTORY 8UL  /* it must be a directory */

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
#define QUARK_VFS_EXISTS        10
#define QUARK_VFS_NOT_EMPTY     11
#define QUARK_VFS_NOT_SUPPORTED 12
#define QUARK_VFS_NAME_TOO_LONG 13

/* A path is lent with the call that names it. One longer than this is
   refused, never shortened. */
#define QUARK_VFS_MAX_PATH 4095

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
    unsigned int mode;   /* with the file-type bits */
    unsigned int access; /* QUARK_VFS_{R,W,X}_OK, for the caller */
    unsigned long id;    /* the inode number, stable while the file exists */
};

/* What STAT fills in: eleven words, in this order. Times are seconds since
   boot, the scale time() counts on here. */
struct quark_vfs_stat {
    unsigned long id;
    unsigned long size;
    unsigned long mode;
    unsigned long links;
    unsigned long uid;
    unsigned long gid;
    unsigned long atime;
    unsigned long mtime;
    unsigned long ctime;
    unsigned long blocks;   /* 512-byte units */
    unsigned long blksize;
};

/* Every one of these returns 0, or a positive error code from the list
   above. Counts come back through the out-parameter, so that a short read is
   never confused with a small error number. */
int quark_vfs_open(const char *path, unsigned long flags, struct quark_vfs_file *out);
int quark_vfs_stat(unsigned long handle, struct quark_vfs_stat *out);
int quark_vfs_mkdir(const char *path);
int quark_vfs_unlink(const char *path);
int quark_vfs_rmdir(const char *path);
int quark_vfs_rename(const char *from, const char *to);
int quark_vfs_truncate(unsigned long handle, unsigned long size);

/* Fill `buf` (at most a page) with the records of a directory from entry
   `start`. `*next` is where the next call should start, `*end` says there is
   nothing more. A buffer too small for even one record comes back empty with
   `*end` clear. */
int quark_vfs_readdir(unsigned long handle, unsigned long start, void *buf, unsigned long len,
                      unsigned long *used, unsigned long *next, int *end);

/* What the mounted filesystem is and how full: eight words, in this order. */
struct quark_vfs_statfs {
    unsigned long magic;   /* 0xEF53 for ext2/ext4, 0x4d44 for FAT */
    unsigned long bsize;
    unsigned long blocks;
    unsigned long bfree;
    unsigned long bavail;
    unsigned long files;
    unsigned long ffree;
    unsigned long namemax;
};
int quark_vfs_statfs(struct quark_vfs_statfs *out);
/* A read or write carries at most QUARK_VFS_MAX_IO bytes, which the VFS is
   lent for the call: it fills `buf` or copies out of it, and never maps it. */
#define QUARK_VFS_MAX_IO 4096UL
int quark_vfs_read(unsigned long handle, void *buf, unsigned long offset,
                   unsigned long len, unsigned long *got);
int quark_vfs_write(unsigned long handle, const void *buf, unsigned long offset,
                    unsigned long len, unsigned long *put);
int quark_vfs_close(unsigned long handle);

#endif
