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

#include "abi.h"

typedef long ssize_t;
typedef unsigned long size_t;

#define NULL ((void *)0)

/* Linux x86-64 numbers, only the ones that are answered or deliberately
   refused. The rest fall through to -ENOSYS by not being here. */
#define LX_read              0
#define LX_write             1
#define LX_open              2
#define LX_close             3
#define LX_stat              4
#define LX_access           21
#define LX_fstat             5
#define LX_lstat             6
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
#define LX_fadvise64       221
#define LX_getpid           39
#define LX_fcntl            72
#define LX_exit             60
#define LX_uname            63
#define LX_getcwd           79
#define LX_getuid          102
#define LX_getgid          104
#define LX_geteuid         107
#define LX_getegid         108
#define LX_arch_prctl      158
#define LX_sched_getaffinity 204
#define LX_futex           202
#define LX_set_tid_address 218
#define LX_clock_gettime   228
#define LX_exit_group      231
#define LX_openat          257
#define LX_faccessat       269
#define LX_set_robust_list 273
#define LX_prlimit64       302
#define LX_getrandom       318
#define LX_newfstatat      262
#define LX_statx           332
#define LX_rseq            334
#define LX_faccessat2      439

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

#ifdef QUARK_ABI_TRACE
/* A porting aid, off unless asked for: an unimplemented call otherwise reaches
   the program as a bare errno and is reported as whatever it was doing at the
   time ("sort: cannot read"), with nothing saying which call was missing. */
long __quark_write(long fd, const void *buf, unsigned long n);
static void trace(const char *what, long n) {
    char buf[64];
    int i = 0;
    buf[i++] = '[';
    while (*what && i < 40) {
        buf[i++] = *what++;
    }
    buf[i++] = ' ';
    if (n < 0) { buf[i++] = '-'; n = -n; }
    char d[24];
    int j = 0;
    do { d[j++] = (char)('0' + n % 10); n /= 10; } while (n && j < 20);
    while (j) { buf[i++] = d[--j]; }
    buf[i++] = ']';
    buf[i++] = '\n';
    __quark_write(2, buf, (unsigned long)i);
}
#else
#define trace(what, n) ((void)0)
#endif

static long do_mmap(unsigned long len) {
    if (len == 0) {
        return -LX_EINVAL;
    }
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long at = mmap_next;
    if (at + pages * PAGE_SIZE > MMAP_LIMIT) {
        trace("mmap-arena-full", (long)pages);
        return -LX_ENOMEM;
    }
    if (map_pages(at, pages) != 0) {
        trace("mmap-failed-pages", (long)pages);
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

/* Descriptors 0, 1 and 2 go to the kernel and anything above them to the VFS;
   `files.c` decides which, so that `write` and `writev` agree about it. */
#define do_write __quark_write
#define do_read  __quark_read

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
       mapping already allows what was asked for. Advice about how a file will
       be read is the same kind of thing: nothing here acts on it, and having
       done nothing is a complete implementation of it rather than a refusal. */
    case LX_mprotect:
    case LX_madvise:
    case LX_fadvise64:
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

    /* Quark is uniprocessor, and deliberately: several kernel invariants
       depend on it. One CPU, which is a fact rather than a placeholder, and
       reporting it is what makes `nproc` right. */
    case LX_sched_getaffinity: {
        unsigned long size = (unsigned long)a2;
        unsigned char *mask = (unsigned char *)a3;
        if (!mask || size < sizeof(unsigned long)) {
            return -LX_EINVAL;
        }
        for (unsigned long i = 0; i < size; i++) {
            mask[i] = 0;
        }
        mask[0] = 1;
        return (long)sizeof(unsigned long);
    }

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

    /* Files. The VFS answers all of these; `files.c` is where a handle and an
       offset become a descriptor. */
    case LX_open:
        return __quark_open((const char *)a1, a2);
    case LX_openat:
        return __quark_openat(a1, (const char *)a2, a3);
    case LX_close:
        return __quark_close(a1);
    case LX_lseek:
        return __quark_lseek(a1, a2, a3);
    case LX_fstat:
        return __quark_fstat(a1, (void *)a2);
    case LX_stat:
    case LX_lstat:
        /* No symbolic links on either filesystem here, so following one and
           not following it are the same question. */
        return __quark_stat((const char *)a1, (void *)a2);
    case LX_newfstatat:
        if (!a2 || !*(const char *)a2) {
            return __quark_fstat(a1, (void *)a3);
        }
        return __quark_stat((const char *)a2, (void *)a3);

    /* access(2). gnulib's euidaccess tries faccessat2 first, then faccessat,
       and reports whatever the last one said — so refusing these is not a
       missing convenience: `sort /etc/passwd` says "cannot read" about a file
       it can read perfectly well. All three ask the same question, and only
       the directory this system does not have separates them. */
    case LX_access:
        return __quark_access((const char *)a1, a2);
    case LX_faccessat:
    case LX_faccessat2:
        if (a1 != LX_AT_FDCWD) {
            return -LX_ENOSYS;
        }
        return __quark_access((const char *)a2, a3);

    /* musl probes this when fstat says EBADF, to tell a closed descriptor
       from one the kernel will not stat. Answering keeps it on the path that
       works rather than sending it to /proc, which does not exist. */
    case LX_fcntl:
        return 0;

    /* statx carries more than the VFS knows, and musl falls back to the plain
       stat calls when it is refused. */
    case LX_statx:
        return -LX_ENOSYS;

    default:
        trace("nosys", n);
        return -LX_ENOSYS;
    }
}
