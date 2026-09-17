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
#define FIRST_FD  LX_FIRST_FILE_FD
#define MAX_FILES 16

/* An open file, as `open` made it. A descriptor names one, and `dup` gives it
 * another name: the two share its position, as they do on Linux, and the VFS
 * handle is closed with the last of them. The server never hears about the
 * copies. A handle belongs to this process, so this is the only place that
 * needs to count them. */
struct openfile {
    int refs;             /* descriptors naming it; 0 is a free entry */
    unsigned long handle; /* what the VFS calls it */
    unsigned long offset; /* the VFS has no seek, so the position is ours */
    unsigned long size;
    int is_dir;
    unsigned int mode;    /* permission bits, as the server reports them */
    unsigned long dir_next; /* a directory's position: the next entry's index */
    int dir_end;            /* and whether the last read reached its end */
};

static struct openfile files[MAX_FILES];

/* Which open file each of this layer's descriptors names: an index into
   `files` plus one, so that zero is a free descriptor. */
static unsigned char fdmap[MAX_FILES];

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
    unsigned idx = fdmap[fd - FIRST_FD];
    return idx ? &files[idx - 1] : 0;
}

/* The lowest free descriptor at or above `lowest`, or -1. */
static long free_fd(long lowest) {
    for (long fd = lowest < FIRST_FD ? FIRST_FD : lowest; fd < FIRST_FD + MAX_FILES; fd++) {
        if (!fdmap[fd - FIRST_FD]) {
            return fd;
        }
    }
    return -1;
}

long __quark_open(const char *path, long flags) {
    if (!path || !*path) {
        return -LX_ENOENT;
    }
    /* Every open file has a descriptor, so a free descriptor means a free
       entry too; both are looked for anyway. */
    long fd = free_fd(FIRST_FD);
    int k = -1;
    for (int i = 0; i < MAX_FILES; i++) {
        if (!files[i].refs) {
            k = i;
            break;
        }
    }
    if (fd < 0 || k < 0) {
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
    if ((flags & LX_O_TRUNC) && (flags & LX_O_ACCMODE) != 0) {
        how |= QUARK_VFS_OPEN_TRUNCATE;
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

    struct openfile *f = &files[k];
    f->refs = 1;
    f->handle = info.handle;
    f->size = info.size;
    f->is_dir = info.is_dir;
    f->mode = info.mode;
    /* Appending starts at the end; everything else starts at the beginning,
       and O_TRUNC was the server's to do. */
    f->offset = (flags & LX_O_APPEND) ? info.size : 0;
    f->dir_next = 0;
    f->dir_end = 0;
    fdmap[fd - FIRST_FD] = (unsigned char)(k + 1);
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
    fdmap[fd - FIRST_FD] = 0;
    if (--f->refs == 0) {
        quark_vfs_close(f->handle);
    }
    return 0;
}

/* Another descriptor for the open file `fd` names: the lowest free one at or
   above `lowest`, or `exact` if that is not negative, closing whatever it
   named first. */
static long file_dup(long fd, long lowest, long exact) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    long to = exact;
    if (exact >= 0) {
        if (exact == fd) {
            return fd;
        }
        if (exact < FIRST_FD || exact >= FIRST_FD + MAX_FILES) {
            /* The kernel's numbers name the kernel's objects, and a VFS file
               is not one: it cannot be put where a program's stdout is. */
            return -LX_EBADF;
        }
        if (slot(exact)) {
            __quark_close(exact);
        }
    } else {
        to = free_fd(lowest);
        if (to < 0) {
            return -LX_EMFILE;
        }
    }
    f->refs++;
    fdmap[to - FIRST_FD] = (unsigned char)(f - files + 1);
    return to;
}

/* dup, dup2 and dup3. A copy of a kernel descriptor is the kernel's to make,
   and one of a file is this layer's; neither can become the other. */
long __quark_dup(long fd, long to) {
    if (fd >= FIRST_FD) {
        return file_dup(fd, FIRST_FD, to);
    }
    if (fd < 0 || to >= FIRST_FD) {
        return -LX_EBADF;
    }
    unsigned long me = __syscall0(SYS_GETPID);
    unsigned long copy = __syscall4(SYS_FD_DUP, me, QUARK_ANY_FD, (unsigned long)fd, 0);
    if (copy == QUARK_ERR) {
        return -LX_EBADF;
    }
    if (to < 0 || (long)copy == to || fd == to) {
        if (fd == to) {
            __syscall1(SYS_FD_CLOSE, copy);
        }
        return fd == to ? to : (long)copy;
    }
    /* The kernel fills a slot without emptying it, so whatever `to` named is
       closed first -- a pipe end left behind would keep its reader waiting. */
    __syscall1(SYS_FD_CLOSE, (unsigned long)to);
    unsigned long r = __syscall4(SYS_FD_DUP, me, (unsigned long)to, copy, 0);
    __syscall1(SYS_FD_CLOSE, copy);
    return r == QUARK_ERR ? -LX_EBADF : to;
}

/* Read from `*at`, moving it on. `read` passes the descriptor's position and
   `pread` a copy of its argument, which is the whole difference between them. */
static long read_at(struct openfile *f, void *buf, unsigned long n, unsigned long *at) {
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
        int e = quark_vfs_read(f->handle, out + done, *at, want, &got);
        if (e) {
            return done ? (long)done : vfs_errno(e);
        }
        if (got == 0) {
            break; /* end of file */
        }
        done += got;
        *at += got;
        /* A short read means the end, not a hiccup: the server answers from a
           page at a time and gives everything it has. */
        if (got < want) {
            break;
        }
    }
    return (long)done;
}

long __quark_file_read(long fd, void *buf, unsigned long n) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    return read_at(f, buf, n, &f->offset);
}

long __quark_pread(long fd, void *buf, unsigned long n, long offset) {
    struct openfile *f = slot(fd);
    if (!f) {
        return (fd >= 0 && fd < FIRST_FD) ? -LX_ESPIPE : -LX_EBADF;
    }
    if (offset < 0) {
        return -LX_EINVAL;
    }
    unsigned long at = (unsigned long)offset;
    return read_at(f, buf, n, &at);
}

/* Write at `*at`, moving it on; see `read_at`. */
static long write_at(struct openfile *f, const void *buf, unsigned long n, unsigned long *at) {
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
        int e = quark_vfs_write(f->handle, in + done, *at, want, &put);
        if (e) {
            return done ? (long)done : vfs_errno(e);
        }
        done += put;
        *at += put;
        if (*at > f->size) {
            f->size = *at;
        }
        if (put < want) {
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
    return write_at(f, buf, n, &f->offset);
}

long __quark_pwrite(long fd, const void *buf, unsigned long n, long offset) {
    struct openfile *f = slot(fd);
    if (!f) {
        return (fd >= 0 && fd < FIRST_FD) ? -LX_ESPIPE : -LX_EBADF;
    }
    if (offset < 0) {
        return -LX_EINVAL;
    }
    unsigned long at = (unsigned long)offset;
    return write_at(f, buf, n, &at);
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
    /* A directory's position is an entry index: rewinddir seeks to 0, and
       seekdir to a d_off getdents handed out. */
    if (f->is_dir) {
        f->dir_next = (unsigned long)to;
        f->dir_end = 0;
    }
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

static unsigned long rd(const unsigned char *p, int n) {
    unsigned long v = 0;
    for (int i = n - 1; i >= 0; i--) {
        v = (v << 8) | p[i];
    }
    return v;
}

static void wr(unsigned char *p, int n, unsigned long v) {
    for (int i = 0; i < n; i++) {
        p[i] = (unsigned char)(v >> (8 * i));
    }
}

/* getdents64: the server's records, copied field by field into Linux's
   (d_ino, d_off, d_reclen, d_type, d_name). A Linux record is always the
   shorter of the two, so what fits a page of the server's fits the caller's
   buffer of the same size. */
long __quark_getdents(long fd, void *buf, unsigned long count) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    if (!f->is_dir) {
        return -LX_ENOTDIR;
    }
    if (f->dir_end) {
        return 0;
    }
    unsigned char page[4096];
    unsigned long want = count < sizeof page ? count : sizeof page;
    unsigned long used = 0, next = 0;
    int end = 0;
    int e = quark_vfs_readdir(f->handle, f->dir_next, page, want, &used, &next, &end);
    if (e) {
        return vfs_errno(e);
    }
    unsigned char *out = buf;
    unsigned long in = 0, put = 0;
    while (in + QUARK_VFS_DIRENT_HEADER <= used) {
        const unsigned char *r = page + in;
        unsigned long reclen = rd(r + 24, 2);
        unsigned long namelen = r[27];
        unsigned long lreclen = (19 + namelen + 1 + 7) & ~7UL;
        if (reclen < QUARK_VFS_DIRENT_HEADER + namelen || in + reclen > used) {
            return put ? (long)put : -LX_EIO;
        }
        if (put + lreclen > count) {
            break;
        }
        bytes_zero(out + put, lreclen);
        wr(out + put, 8, rd(r, 8));           /* d_ino */
        wr(out + put + 8, 8, rd(r + 8, 8));   /* d_off: where to resume after it */
        wr(out + put + 16, 2, lreclen);       /* d_reclen */
        out[put + 18] = r[26];                /* d_type */
        for (unsigned long i = 0; i < namelen; i++) {
            out[put + 19 + i] = r[QUARK_VFS_DIRENT_HEADER + i];
        }
        f->dir_next = rd(r + 8, 8);
        put += lreclen;
        in += reclen;
    }
    if (put == 0) {
        if (used == 0 && end) {
            f->dir_end = 1;
            return 0;
        }
        return -LX_EINVAL; /* the caller's buffer holds no entry */
    }
    if (in >= used && end) {
        f->dir_end = 1;
    }
    return (long)put;
}

/* readlink. Nothing here makes links, and the server does not follow or read
   them, so the honest answers are "that is not a link" and, for a link made
   elsewhere, "not something this can do". */
long __quark_readlink(const char *path, char *buf, unsigned long size) {
    (void)buf;
    (void)size;
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    quark_vfs_close(info.handle);
    return ((info.mode & 0170000) == 0120000) ? -LX_EOPNOTSUPP : -LX_EINVAL;
}

/* Linux's struct statfs for x86-64: seven words, a two-int fsid, four more
   words and four spare. */
static long fill_statfs(unsigned char *out) {
    struct quark_vfs_statfs fs;
    int err = quark_vfs_statfs(&fs);
    if (err) {
        return vfs_errno(err);
    }
    bytes_zero(out, 120);
    wr(out + 0, 8, fs.magic);
    wr(out + 8, 8, fs.bsize);
    wr(out + 16, 8, fs.blocks);
    wr(out + 24, 8, fs.bfree);
    wr(out + 32, 8, fs.bavail);
    wr(out + 40, 8, fs.files);
    wr(out + 48, 8, fs.ffree);
    wr(out + 64, 8, fs.namemax);  /* f_namelen */
    wr(out + 72, 8, fs.bsize);    /* f_frsize */
    return 0;
}

/* One filesystem is mounted, so the path only has to exist. */
long __quark_statfs(const char *path, void *buf) {
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    quark_vfs_close(info.handle);
    return fill_statfs(buf);
}

long __quark_fstatfs(long fd, void *buf) {
    if (!slot(fd) && (fd < 0 || fd >= FIRST_FD)) {
        return -LX_EBADF;
    }
    return fill_statfs(buf);
}

long __quark_mkdir(const char *path) {
    int err = quark_vfs_mkdir(path);
    return err ? vfs_errno(err) : 0;
}

long __quark_unlink(const char *path) {
    int err = quark_vfs_unlink(path);
    return err ? vfs_errno(err) : 0;
}

long __quark_rmdir(const char *path) {
    int err = quark_vfs_rmdir(path);
    return err ? vfs_errno(err) : 0;
}

long __quark_rename(const char *from, const char *to) {
    int err = quark_vfs_rename(from, to);
    return err ? vfs_errno(err) : 0;
}

/* ftruncate on one of this layer's descriptors. The kernel's own, below
   FIRST_FD, are memory, and net.c answers for those. */
long __quark_file_truncate(long fd, long length) {
    struct openfile *f = slot(fd);
    if (!f) {
        return -LX_EBADF;
    }
    if (length < 0) {
        return -LX_EINVAL;
    }
    int err = quark_vfs_truncate(f->handle, (unsigned long)length);
    if (err) {
        return vfs_errno(err);
    }
    f->size = (unsigned long)length;
    return 0;
}

long __quark_truncate(const char *path, long length) {
    if (length < 0) {
        return -LX_EINVAL;
    }
    struct quark_vfs_file info;
    int err = quark_vfs_open(path, 0, &info);
    if (err) {
        return vfs_errno(err);
    }
    if (info.is_dir) {
        quark_vfs_close(info.handle);
        return -LX_EISDIR;
    }
    err = quark_vfs_truncate(info.handle, (unsigned long)length);
    quark_vfs_close(info.handle);
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
    if (fd >= FIRST_FD && !slot(fd)) {
        return -LX_EBADF;
    }
    switch (cmd) {
    case LX_F_DUPFD:
    case LX_F_DUPFD_CLOEXEC: {
        /* Close-on-exec is not a distinction here: nothing execs, so a
           duplicate is a duplicate. */
        if (fd >= FIRST_FD) {
            return file_dup(fd, arg, -1);
        }
        if (fd < 0) {
            return -LX_EBADF;
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
