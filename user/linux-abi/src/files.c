/* Files, for a libc that thinks it is talking to Linux.
 *
 * A file on Quark is a handle held by the VFS, not an object the kernel knows
 * about, and its bytes move through a page this program owns and the server
 * maps. Linux's file calls are descriptors, offsets and a `struct stat`. This
 * is where one becomes the other.
 *
 * Descriptors 0, 1 and 2 are the kernel's — a spawner wired them to services,
 * and reading or writing one really is a system call. Everything above them is
 * this file's own bookkeeping, exactly as it is in Quark's own C library: the
 * numbering is a fiction each library maintains, because the VFS hands out
 * handles and has no opinion about which small integer you call them.
 */

#include <quark/layout.h>
#include <quark/syscall.h>
#include <quark/vfs.h>

#include "abi.h"

#define PAGE_SIZE 4096UL

/* The page file data moves through. One is enough: a read is a round trip, so
   there is never a second one in flight. */
#define XFER_VADDR QUARK_XFER_PAGE

#define FIRST_FD  3
#define MAX_FILES 16

struct openfile {
    int used;
    unsigned long handle; /* what the VFS calls it */
    unsigned long offset; /* the VFS has no seek, so the position is ours */
    unsigned long size;
    int is_dir;
    unsigned int mode;    /* permission bits, as the server reports them */
};

static struct openfile files[MAX_FILES];
static unsigned long xfer_phys;
static int xfer_ready;

/* Linux's open flags, which are what musl passes. */
#define LX_O_WRONLY 1
#define LX_O_RDWR   2
#define LX_O_CREAT  0100
#define LX_O_TRUNC  01000
#define LX_O_APPEND 02000

#define LX_SEEK_SET 0
#define LX_SEEK_CUR 1
#define LX_SEEK_END 2

/* File type bits, so that a caller can tell a directory from a file. */
#define LX_S_IFDIR  0040000
#define LX_S_IFREG  0100000

/* The server's codes are its own. This is where they become Linux's. */
static long vfs_errno(int code) {
    switch (code) {
    case QUARK_VFS_NOT_FOUND:      return -LX_ENOENT;
    case QUARK_VFS_INVALID_HANDLE: return -LX_EBADF;
    case QUARK_VFS_IO:             return -LX_EIO;
    case QUARK_VFS_TOO_MANY_OPEN:  return -LX_EMFILE;
    case QUARK_VFS_INVALID_PATH:   return -LX_EINVAL;
    case QUARK_VFS_NOT_DIR:        return -LX_ENOTDIR;
    case QUARK_VFS_IS_DIR:         return -LX_EISDIR;
    case QUARK_VFS_PERMISSION:     return -LX_EACCES;
    case QUARK_VFS_READ_ONLY:      return -LX_EROFS;
    case QUARK_VFS_UNREACHABLE:    return -LX_EIO;
    default:                       return -LX_EIO;
    }
}

/* Map the page the VFS reads from and writes into.
 *
 * Done on first use rather than at startup: a program that never opens a file
 * should not need the capability to allocate a page, and asking for one it
 * does not have would fail at a moment that has nothing to do with files. */
static long xfer_page(void) {
    if (xfer_ready) {
        return 0;
    }
    unsigned long phys = __syscall1(SYS_PHYS_ALLOC, 1);
    if (phys == QUARK_ERR) {
        return -LX_ENOMEM;
    }
    if (__syscall3(SYS_MAP_PHYS, phys, XFER_VADDR, 1) == QUARK_ERR) {
        return -LX_ENOMEM;
    }
    xfer_phys = phys;
    xfer_ready = 1;
    return 0;
}

static void bytes_copy(void *dst, const void *src, unsigned long n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--) {
        *d++ = *s++;
    }
}

static void bytes_zero(void *p, unsigned long n) {
    unsigned char *b = p;
    while (n--) {
        *b++ = 0;
    }
}

static struct openfile *slot(long fd) {
    if (fd < FIRST_FD || fd >= FIRST_FD + MAX_FILES) {
        return 0;
    }
    struct openfile *f = &files[fd - FIRST_FD];
    return f->used ? f : 0;
}

long __quark_open(const char *path, long flags) {
    if (!path || !*path) {
        return -LX_ENOENT;
    }
    long fd = -1;
    for (int i = 0; i < MAX_FILES; i++) {
        if (!files[i].used) {
            fd = FIRST_FD + i;
            break;
        }
    }
    if (fd < 0) {
        return -LX_EMFILE;
    }

    struct quark_vfs_file info;
    int err = quark_vfs_open(path, (flags & LX_O_CREAT) != 0, &info);
    if (err) {
        return vfs_errno(err);
    }

    struct openfile *f = &files[fd - FIRST_FD];
    f->used = 1;
    f->handle = info.handle;
    f->size = info.size;
    f->is_dir = info.is_dir;
    f->mode = info.mode;
    /* Appending starts at the end; everything else starts at the beginning.
       There is no O_TRUNC here because the VFS has no truncate — a caller
       asking for one gets a file it can overwrite but not shorten, which is
       worth knowing about rather than pretending away. */
    f->offset = (flags & LX_O_APPEND) ? info.size : 0;
    return fd;
}

long __quark_close(long fd) {
    struct openfile *f = slot(fd);
    if (!f) {
        /* 0, 1 and 2 belong to whoever spawned this task, and go when the
           task does. A program closing them is finished with them, which is
           the same outcome — and reporting EBADF instead makes every tool
           that tidies up after itself print an error it cannot act on. */
        if (fd >= 0 && fd < FIRST_FD) {
            return 0;
        }
        return -LX_EBADF;
    }
    quark_vfs_close(f->handle);
    f->used = 0;
    return 0;
}

long __quark_file_read(long fd, void *buf, unsigned long n) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    if (f->is_dir) {
        return -LX_EISDIR;
    }
    if (n == 0) {
        return 0;
    }
    long err = xfer_page();
    if (err) {
        return err;
    }

    unsigned long done = 0;
    unsigned char *out = buf;
    /* One page per message, so anything larger is a loop. */
    while (done < n) {
        unsigned long want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        unsigned long got = 0;
        int e = quark_vfs_read(f->handle, xfer_phys, f->offset, want, &got);
        if (e) {
            return done ? (long)done : vfs_errno(e);
        }
        if (got == 0) {
            break; /* end of file */
        }
        bytes_copy(out + done, (const void *)XFER_VADDR, got);
        done += got;
        f->offset += got;
        /* A short read means the end, not a hiccup: the server answers from a
           page at a time and gives everything it has. */
        if (got < want) {
            break;
        }
    }
    return (long)done;
}

long __quark_file_write(long fd, const void *buf, unsigned long n) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    if (n == 0) {
        return 0;
    }
    long err = xfer_page();
    if (err) {
        return err;
    }

    unsigned long done = 0;
    const unsigned char *in = buf;
    while (done < n) {
        unsigned long want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        bytes_copy((void *)XFER_VADDR, in + done, want);
        unsigned long put = 0;
        int e = quark_vfs_write(f->handle, xfer_phys, f->offset, want, &put);
        if (e) {
            return done ? (long)done : vfs_errno(e);
        }
        done += put;
        f->offset += put;
        if (f->offset > f->size) {
            f->size = f->offset;
        }
        if (put < want) {
            break;
        }
    }
    return (long)done;
}

long __quark_lseek(long fd, long offset, long whence) {
    struct openfile *f = slot(fd);
    if (!f) {
        /* A standard descriptor is a pipe or a service, and seeking one is
           the error Linux calls ESPIPE — which is what stdio checks for when
           it decides whether a stream is seekable. */
        if (fd >= 0 && fd < FIRST_FD) {
            return -LX_ESPIPE;
        }
        return -LX_EBADF;
    }
    long base;
    switch (whence) {
    case LX_SEEK_SET: base = 0; break;
    case LX_SEEK_CUR: base = (long)f->offset; break;
    case LX_SEEK_END: base = (long)f->size; break;
    default: return -LX_EINVAL;
    }
    long to = base + offset;
    if (to < 0) {
        return -LX_EINVAL;
    }
    f->offset = (unsigned long)to;
    return to;
}

/* The kernel's `struct stat` for x86-64, which is what musl copies out of.
   Its layout is the ABI, not musl's choice, so it is spelled out here. */
struct lx_kstat {
    unsigned long st_dev;
    unsigned long st_ino;
    unsigned long st_nlink;
    unsigned int  st_mode;
    unsigned int  st_uid;
    unsigned int  st_gid;
    unsigned int  __pad0;
    unsigned long st_rdev;
    long          st_size;
    long          st_blksize;
    long          st_blocks;
    long          st_atime_sec, st_atime_nsec;
    long          st_mtime_sec, st_mtime_nsec;
    long          st_ctime_sec, st_ctime_nsec;
    long          __unused[3];
};

static void fill_stat(struct lx_kstat *st, unsigned long size, int is_dir,
                      unsigned long ino, unsigned int mode) {
    bytes_zero(st, sizeof *st);
    st->st_dev = 1;
    st->st_ino = ino;
    st->st_nlink = 1;
    st->st_mode = (is_dir ? LX_S_IFDIR : LX_S_IFREG) | (mode & 07777);
    st->st_size = (long)size;
    st->st_blksize = (long)PAGE_SIZE;
    st->st_blocks = (long)((size + 511) / 512);
    return;
}

long __quark_fstat(long fd, void *statbuf) {
    struct openfile *f = slot(fd);
    if (!f) {
        /* 0, 1 and 2 are not files. Reporting them as a character device is
           what makes stdio treat them as one and pick line buffering. */
        if (fd >= 0 && fd < FIRST_FD) {
            struct lx_kstat *st = statbuf;
            bytes_zero(st, sizeof *st);
            st->st_dev = 1;
            st->st_ino = (unsigned long)fd;
            st->st_nlink = 1;
            st->st_mode = 020000 | 0666; /* S_IFCHR */
            st->st_blksize = (long)PAGE_SIZE;
            return 0;
        }
        return -LX_EBADF;
    }
    fill_stat(statbuf, f->size, f->is_dir, f->handle, f->mode);
    return 0;
}

long __quark_stat(const char *path, void *statbuf) {
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    fill_stat(statbuf, info.size, info.is_dir, info.handle, info.mode);
    quark_vfs_close(info.handle);
    return 0;
}

/* access(2), and the *at forms of it that gnulib reaches for first.
 *
 * The question is "may I", and the only way to ask it is to open the file:
 * that runs the server's own permission check, and the reply says what this
 * caller may do with what it found. Working it out from the mode bits here
 * would need the file's owner as well, and would be this program's opinion
 * about a policy the VFS enforces — which is the one that decides.
 *
 * The one thing to know is that Quark checks nothing on execute: init and the
 * shell load a program by reading it. So a file the server calls executable is
 * one a program is allowed to try, which is what a caller asks X_OK for. */
long __quark_access(const char *path, long mode) {
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    unsigned int have = info.access;
    quark_vfs_close(info.handle);

    /* F_OK is 0 — asking only whether it is there, which the open answered. */
    unsigned int want = (unsigned int)mode & 7;
    return (want & ~have) ? -LX_EACCES : 0;
}

/* Reads and writes on 0, 1 and 2 go to the kernel; anything else is a file. */
long __quark_read(long fd, void *buf, unsigned long n) {
    if (fd >= 0 && fd < FIRST_FD) {
        unsigned long r = __syscall3(SYS_FD_READ, (unsigned long)fd,
                                     (unsigned long)buf, n);
        return r == QUARK_ERR ? -LX_EBADF : (long)r;
    }
    return __quark_file_read(fd, buf, n);
}

long __quark_write(long fd, const void *buf, unsigned long n) {
    if (fd >= 0 && fd < FIRST_FD) {
        unsigned long r = __syscall3(SYS_FD_WRITE, (unsigned long)fd,
                                     (unsigned long)buf, n);
        return r == QUARK_ERR ? -LX_EBADF : (long)r;
    }
    return __quark_file_write(fd, buf, n);
}

/* Which of the two `open` numbers this came in on does not matter, except
   that `openat` may name a directory to resolve against. There is no working
   directory here — no server keeps one, and inventing one in this layer would
   make every program disagree with every other — so only AT_FDCWD is
   accepted, and paths are what the VFS is given. */
long __quark_openat(long dirfd, const char *path, long flags) {
    if (dirfd != LX_AT_FDCWD) {
        return -LX_ENOSYS;
    }
    return __quark_open(path, flags);
}
