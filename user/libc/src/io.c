/* Descriptors, and what they mean here.
 *
 * 0, 1 and 2 are the kernel's: a spawner wired them to the console and the
 * input server, and reading or writing one is a system call. Everything above
 * them is this library's own bookkeeping, because a file on Quark is a handle
 * held by the VFS rather than an object the kernel knows about. `open` asks
 * the VFS for one and remembers which descriptor number stands for it.
 *
 * The VFS moves file data through a page the client owns and the server maps,
 * so this library allocates one at startup and every read and write goes
 * through it. That bounds a transfer to a page, which `read` loops over.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <quark/layout.h>
#include <quark/syscall.h>
#include <quark/vfs.h>

#define PAGE_SIZE 4096
/* The shared transfer page: above everything the loader places, and clear of
   the stack and the heap. */
#define XFER_VADDR QUARK_XFER_PAGE

#define FIRST_FD 3
#define MAX_FILES 16

struct openfile {
    int used;
    unsigned long handle; /* what the VFS calls it */
    unsigned long offset; /* the VFS has no seek, so the position is ours */
    unsigned long size;
};

static struct openfile files[MAX_FILES];
static unsigned long xfer_phys;
static int xfer_ready;

/* Map the page the VFS reads from and writes into. Done on first use rather
   than at startup: a program that never touches a file should not need the
   capability to allocate one. */
static int xfer_page(void) {
    if (xfer_ready) {
        return 0;
    }
    unsigned long phys = __syscall1(SYS_PHYS_ALLOC, 1);
    if (phys == QUARK_ERR) {
        errno = ENOMEM;
        return -1;
    }
    if (__syscall3(SYS_MAP_PHYS, phys, XFER_VADDR, 1) == QUARK_ERR) {
        errno = ENOMEM;
        return -1;
    }
    xfer_phys = phys;
    xfer_ready = 1;
    return 0;
}

/* The server's codes are its own; this is where they become ours. */
static int vfs_errno(int code) {
    switch (code) {
    case QUARK_VFS_NOT_FOUND:      return ENOENT;
    case QUARK_VFS_INVALID_HANDLE: return EBADF;
    case QUARK_VFS_IO:             return EIO;
    case QUARK_VFS_TOO_MANY_OPEN:  return EMFILE;
    case QUARK_VFS_INVALID_PATH:   return EINVAL;
    case QUARK_VFS_NOT_DIR:        return ENOTDIR;
    case QUARK_VFS_IS_DIR:         return EISDIR;
    case QUARK_VFS_PERMISSION:     return EACCES;
    case QUARK_VFS_READ_ONLY:      return EROFS;
    default:                       return EIO;
    }
}

static struct openfile *slot(int fd) {
    if (fd < FIRST_FD || fd >= FIRST_FD + MAX_FILES) {
        return 0;
    }
    struct openfile *f = &files[fd - FIRST_FD];
    return f->used ? f : 0;
}

int open(const char *path, int flags, ...) {
    int fd = -1;
    for (int i = 0; i < MAX_FILES; i++) {
        if (!files[i].used) {
            fd = FIRST_FD + i;
            break;
        }
    }
    if (fd < 0) {
        errno = EMFILE;
        return -1;
    }

    struct quark_vfs_file info;
    int err = quark_vfs_open(path, (flags & O_CREAT) != 0, &info);
    if (err) {
        errno = vfs_errno(err);
        return -1;
    }

    struct openfile *f = &files[fd - FIRST_FD];
    f->used = 1;
    f->handle = info.handle;
    f->size = info.size;
    f->offset = 0;
    return fd;
}

int close(int fd) {
    struct openfile *f = slot(fd);
    if (!f) {
        errno = EBADF;
        return -1;
    }
    quark_vfs_close(f->handle);
    f->used = 0;
    return 0;
}

ssize_t read(int fd, void *buf, size_t n) {
    if (fd < FIRST_FD) {
        /* A kernel descriptor: the read is a system call. */
        unsigned long r = __syscall3(SYS_FD_READ, (unsigned long)fd, (unsigned long)buf, n);
        if (r == QUARK_ERR) {
            errno = EIO;
            return -1;
        }
        return (ssize_t)r;
    }

    struct openfile *f = slot(fd);
    if (!f) {
        errno = EBADF;
        return -1;
    }
    if (xfer_page() != 0) {
        return -1;
    }

    size_t done = 0;
    unsigned char *out = buf;
    /* One page per message, so anything larger is a loop. */
    while (done < n) {
        size_t want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        unsigned long got = 0;
        int err = quark_vfs_read(f->handle, xfer_phys, f->offset, want, &got);
        if (err) {
            errno = vfs_errno(err);
            return done ? (ssize_t)done : -1;
        }
        if (got == 0) {
            break; /* end of file */
        }
        memcpy(out + done, (const void *)XFER_VADDR, got);
        done += got;
        f->offset += got;
        /* A short read means the end, not a hiccup: the VFS answers from a
           page at a time and gives everything it has. */
        if (got < want) {
            break;
        }
    }
    return (ssize_t)done;
}

ssize_t write(int fd, const void *buf, size_t n) {
    if (fd < FIRST_FD) {
        unsigned long r = __syscall3(SYS_FD_WRITE, (unsigned long)fd, (unsigned long)buf, n);
        if (r == QUARK_ERR) {
            errno = EIO;
            return -1;
        }
        return (ssize_t)r;
    }

    struct openfile *f = slot(fd);
    if (!f) {
        errno = EBADF;
        return -1;
    }
    if (xfer_page() != 0) {
        return -1;
    }

    size_t done = 0;
    const unsigned char *in = buf;
    while (done < n) {
        size_t want = n - done;
        if (want > PAGE_SIZE) {
            want = PAGE_SIZE;
        }
        memcpy((void *)XFER_VADDR, in + done, want);

        unsigned long put = 0;
        int err = quark_vfs_write(f->handle, xfer_phys, f->offset, want, &put);
        if (err) {
            errno = vfs_errno(err);
            return done ? (ssize_t)done : -1;
        }
        done += put;
        f->offset += put;
        if (put < want) {
            break;
        }
    }
    return (ssize_t)done;
}

pid_t getpid(void) {
    return (pid_t)__syscall0(SYS_GETPID);
}

/* The PIT runs at 100 Hz, so a tick is 10 ms — the only clock there is. */
int usleep(unsigned int usec) {
    unsigned long ticks = (usec + 9999) / 10000;
    unsigned long until = __syscall0(SYS_TICKS) + ticks;
    while (__syscall0(SYS_TICKS) < until) {
        __syscall0(SYS_YIELD);
    }
    return 0;
}

unsigned int sleep(unsigned int seconds) {
    for (unsigned int i = 0; i < seconds; i++) {
        usleep(1000000);
    }
    return 0;
}

long readfile(const char *path, char *buf, long size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = read(fd, buf, (size_t)size);
    close(fd);
    return n;
}

time_t time(time_t *t) {
    time_t now = (time_t)(__syscall0(SYS_TICKS) / 100);
    if (t) {
        *t = now;
    }
    return now;
}

clock_t clock(void) {
    return (clock_t)__syscall0(SYS_TICKS);
}

int nanosleep(const struct timespec *req, struct timespec *rem) {
    (void)rem;
    if (!req) {
        errno = EINVAL;
        return -1;
    }
    /* Rounded up: a sleep that returns early is a bug, one that returns a
       tick late is a 100 Hz timer. */
    unsigned long ticks = (unsigned long)req->tv_sec * 100
                        + (unsigned long)((req->tv_nsec + 9999999) / 10000000);
    unsigned long until = __syscall0(SYS_TICKS) + ticks;
    while (__syscall0(SYS_TICKS) < until) {
        __syscall0(SYS_YIELD);
    }
    return 0;
}
