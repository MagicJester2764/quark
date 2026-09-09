/* The Linux system call surface, answered by Quark.
 *
 * musl is written against Linux. Not against "a kernel" — against Linux's
 * numbers, its argument order, its error convention and its idea of what a
 * process is. Porting it to a system whose calls are a different set with
 * different semantics is therefore not a port at all: it is a translation
 * layer, and this is it.
 *
 * The shape of the translation is the interesting part. Quark is a
 * microkernel, so most of what Linux calls a system call is not one here:
 * `open` and `read` are messages to the VFS, `write` on a descriptor is a
 * message to whatever is on the other end of it. The layer is mostly an IPC
 * client wearing Linux's numbers, which is the same thing the C library was
 * already — one level further down.
 *
 * Anything not translated returns -ENOSYS rather than pretending. A libc that
 * is told "no" can usually carry on; one that is told "yes" and handed a lie
 * fails somewhere unrelated and much later.
 */

#include <quark/layout.h>
#include <quark/syscall.h>

typedef long ssize_t;
typedef unsigned long size_t;

#define NULL ((void *)0)

/* Linux x86-64 numbers, only the ones that are answered or deliberately
   refused. The rest fall through to -ENOSYS by not being here. */
#define LX_read              0
#define LX_write             1
#define LX_open              2
#define LX_close             3
#define LX_fstat             5
#define LX_lseek             8
#define LX_mmap              9
#define LX_mprotect         10
#define LX_munmap           11
#define LX_brk              12
#define LX_rt_sigaction     13
#define LX_rt_sigprocmask   14
#define LX_ioctl            16
#define LX_readv            19
#define LX_writev           20
#define LX_madvise          28
#define LX_getpid           39
#define LX_exit             60
#define LX_uname            63
#define LX_getcwd           79
#define LX_getuid          102
#define LX_getgid          104
#define LX_geteuid         107
#define LX_getegid         108
#define LX_arch_prctl      158
#define LX_futex           202
#define LX_set_tid_address 218
#define LX_clock_gettime   228
#define LX_exit_group      231
#define LX_openat          257
#define LX_set_robust_list 273
#define LX_prlimit64       302
#define LX_getrandom       318
#define LX_statx           332
#define LX_rseq            334

/* Errors, in the values Linux uses — they are what musl compares against. */
#define LX_EPERM     1
#define LX_ENOENT    2
#define LX_EBADF     9
#define LX_ENOMEM   12
#define LX_EFAULT   14
#define LX_EINVAL   22
#define LX_ENOSYS   38
#define LX_ENOTTY   25
#define LX_ESPIPE   29

#define ARCH_SET_FS 0x1002
#define ARCH_GET_FS 0x1003

/* Where anonymous mappings go.
 *
 * Linux's mmap picks an address; Quark's maps one you name, because a task
 * knows its own address space and the kernel has no business guessing. So the
 * choosing happens here, as a bump allocator over a region nothing else uses.
 * Nothing reclaims a hole left by munmap: a libc allocator returns memory to
 * itself far more often than it returns it here, and an address space this
 * size is not the scarce thing. */
#define MMAP_BASE   0x93000000000UL
#define MMAP_LIMIT  0x9F000000000UL
static unsigned long mmap_next = MMAP_BASE;

#define PAGE_SIZE 4096UL
/* What sys_mmap will take in one call. */
#define MAP_CHUNK 256UL

static long map_pages(unsigned long at, unsigned long pages) {
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        if (__syscall2(SYS_MMAP, at + done * PAGE_SIZE, n) == QUARK_ERR) {
            return -1;
        }
        done += n;
    }
    return 0;
}

static long do_mmap(unsigned long len) {
    if (len == 0) {
        return -LX_EINVAL;
    }
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long at = mmap_next;
    if (at + pages * PAGE_SIZE > MMAP_LIMIT) {
        return -LX_ENOMEM;
    }
    if (map_pages(at, pages) != 0) {
        return -LX_ENOMEM;
    }
    mmap_next = at + pages * PAGE_SIZE;
    return (long)at;
}

static long do_munmap(unsigned long at, unsigned long len) {
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        __syscall2(SYS_MUNMAP, at + done * PAGE_SIZE, n);
        done += n;
    }
    return 0;
}

struct iovec {
    void  *iov_base;
    size_t iov_len;
};

static long do_write(long fd, const void *buf, unsigned long n) {
    unsigned long r = __syscall3(SYS_FD_WRITE, (unsigned long)fd, (unsigned long)buf, n);
    return r == QUARK_ERR ? -LX_EBADF : (long)r;
}

static long do_read(long fd, void *buf, unsigned long n) {
    unsigned long r = __syscall3(SYS_FD_READ, (unsigned long)fd, (unsigned long)buf, n);
    return r == QUARK_ERR ? -LX_EBADF : (long)r;
}

/* Linux's struct timespec, which is what clock_gettime fills in. */
struct lx_timespec {
    long tv_sec;
    long tv_nsec;
};

/* Setting the thread pointer arrives here rather than through the numbered
 * table: musl issues it from assembly, because on Linux it is one instruction
 * and not worth a call. Quark has a system call for exactly this, so it is a
 * direct translation and the shortest path in the whole layer. */
long __quark_set_fs(unsigned long tp);

long __quark_set_fs(unsigned long tp) {
    if (__syscall1(SYS_SET_FS_BASE, tp) == QUARK_ERR) {
        return -LX_EINVAL;
    }
    return 0;
}

long __quark_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6);

long __quark_syscall(long n, long a1, long a2, long a3, long a4, long a5, long a6) {
    (void)a4; (void)a5; (void)a6;

    switch (n) {
    case LX_write:
        return do_write(a1, (const void *)a2, (unsigned long)a3);

    case LX_read:
        return do_read(a1, (void *)a2, (unsigned long)a3);

    case LX_writev: {
        const struct iovec *v = (const struct iovec *)a2;
        long total = 0;
        for (long i = 0; i < a3; i++) {
            if (v[i].iov_len == 0) {
                continue;
            }
            long w = do_write(a1, v[i].iov_base, v[i].iov_len);
            if (w < 0) {
                return total ? total : w;
            }
            total += w;
            if ((unsigned long)w < v[i].iov_len) {
                break; /* short write; the caller asks again */
            }
        }
        return total;
    }

    case LX_readv: {
        const struct iovec *v = (const struct iovec *)a2;
        long total = 0;
        for (long i = 0; i < a3; i++) {
            if (v[i].iov_len == 0) {
                continue;
            }
            long r = do_read(a1, v[i].iov_base, v[i].iov_len);
            if (r < 0) {
                return total ? total : r;
            }
            total += r;
            if ((unsigned long)r < v[i].iov_len) {
                break;
            }
        }
        return total;
    }

    case LX_mmap:
        /* Only anonymous mappings. A file mapping would have to be read
           through the VFS into pages, which is a different thing wearing the
           same name, and musl does not need it to start. */
        if (a5 != -1L) {
            return -LX_ENOSYS;
        }
        return do_mmap((unsigned long)a2);

    case LX_munmap:
        return do_munmap((unsigned long)a1, (unsigned long)a2);

    /* Nothing here has page permissions to change after the fact, and the
       mapping already allows what was asked for. */
    case LX_mprotect:
    case LX_madvise:
        return 0;

    /* There is no brk. Saying so is what makes musl use mmap instead, which
       is the path that works. */
    case LX_brk:
        return -LX_ENOMEM;

    case LX_arch_prctl:
        if (a1 == ARCH_SET_FS) {
            __syscall1(SYS_SET_FS_BASE, (unsigned long)a2);
            return 0;
        }
        return -LX_EINVAL;

    case LX_set_tid_address:
        return (long)__syscall0(SYS_GETPID);

    case LX_getpid:
        return (long)__syscall0(SYS_GETPID);

    /* One user, and it is the one that started the program. */
    case LX_getuid:
    case LX_geteuid:
    case LX_getgid:
    case LX_getegid:
        return 0;

    case LX_exit:
    case LX_exit_group:
        __syscall1(SYS_EXIT_CODE, (unsigned long)a1);
        for (;;) { }

    case LX_futex: {
        /* FUTEX_WAIT is 0 and FUTEX_WAKE is 1, with the private flag masked
           off: every process here has its own address space, so every futex
           is private already. */
        long op = a2 & 0x7f;
        if (op == 0) {
            __syscall2(SYS_FUTEX_WAIT, (unsigned long)a1, (unsigned long)a3);
            return 0;
        }
        if (op == 1) {
            __syscall2(SYS_FUTEX_WAKE, (unsigned long)a1, (unsigned long)a3);
            return 0;
        }
        return -LX_ENOSYS;
    }

    case LX_clock_gettime: {
        struct lx_timespec *ts = (struct lx_timespec *)a2;
        if (!ts) {
            return -LX_EFAULT;
        }
        /* A 100 Hz tick counter and no real-time clock, so this counts from
           boot. See <time.h> in the C library: an obviously small number is
           better than a confident wrong date. */
        unsigned long ticks = __syscall0(SYS_TICKS);
        ts->tv_sec = (long)(ticks / 100);
        ts->tv_nsec = (long)((ticks % 100) * 10000000L);
        return 0;
    }

    /* Refused deliberately, and each for a reason worth stating rather than
       leaving as an unexplained failure later:

       ioctl  — the terminal is a service on the other end of a descriptor,
                not a device with ioctls. ENOTTY is the true answer, and it is
                also the one that makes isatty() say "no" and stdio pick block
                buffering, which is what we want.
       signals — Quark has three, delivered to a task rather than to a handler
                a program installs. Accepting a sigaction would be promising
                something that cannot happen.
       rseq, robust lists, getrandom, prlimit — no equivalent, and musl copes
                with being refused all four. */
    case LX_ioctl:
        return -LX_ENOTTY;
    case LX_rt_sigaction:
    case LX_rt_sigprocmask:
        return 0; /* accepted and ignored: musl masks signals during startup */
    case LX_set_robust_list:
    case LX_rseq:
    case LX_prlimit64:
    case LX_getrandom:
    case LX_uname:
    case LX_getcwd:
        return -LX_ENOSYS;

    /* Files are the VFS's, and reaching it needs the IPC client that the C
       library already has. Not yet wired through here, which is why this says
       so plainly instead of returning a descriptor that does not work. */
    case LX_open:
    case LX_openat:
    case LX_close:
    case LX_fstat:
    case LX_statx:
    case LX_lseek:
        return -LX_ENOSYS;

    default:
        return -LX_ENOSYS;
    }
}
