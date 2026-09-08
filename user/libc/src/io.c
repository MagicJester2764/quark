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
#include <unistd.h>
#include <quark/layout.h>
#include <quark/syscall.h>

#define TAG_OPEN   1
#define TAG_READ   2
#define TAG_CLOSE  3
#define TAG_WRITE  6
#define TAG_CREATE 7
#define TAG_ERROR  ((unsigned long)-1)

/* Errors the VFS reports, which are its own small integers rather than
   anybody's errno. */
#define VFS_NOT_FOUND      1
#define VFS_INVALID_HANDLE 2
#define VFS_IO             3
#define VFS_TOO_MANY_OPEN  4
#define VFS_INVALID_PATH   5
#define VFS_NOT_DIR        6
#define VFS_IS_DIR         7
#define VFS_PERMISSION     8
#define VFS_READ_ONLY      9

#define PAGE_SIZE 4096
/* The shared transfer page: above everything the loader places, and clear of
   the stack and the heap. */
#define XFER_VADDR QUARK_XFER_PAGE

/* Longest path the VFS protocol carries: it travels in the six data words of
   one message, with room for a terminator. */
#define MAX_PATH 47

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

static int vfs_errno(unsigned long code) {
    switch (code) {
    case VFS_NOT_FOUND:      return ENOENT;
    case VFS_INVALID_HANDLE: return EBADF;
    case VFS_IO:             return EIO;
    case VFS_TOO_MANY_OPEN:  return EMFILE;
    case VFS_INVALID_PATH:   return EINVAL;
    case VFS_NOT_DIR:        return ENOTDIR;
    case VFS_IS_DIR:         return EISDIR;
    case VFS_PERMISSION:     return EACCES;
    case VFS_READ_ONLY:      return EROFS;
    default:                 return EIO;
    }
}

static struct openfile *slot(int fd) {
    if (fd < FIRST_FD || fd >= FIRST_FD + MAX_FILES) {
        return 0;
    }
    struct openfile *f = &files[fd - FIRST_FD];
    return f->used ? f : 0;
}

/* Pack a path into a message's data words, as the VFS expects it. */
static int path_msg(struct quark_msg *msg, unsigned long tag, const char *path) {
    size_t len = strlen(path);
    if (len > MAX_PATH) {
        errno = EINVAL;
        return -1;
    }
    memset(msg, 0, sizeof *msg);
    msg->tag = tag;
    memcpy(msg->data, path, len);
    return 0;
}

int open(const char *path, int flags, ...) {
    size_t vfs = quark_vfs();
    if (vfs == 0) {
        errno = EIO;
        return -1;
    }

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

    struct quark_msg msg, reply;
    unsigned long tag = (flags & O_CREAT) ? TAG_CREATE : TAG_OPEN;
    if (path_msg(&msg, tag, path) != 0) {
        return -1;
    }
    /* TAG_CREATE reads its flags from the last data word, which the path
       never reaches: a path that long is refused above. */
    if (tag == TAG_CREATE) {
        msg.data[5] = 0; /* a file, not a directory */
    }

    if (quark_call(vfs, &msg, &reply) != 0) {
        errno = EIO;
        return -1;
    }
    if (reply.tag == TAG_ERROR) {
        errno = vfs_errno(reply.data[0]);
        return -1;
    }

    struct openfile *f = &files[fd - FIRST_FD];
    f->used = 1;
    f->handle = reply.data[0];
    f->size = reply.data[1];
    f->offset = 0;
    return fd;
}

int close(int fd) {
    struct openfile *f = slot(fd);
    if (!f) {
        errno = EBADF;
        return -1;
    }
    struct quark_msg msg, reply;
    memset(&msg, 0, sizeof msg);
    msg.tag = TAG_CLOSE;
    msg.data[0] = f->handle;
    quark_call(quark_vfs(), &msg, &reply);
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
        struct quark_msg msg, reply;
        memset(&msg, 0, sizeof msg);
        msg.tag = TAG_READ;
        msg.data[0] = f->handle;
        msg.data[1] = xfer_phys;
        msg.data[2] = f->offset;
        msg.data[3] = want;

        if (quark_call(quark_vfs(), &msg, &reply) != 0) {
            errno = EIO;
            return done ? (ssize_t)done : -1;
        }
        if (reply.tag == TAG_ERROR) {
            errno = vfs_errno(reply.data[0]);
            return done ? (ssize_t)done : -1;
        }
        unsigned long got = reply.data[0];
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

        struct quark_msg msg, reply;
        memset(&msg, 0, sizeof msg);
        msg.tag = TAG_WRITE;
        msg.data[0] = f->handle;
        msg.data[1] = xfer_phys;
        msg.data[2] = f->offset;
        msg.data[3] = want;

        if (quark_call(quark_vfs(), &msg, &reply) != 0) {
            errno = EIO;
            return done ? (ssize_t)done : -1;
        }
        if (reply.tag == TAG_ERROR) {
            errno = vfs_errno(reply.data[0]);
            return done ? (ssize_t)done : -1;
        }
        unsigned long put = reply.data[0];
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
