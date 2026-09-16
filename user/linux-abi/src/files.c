/* Files, for a libc that thinks it is talking to Linux.
 *
 * A file on Quark is a handle held by the VFS, not an object the kernel knows
 * about, and its bytes and paths travel in buffers lent to the server with
 * each call. Linux's file calls are descriptors, offsets and a `struct stat`.
 * This is where one becomes the other.
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

/* Where this layer's own descriptors live.
 *
 * There are two descriptor allocators and one number space. The kernel hands
 * out 3 upwards for the objects it knows about — streams, memory, sets — and
 * this layer hands out numbers for VFS files, which the kernel knows nothing
 * about. Both starting at 3 meant a program that called `socketpair` and then
 * `open` had two different things called descriptor 4, and neither could see
 * the other's.
 *
 * So this layer takes the numbers above the kernel's table and the kernel
 * keeps the ones inside it. The split has to sit exactly at the kernel's
 * MAX_FDS: any lower and a descriptor the kernel installs on our behalf --
 * `recvmsg` asks it to choose one -- can land on a number this layer has
 * already given to an open file, and the two are invisible to each other. */
#define FIRST_FD  32 /* == the kernel's MAX_FDS */
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

/* Linux's open flags, which are what musl passes. */
#define LX_O_ACCMODE   3
#define LX_O_WRONLY    1
#define LX_O_RDWR      2
#define LX_O_CREAT     0100
#define LX_O_EXCL      0200
#define LX_O_TRUNC     01000
#define LX_O_APPEND    02000
#define LX_O_DIRECTORY 0200000

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
    case QUARK_VFS_EXISTS:         return -LX_EEXIST;
    case QUARK_VFS_NOT_EMPTY:      return -LX_ENOTEMPTY;
    case QUARK_VFS_NOT_SUPPORTED:  return -LX_EOPNOTSUPP;
    case QUARK_VFS_NAME_TOO_LONG:  return -LX_ENAMETOOLONG;
    default:                       return -LX_EIO;
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

    unsigned long how = 0;
    if (flags & LX_O_CREAT) {
        how |= QUARK_VFS_OPEN_CREATE;
        if (flags & LX_O_EXCL) {
            how |= QUARK_VFS_OPEN_EXCLUSIVE;
        }
    }
    if (flags & LX_O_DIRECTORY) {
        how |= QUARK_VFS_OPEN_DIRECTORY;
    }
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, how, &info);
    if (err) {
        return vfs_errno(err);
    }
    /* Asked of the server rather than worked out here: whether this caller may
       write it, and whether it is something that can be written at all. */
    if ((flags & LX_O_ACCMODE) != 0) {
        if (info.is_dir || !(info.access & QUARK_VFS_W_OK)) {
            quark_vfs_close(info.handle);
            return info.is_dir ? -LX_EISDIR : -LX_EACCES;
        }
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
        /* 0, 1 and 2 belong to whoever spawned this task, and go when the task
           does. A program closing them is finished with them, which is the same
           outcome — and reporting EBADF instead makes every tool that tidies up
           after itself print an error it cannot act on. */
        if (fd >= 0 && fd < 3) {
            return 0;
        }
        /* Everything else below FIRST_FD is the kernel's: a stream end, a pipe
           end, memory, a descriptor set. These used to be answered the same way
           — success, and nothing done — which is wrong in a way that only shows
           up somewhere else. A pipe ends when its last writer closes, so a
           `close` that does not happen is a reader that waits for ever; the
           clipboard was the first thing here whose correctness depended on
           another task observing a close. */
        if (fd >= 0 && fd < FIRST_FD) {
            return __syscall1(SYS_FD_CLOSE, (unsigned long)fd) == QUARK_ERR
                       ? -LX_EBADF
                       : 0;
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

    unsigned long done = 0;
    unsigned char *out = buf;
    /* One page per message, so anything larger is a loop. */
    while (done < n) {
        unsigned long want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        unsigned long got = 0;
        /* The caller's own buffer, lent to the VFS to fill. */
        int e = quark_vfs_read(f->handle, out + done, f->offset, want, &got);
        if (e) {
            return done ? (long)done : vfs_errno(e);
        }
        if (got == 0) {
            break; /* end of file */
        }
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

    unsigned long done = 0;
    const unsigned char *in = buf;
    while (done < n) {
        unsigned long want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        unsigned long put = 0;
        int e = quark_vfs_write(f->handle, in + done, f->offset, want, &put);
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

static void fill_stat(struct lx_kstat *st, const struct quark_vfs_stat *r) {
    bytes_zero(st, sizeof *st);
    st->st_dev = 1;
    st->st_ino = r->id;
    st->st_nlink = r->links;
    st->st_mode = (unsigned int)r->mode;
    st->st_uid = (unsigned int)r->uid;
    st->st_gid = (unsigned int)r->gid;
    st->st_size = (long)r->size;
    st->st_blksize = (long)r->blksize;
    st->st_blocks = (long)r->blocks;
    st->st_atime_sec = (long)r->atime;
    st->st_mtime_sec = (long)r->mtime;
    st->st_ctime_sec = (long)r->ctime;
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
    /* Asked, not remembered: another descriptor, or another program, may
       have changed the file since this one was opened. */
    struct quark_vfs_stat r;
    int err = quark_vfs_stat(f->handle, &r);
    if (err) {
        return vfs_errno(err);
    }
    f->size = r.size;
    fill_stat(statbuf, &r);
    return 0;
}

long __quark_stat(const char *path, void *statbuf) {
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    struct quark_vfs_stat r;
    err = quark_vfs_stat(info.handle, &r);
    quark_vfs_close(info.handle);
    if (err) {
        return vfs_errno(err);
    }
    fill_stat(statbuf, &r);
    return 0;
}

long __quark_mkdir(const char *path) {
    int err = quark_vfs_mkdir(path);
    return err ? vfs_errno(err) : 0;
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
/* `fcntl`, for the commands a program on this system can actually be answered.
 *
 * It used to return 0 for everything, on the theory that musl only probes it.
 * That is true of musl and false of everything above it: libwayland duplicates
 * a descriptor with F_DUPFD_CLOEXEC before sending it, believed the 0, and sent
 * descriptor zero -- its own standard input -- to the compositor. A stub that
 * answers "fine" to a question it did not understand is worse than one that
 * answers "no", because the caller has no way to find out. */
#define LX_F_DUPFD          0
#define LX_F_GETFD          1
#define LX_F_SETFD          2
#define LX_F_GETFL          3
#define LX_F_SETFL          4
#define LX_F_DUPFD_CLOEXEC  1030

/* Which descriptors a program has asked to be non-blocking. One bit per
   kernel descriptor; this layer's own file numbers are always blocking,
   because the VFS is a synchronous call and there is nothing to wait for. */
static unsigned int nonblock_mask;

int __quark_fd_is_nonblock(long fd) {
    return fd >= 0 && fd < FIRST_FD && (nonblock_mask & (1u << fd)) != 0;
}

long __quark_fcntl(long fd, long cmd, long arg) {
    switch (cmd) {
    case LX_F_DUPFD:
    case LX_F_DUPFD_CLOEXEC: {
        /* Close-on-exec is not a distinction here: nothing execs, so a
           duplicate is a duplicate. */
        if (fd < 0 || fd >= FIRST_FD) {
            /* A VFS file has a handle this layer holds one reference to, and
               a second descriptor for it would need that refcounted -- which
               the VFS protocol does not offer. Saying so beats inventing an
               alias whose close destroys the original. */
            return -LX_ENOSYS;
        }
        unsigned long r = __syscall4(SYS_FD_DUP, __syscall0(SYS_GETPID),
                                     QUARK_ANY_FD, (unsigned long)fd,
                                     arg < 0 ? 0 : (unsigned long)arg);
        return r == QUARK_ERR ? -LX_EMFILE : (long)r;
    }
    case LX_F_GETFD:
    case LX_F_SETFD:
        /* FD_CLOEXEC and nothing else. There is no exec, so every descriptor
           behaves as though the bit were clear, and setting it changes
           nothing that can be observed. */
        return 0;
    case LX_F_GETFL:
        return LX_O_RDWR | (__quark_fd_is_nonblock(fd) ? LX_O_NONBLOCK : 0);
    case LX_F_SETFL:
        if (fd < 0 || fd >= FIRST_FD) {
            return (arg & LX_O_NONBLOCK) ? -LX_EINVAL : 0;
        }
        if (arg & LX_O_NONBLOCK) {
            nonblock_mask |= 1u << fd;
        } else {
            nonblock_mask &= ~(1u << fd);
        }
        return 0;
    default:
        return -LX_EINVAL;
    }
}

long __quark_openat(long dirfd, const char *path, long flags) {
    if (dirfd != LX_AT_FDCWD) {
        return -LX_ENOSYS;
    }
    return __quark_open(path, flags);
}
