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
#define LX_poll              7
#define LX_stat              4
#define LX_access           21
#define LX_fstat             5
#define LX_lstat             6
#define LX_lseek             8
#define LX_mmap              9
#define LX_mprotect         10
#define LX_munmap           11
#define LX_msync            26
#define LX_brk              12
#define LX_rt_sigaction     13
#define LX_rt_sigprocmask   14
#define LX_ioctl            16
#define LX_readv            19
#define LX_writev           20
#define LX_madvise          28
#define LX_sendmsg          46
#define LX_recvmsg          47
#define LX_pipe              22
#define LX_eventfd         284
#define LX_eventfd2        290
#define LX_pipe2           293
#define LX_socketpair       53
#define LX_fadvise64       221
#define LX_getpid           39
#define LX_getppid         110
#define LX_wait4            61
#define LX_fork             57
#define LX_execve           59
#define LX_vfork            58
#define LX_clone            56
#define LX_setsid          112
#define LX_timerfd_create  283
#define LX_timerfd_settime 286
#define LX_timerfd_gettime 287
#define LX_fcntl            72
#define LX_flock            73
#define LX_exit             60
#define LX_uname            63
#define LX_getcwd           79
#define LX_chdir            80
#define LX_fchdir           81
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
#define LX_epoll_wait      232
#define LX_epoll_ctl       233
#define LX_ppoll           271
#define LX_epoll_create1   291
#define LX_ftruncate       77
#define LX_fallocate      285
#define LX_memfd_create    319
#define LX_faccessat2      439
#define LX_mkdir            83
#define LX_mkdirat         258
#define LX_truncate         76
#define LX_rename           82
#define LX_rmdir            84
#define LX_link             86
#define LX_unlink           87
#define LX_unlinkat        263
#define LX_renameat        264
#define LX_linkat          265
#define LX_renameat2       316
#define LX_AT_REMOVEDIR  0x200
#define LX_getdents64      217
#define LX_dup              32
#define LX_pread64          17
#define LX_pwrite64         18
#define LX_dup2             33
#define LX_dup3            292
#define LX_O_CLOEXEC  02000000
#define LX_readlink         89
#define LX_symlink          88
#define LX_symlinkat       266
#define LX_readlinkat      267
#define LX_statfs          137
#define LX_fstatfs         138

#define LX_CLOCK_REALTIME        0
#define LX_CLOCK_REALTIME_COARSE 5
#define LX_CLOCK_TAI             11
#define LX_CLOCK_MONOTONIC       1
#define LX_CLOCK_PROCESS_CPUTIME 2
#define LX_CLOCK_THREAD_CPUTIME  3
#define LX_TIMER_ABSTIME         1
#define LX_nanosleep            35
#define LX_clock_nanosleep     230

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
/* What sys_munmap will take in one call. */
#define MAP_CHUNK 256UL

static void unmap_pages(unsigned long at, unsigned long pages) {
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        __syscall2(SYS_MUNMAP, at + done * PAGE_SIZE, n);
        done += n;
    }
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

#define LX_MAP_POPULATE  0x8000
#define LX_MAP_NORESERVE 0x4000

/* Anonymous memory is reserved whole and given its frames as it is touched,
   as Linux gives them: a mapping far bigger than what is used costs what is
   used. As Linux's overcommit heuristic does, a mapping bigger than the whole
   machine is refused unless MAP_NORESERVE says the program knows — calloc of
   a size nothing could hold has to come back NULL, not succeed and then die
   reading it. MAP_POPULATE asks for all of it now, which can be refused. */
static long do_mmap(unsigned long len, long flags) {
    if (len == 0) {
        return -LX_EINVAL;
    }
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long at = mmap_next;
    if (pages > (MMAP_LIMIT - at) / PAGE_SIZE) {
        trace("mmap-arena-full", (long)pages);
        return -LX_ENOMEM;
    }
    unsigned long how = (flags & LX_MAP_POPULATE) ? QUARK_MAP_POPULATE : 0;
    if (!(flags & LX_MAP_NORESERVE)) {
        how |= QUARK_MAP_ACCOUNT;
    }
    if (__syscall3(SYS_MAP_ANON, at, pages, how) == QUARK_ERR) {
        trace("mmap-failed-pages", (long)pages);
        return -LX_ENOMEM;
    }
    mmap_next = at + pages * PAGE_SIZE;
    return (long)at;
}

/* Map memory named by a descriptor, at an address of our choosing. */
static long do_mmap_fd(long fd, unsigned long len) {
    if (len == 0) {
        return -LX_EINVAL;
    }
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long at = mmap_next;
    if (at + pages * PAGE_SIZE > MMAP_LIMIT) {
        return -LX_ENOMEM;
    }
    /* The whole region is mapped, whatever the caller asked to see of it --
       Quark has no partial mapping of a descriptor. The call says how much
       that was, and the arena must step over all of it: advancing by the
       caller's length instead would hand the next mapping addresses that are
       already occupied, and the kernel refuses to map over them. */
    unsigned long got = __syscall2(SYS_MMAP_FD, (unsigned long)fd, at);
    if (got == QUARK_ERR) {
        return -LX_ENODEV;
    }
    unsigned long real = (got + PAGE_SIZE - 1) / PAGE_SIZE;
    mmap_next = at + (real > pages ? real : pages) * PAGE_SIZE;
    return (long)at;
}

#define LX_PROT_WRITE   2
#define LX_PROT_EXEC    4
#define LX_MAP_SHARED   1

/* A file, through the VFS: a memory object whose pages the server provides
   as they are touched. The capability it grants is only needed to make the
   mapping, which keeps the object; it goes straight after. */
static long do_mmap_file(long fd, unsigned long len, long prot, long flags, long off) {
    if (len == 0 || off < 0 || (off & (PAGE_SIZE - 1))) {
        return -LX_EINVAL;
    }
    unsigned long pages = (len + PAGE_SIZE - 1) / PAGE_SIZE;
    unsigned long at = mmap_next;
    if (pages > (MMAP_LIMIT - at) / PAGE_SIZE) {
        return -LX_ENOMEM;
    }
    int shared = (flags & LX_MAP_SHARED) != 0;
    int write = (prot & LX_PROT_WRITE) != 0;
    unsigned long slot;
    long bad = __quark_file_map(fd, shared && write, &slot);
    if (bad) {
        return bad;
    }
    unsigned long how = (write ? QUARK_OBJECT_WRITE : 0) | (shared ? QUARK_OBJECT_SHARED : 0) |
                        ((prot & LX_PROT_EXEC) ? QUARK_OBJECT_EXEC : 0);
    unsigned long r = __syscall5(SYS_OBJECT_MAP, slot, at, pages,
                                 (unsigned long)off / PAGE_SIZE, how);
    __syscall1(SYS_CAP_DELETE, slot);
    if (r == QUARK_ERR) {
        return -LX_ENOMEM;
    }
    mmap_next = at + pages * PAGE_SIZE;
    return (long)at;
}

static long do_munmap(unsigned long at, unsigned long len) {
    unmap_pages(at, (len + PAGE_SIZE - 1) / PAGE_SIZE);
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

/* Sleep until `req` has passed on `clock` — or, with TIMER_ABSTIME, until
   the clock reads `req`. Time here is a 100 Hz tick, so a sleep is rounded
   up to whole ticks and one more, never ending early. It waits by receiving
   from itself, which nobody sends to. */
static long do_sleep(long clock, long flags, const struct lx_timespec *req) {
    if (!req) {
        return -LX_EFAULT;
    }
    if (req->tv_sec < 0 || req->tv_nsec < 0 || req->tv_nsec >= 1000000000L) {
        return -LX_EINVAL;
    }
    if (clock == LX_CLOCK_PROCESS_CPUTIME || clock == LX_CLOCK_THREAD_CPUTIME || clock < 0) {
        return -LX_EINVAL;
    }
    unsigned long now = __syscall0(SYS_TICKS);
    unsigned long ticks = (unsigned long)req->tv_sec * 100 +
                          ((unsigned long)req->tv_nsec + 9999999UL) / 10000000UL;
    unsigned long deadline;
    if (flags & LX_TIMER_ABSTIME) {
        long boot = 0;
        if (clock == LX_CLOCK_REALTIME || clock == LX_CLOCK_REALTIME_COARSE || clock == LX_CLOCK_TAI) {
            boot = (long)__syscall0(SYS_BOOT_TIME);
        }
        long since_boot = req->tv_sec - boot;
        if (since_boot < 0) {
            return 0;
        }
        deadline = (unsigned long)since_boot * 100 +
                   ((unsigned long)req->tv_nsec + 9999999UL) / 10000000UL;
    } else {
        deadline = now + ticks + (ticks ? 1 : 0);
    }
    unsigned long self = __syscall0(SYS_GETPID);
    while ((now = __syscall0(SYS_TICKS)) < deadline) {
        struct quark_msg m;
        __syscall3(SYS_RECV_TIMEOUT, self, (unsigned long)&m, deadline - now);
    }
    return 0;
}

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

/* uname: six fields of 65 bytes. The release is the kernel's ABI version,
   which is the thing a program built for Quark would want to compare. */
static void put_field(char *field, const char *text) {
    int i = 0;
    for (; text[i] && i < 64; i++) {
        field[i] = text[i];
    }
    for (; i < 65; i++) {
        field[i] = 0;
    }
}

static long do_uname(char *u) {
    if (!u) {
        return -LX_EFAULT;
    }
    unsigned long v = __syscall0(SYS_ABI_VERSION);
    char release[16];
    int n = 0;
    unsigned long parts[2] = { v >> 16, v & 0xffff };
    for (int p = 0; p < 2; p++) {
        char digits[8];
        int d = 0;
        unsigned long x = parts[p];
        do {
            digits[d++] = (char)('0' + x % 10);
            x /= 10;
        } while (x && d < 7);
        while (d) {
            release[n++] = digits[--d];
        }
        if (p == 0) {
            release[n++] = '.';
        }
    }
    release[n] = 0;
    put_field(u, "Quark");
    put_field(u + 65, "quark");
    put_field(u + 130, release);
    put_field(u + 195, "Quark microkernel");
    put_field(u + 260, "x86_64");
    put_field(u + 325, "(none)");
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
        /* A mapping backed by a descriptor is `wl_shm`: the caller made memory
           with memfd_create and wants it visible. Quark maps at an address you
           name, so the address is chosen here from the same arena anonymous
           mappings come out of.
           A mapping of a file is a memory object the VFS pages, below. */
        if (a5 >= LX_FIRST_FILE_FD) {
            return do_mmap_file(a5, (unsigned long)a2, a3, a4, a6);
        }
        if (a5 != -1L) {
            return do_mmap_fd(a5, (unsigned long)a2);
        }
        return do_mmap((unsigned long)a2, a4);

    case LX_munmap:
        return do_munmap((unsigned long)a1, (unsigned long)a2);

    /* What a shared mapping wrote reaches the file before this returns,
       whether it was asked for now (MS_SYNC) or eventually (MS_ASYNC):
       nothing here writes asynchronously. MS_INVALIDATE asks for nothing a
       mapping of the cache itself needs. */
    case LX_msync: {
        if ((a1 & (PAGE_SIZE - 1)) || (a3 & ~7L) || ((a3 & 1) && (a3 & 4))) {
            return -LX_EINVAL;
        }
        unsigned long pages = ((unsigned long)a2 + PAGE_SIZE - 1) / PAGE_SIZE;
        if (pages == 0) {
            return 0;
        }
        return __syscall2(SYS_OBJECT_SYNC, (unsigned long)a1, pages) == QUARK_ERR ? -LX_EIO : 0;
    }

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
        // Not a formality: this is where the word to clear on exit is
        // registered, and musl's thread-list lock is that word.
        return (long)__syscall1(SYS_SET_CLEAR_TID, (unsigned long)a1);

    case LX_getpid:
        return (long)__syscall0(SYS_GETPID);

    /* Waiting for a child.
     *
     * `SYS_WAIT` answers with the child's id and its exit code packed in one
     * word, and reaps it: by the time it returns, that child's memory is free
     * and its id may already belong to something else. Linux's status word is
     * a different shape — the exit code lives in bits 8 to 15, and the low
     * bits say whether it was a signal — so the translation is the whole of
     * what this does.
     *
     * A negative code is the kernel saying "killed", with the signal number
     * negated, which is Linux's low byte with no `WIFEXITED` bit above it. */
    case LX_wait4: {
        unsigned long got = __syscall0(SYS_WAIT);
        if (got == QUARK_ERR) {
            return -LX_ECHILD;
        }
        long pid = (long)(got & 0xFFFFFFFFUL);
        int code = (int)(got >> 32);
        /* A specific child, when it is not the one that finished, is more than
           this kernel can say. Reporting the one that did is the honest
           answer: a caller waiting on one child has just been given it. */
        if (a1 > 0 && pid != a1) {
            /* nothing else to do: the child was reaped, and saying otherwise
               would leave the caller waiting for a task that no longer is. */
        }
        if (a2) {
            int *status = (int *)a2;
            *status = code < 0 ? (-code & 0x7F) : ((code & 0xFF) << 8);
        }
        return pid;
    }

    case LX_getppid:
        return 1;

    /* A process of one's own.
     *
     * musl calls `SYS_fork` on x86_64 and reaches `__clone` only for threads,
     * so this is the path an ordinary `fork()` takes. `vfork` is the same
     * thing here: its promise is that the parent is suspended until the child
     * execs or exits, and a real fork keeps every program that relies on that
     * working — more slowly, and without the sharp edges.
     *
     * `clone` arrives here only from a program calling it directly, since
     * musl's own thread path is a tail call into `__quark_clone`. Anything
     * asking for a new process with a shared file table or a stopped child is
     * asking for a Linux this is not. */
    /* Become another program. It does not return unless it failed, and what
       it answers then is what `execl` reports. */
    case LX_execve:
        return __quark_execve((const char *)a1, (char *const *)a2, (char *const *)a3);

    case LX_fork:
    case LX_vfork: {
        unsigned long child = __syscall0(SYS_FORK);
        return child == QUARK_ERR ? -LX_EAGAIN : (long)child;
    }
    case LX_clone: {
        unsigned long flags = (unsigned long)a1;
        if (flags & 0x00000100UL /* CLONE_VM */) {
            return -LX_ENOSYS;
        }
        unsigned long child = __syscall0(SYS_FORK);
        return child == QUARK_ERR ? -LX_EAGAIN : (long)child;
    }

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
           is private already.
         *
         * The fourth argument is a *relative* timeout for FUTEX_WAIT, and the
         * answer matters as much as the wait: a caller that gave a deadline
         * asks which happened, so it is ETIMEDOUT when the time ran out and
         * EAGAIN when the word had already changed. Dropping the timeout made
         * every wait with a deadline wait for ever -- which is what
         * `g_cond_wait_until` and musl's `sem_timedwait` are built out of,
         * and glib's thread pool is built out of that. */
        long op = a2 & 0x7f;
        if (op == 0) {
            unsigned long r;
            if (a4) {
                const struct lx_timespec *ts = (const struct lx_timespec *)a4;
                /* Rounded up: the PIT ticks at 100 Hz, and a wait that came
                   back early would be a wait that did not happen. */
                unsigned long ticks =
                    (unsigned long)ts->tv_sec * 100 + (unsigned long)((ts->tv_nsec + 9999999) / 10000000);
                r = __syscall3(SYS_FUTEX_WAIT_TIMEOUT, (unsigned long)a1, (unsigned long)a3, ticks);
            } else {
                r = __syscall2(SYS_FUTEX_WAIT, (unsigned long)a1, (unsigned long)a3);
            }
            if (r == QUARK_ERR) {
                return -LX_EINVAL;
            }
            if (r == 1) {
                return -LX_EAGAIN;
            }
            if (r == 2) {
                return -LX_ETIMEDOUT;
            }
            return 0;
        }
        if (op == 1) {
            unsigned long woken = __syscall2(SYS_FUTEX_WAKE, (unsigned long)a1, (unsigned long)a3);
            return woken == QUARK_ERR ? -LX_EINVAL : (long)woken;
        }
        return -LX_ENOSYS;
    }

    case LX_clock_gettime: {
        struct lx_timespec *ts = (struct lx_timespec *)a2;
        if (!ts) {
            return -LX_EFAULT;
        }
        /* A 100 Hz tick counter, and the date the kernel read at boot. The
           real-time clocks add that date; every other clock counts from boot,
           which is what a monotonic clock is for. */
        unsigned long ticks = __syscall0(SYS_TICKS);
        long base = 0;
        if (a1 == LX_CLOCK_REALTIME || a1 == LX_CLOCK_REALTIME_COARSE || a1 == LX_CLOCK_TAI) {
            base = (long)__syscall0(SYS_BOOT_TIME);
        }
        ts->tv_sec = base + (long)(ticks / 100);
        ts->tv_nsec = (long)((ticks % 100) * 10000000L);
        return 0;
    }

    case LX_nanosleep:
        return do_sleep(LX_CLOCK_MONOTONIC, 0, (const struct lx_timespec *)a1);
    case LX_clock_nanosleep:
        return do_sleep(a1, a2, (const struct lx_timespec *)a3);

    /* Refused deliberately, and each for a reason worth stating rather than
       leaving as an unexplained failure later:

       ioctl  — the terminal is a service on the other end of a descriptor,
                not a device with ioctls. ENOTTY is the true answer, and it is
                also the one that makes isatty() say "no" and stdio pick block
                buffering, which is what we want.
       signals — Quark has three, delivered to a task rather than to a handler
                a program installs. Accepting a sigaction would be promising
                something that cannot happen.
       rseq, robust lists, prlimit — no equivalent, and musl copes with
                being refused all three. */
    case LX_ioctl:
        /* Masked to thirty-two bits on the way in. A request is an `int` in
           the C library's signature, and the ones with the "read" bit set —
           `TIOCGPTN` is 0x80045430 — are negative in it, so they arrive here
           sign-extended and match nothing. */
        return __quark_ioctl(a1, (unsigned long)(unsigned int)a2, (unsigned long)a3);
    /* A deadline as a descriptor.
     *
     * The clock and the flags are read and not honoured: there is one clock
     * here, the tick, and a timerfd that is not close-on-exec is a
     * distinction this system does not draw. The resolution is ten
     * milliseconds, so anything asked for below that fires at the next tick —
     * which is the next time anything happens at all. */
    case LX_timerfd_create: {
        unsigned long fd = __syscall0(SYS_TIMER_CREATE);
        return fd == QUARK_ERR ? -LX_EMFILE : (long)fd;
    }
    case LX_timerfd_settime: {
        /* itimerspec: interval seconds and nanoseconds, then the same for the
           first expiration. Absolute deadlines (TFD_TIMER_ABSTIME) are turned
           into a delay here, since the kernel counts only forwards. */
        if (!a3) {
            return -LX_EINVAL;
        }
        const long *it = (const long *)a3;
        unsigned long interval = (unsigned long)(it[0] * 100 + it[1] / 10000000);
        unsigned long first_s = (unsigned long)it[2];
        unsigned long first_n = (unsigned long)it[3];
        unsigned long first = first_s * 100 + first_n / 10000000;
        if ((first_s || first_n) && first == 0) {
            first = 1; /* sooner than a tick is the next tick */
        }
        if (a2 & 1 /* TFD_TIMER_ABSTIME */) {
            unsigned long now = __syscall0(SYS_TICKS);
            first = first > now ? first - now : 1;
        }
        if (a4) {
            long *old = (long *)a4;
            unsigned long left = __syscall1(SYS_TIMER_GET, (unsigned long)a1);
            old[0] = (long)((left >> 32) / 100);
            old[1] = (long)(((left >> 32) % 100) * 10000000);
            old[2] = (long)((left & 0xFFFFFFFF) / 100);
            old[3] = (long)(((left & 0xFFFFFFFF) % 100) * 10000000);
        }
        return __syscall3(SYS_TIMER_SET, (unsigned long)a1, first, interval) == QUARK_ERR
                   ? -LX_EINVAL
                   : 0;
    }
    case LX_timerfd_gettime: {
        unsigned long left = __syscall1(SYS_TIMER_GET, (unsigned long)a1);
        if (left == QUARK_ERR) {
            return -LX_EINVAL;
        }
        if (a2) {
            long *out = (long *)a2;
            out[0] = (long)((left >> 32) / 100);
            out[1] = (long)(((left >> 32) % 100) * 10000000);
            out[2] = (long)((left & 0xFFFFFFFF) / 100);
            out[3] = (long)(((left & 0xFFFFFFFF) % 100) * 10000000);
        }
        return 0;
    }

    case LX_setsid:
        /* No sessions here. A terminal's child calls this and then asks for a
           controlling terminal; both are about which process group hears a
           signal, and there are no signals. Answering with the caller's own id
           is what a successful `setsid` looks like. */
        return (long)__syscall0(SYS_GETPID);
    case LX_rt_sigaction:
    case LX_rt_sigprocmask:
        return 0; /* accepted and ignored: musl masks signals during startup */
    case LX_set_robust_list:
    case LX_rseq:
    case LX_prlimit64:
        return -LX_ENOSYS;

    /* The kernel's generator never blocks, so GRND_NONBLOCK, GRND_RANDOM
       and GRND_INSECURE all get the same answer. One kernel call gives at
       most a mebibyte; Linux gives at most this much in one of its own. */
    case LX_getrandom: {
        if ((unsigned long)a3 & ~7ul) {
            return -LX_EINVAL;
        }
        unsigned long want = (unsigned long)a2;
        if (want > 33554431ul) {
            want = 33554431ul;
        }
        unsigned long done = 0;
        while (done < want) {
            unsigned long n = __syscall2(SYS_GETRANDOM, (unsigned long)a1 + done, want - done);
            if (n == QUARK_ERR || n == 0) {
                return done ? (long)done : -LX_EFAULT;
            }
            done += n;
        }
        return (long)done;
    }

    case LX_uname:
        return do_uname((char *)a1);

    case LX_getcwd:
        return __quark_getcwd((char *)a1, (unsigned long)a2);
    case LX_chdir:
        return __quark_chdir((const char *)a1);
    case LX_fchdir:
        return __quark_fchdir(a1);

    case LX_getdents64:
        return __quark_getdents(a1, (void *)a2, (unsigned long)a3);
    case LX_readlink:
        return __quark_readlink(LX_AT_FDCWD, (const char *)a1, (char *)a2, (unsigned long)a3);
    case LX_readlinkat:
        return __quark_readlink(a1, (const char *)a2, (char *)a3, (unsigned long)a4);
    case LX_statfs:
        return __quark_statfs((const char *)a1, (void *)a2);
    case LX_fstatfs:
        return __quark_fstatfs(a1, (void *)a2);

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
        return __quark_stat(LX_AT_FDCWD, (const char *)a1, (void *)a2, 1);
    case LX_lstat:
        return __quark_stat(LX_AT_FDCWD, (const char *)a1, (void *)a2, 0);
    case LX_newfstatat:
        /* AT_NO_AUTOMOUNT (0x800) asks for nothing here: nothing mounts. */
        if (a4 & ~(LX_AT_SYMLINK_NOFOLLOW | LX_AT_EMPTY_PATH | 0x800)) {
            return -LX_EINVAL;
        }
        if (!a2 || !*(const char *)a2) {
            if (!(a4 & LX_AT_EMPTY_PATH)) {
                return -LX_ENOENT;
            }
            /* The descriptor itself, which may be the working directory. */
            if (a1 == LX_AT_FDCWD) {
                return __quark_stat(LX_AT_FDCWD, ".", (void *)a3, 1);
            }
            return __quark_fstat(a1, (void *)a3);
        }
        return __quark_stat(a1, (const char *)a2, (void *)a3, !(a4 & LX_AT_SYMLINK_NOFOLLOW));

    /* access(2). gnulib's euidaccess tries faccessat2 first, then faccessat,
       and reports whatever the last one said — so refusing these is not a
       missing convenience: `sort /etc/passwd` says "cannot read" about a file
       it can read perfectly well. All three ask the same question, from the
       working directory or a directory descriptor. */
    case LX_access:
        return __quark_access(LX_AT_FDCWD, (const char *)a1, a2);
    case LX_faccessat:
    case LX_faccessat2:
        return __quark_access(a1, (const char *)a2, a3);

    case LX_fcntl:
        return __quark_fcntl(a1, a2, a3);
    case LX_flock:
        return __quark_flock(a1, a2);
    case LX_pread64:
        return __quark_pread(a1, (void *)a2, (unsigned long)a3, a4);
    case LX_pwrite64:
        return __quark_pwrite(a1, (const void *)a2, (unsigned long)a3, a4);
    case LX_dup:
        return __quark_dup(a1, -1);
    case LX_dup2:
        if (a2 < 0) {
            return -LX_EBADF;
        }
        return __quark_dup(a1, a2);
    /* dup3 differs in refusing a copy onto itself, and in the one flag it
       takes, which means nothing here: nothing execs. */
    case LX_dup3:
        if (a1 == a2 || (a3 & ~LX_O_CLOEXEC)) {
            return -LX_EINVAL;
        }
        if (a2 < 0) {
            return -LX_EBADF;
        }
        return __quark_dup(a1, a2);

    /* The mode is the server's to choose: it makes every directory 0755 for
       the caller, which is what a umask of 022 would leave of most requests. */
    case LX_mkdir:
        return __quark_mkdir(LX_AT_FDCWD, (const char *)a1);
    case LX_mkdirat:
        return __quark_mkdir(a1, (const char *)a2);

    case LX_unlink:
        return __quark_unlink(LX_AT_FDCWD, (const char *)a1);
    case LX_rmdir:
        return __quark_rmdir(LX_AT_FDCWD, (const char *)a1);
    case LX_unlinkat:
        if (a3 & ~LX_AT_REMOVEDIR) {
            return -LX_EINVAL;
        }
        return (a3 & LX_AT_REMOVEDIR) ? __quark_rmdir(a1, (const char *)a2)
                                       : __quark_unlink(a1, (const char *)a2);
    case LX_rename:
        return __quark_rename(LX_AT_FDCWD, (const char *)a1, LX_AT_FDCWD, (const char *)a2);
    case LX_renameat:
    case LX_renameat2:
        /* renameat2's flags (no-replace, exchange) are not offered. */
        if (n == LX_renameat2 && a5 != 0) {
            return -LX_EINVAL;
        }
        return __quark_rename(a1, (const char *)a2, a3, (const char *)a4);
    case LX_link:
        return __quark_link(LX_AT_FDCWD, (const char *)a1, LX_AT_FDCWD, (const char *)a2, 0);
    case LX_linkat:
        /* Naming the source by descriptor (AT_EMPTY_PATH) is not offered. */
        if (a5 & ~LX_AT_SYMLINK_FOLLOW) {
            return -LX_EINVAL;
        }
        return __quark_link(a1, (const char *)a2, a3, (const char *)a4,
                            (a5 & LX_AT_SYMLINK_FOLLOW) != 0);
    case LX_symlink:
        return __quark_symlink((const char *)a1, LX_AT_FDCWD, (const char *)a2);
    case LX_symlinkat:
        return __quark_symlink((const char *)a1, a2, (const char *)a3);
    case LX_truncate:
        return __quark_truncate((const char *)a1, a2);

    /* Streams, descriptors in flight, and waiting. Each is the same operation
       Quark has with Linux's packaging around it. */
    case LX_memfd_create:
        return __quark_memfd((const char *)a1, a2);
    case LX_ftruncate:
        if (a1 >= LX_FIRST_FILE_FD) {
            return __quark_file_truncate(a1, a2);
        }
        return __quark_ftruncate(a1, a2);
    case LX_fallocate:
        /* posix_fallocate(fd, offset, len) is what musl uses when it has it,
           and libwayland's os_create_anonymous_file prefers it to ftruncate.
           There is nothing to preallocate here -- a region's frames are taken
           when it is sized -- so the size is all of it. */
        return __quark_ftruncate(a1, a3 + a4);
    case LX_eventfd:
    case LX_eventfd2: {
        /* A counter with a descriptor. glib reaches for this before anything
           else to wake a sleeping main loop, falling back to a pipe only when
           it fails — and so do libwayland's loop and GTK's. Two of the flags
           are ours to act on: EFD_NONBLOCK, which the read path reads, and
           EFD_SEMAPHORE, which the kernel's counter implements. EFD_CLOEXEC
           means nothing here, as it does for a pipe. */
        long flags = (n == LX_eventfd2) ? a2 : 0;
        unsigned long fd = __syscall2(SYS_EVENT_CREATE, (unsigned long)a1,
                                      (flags & LX_EFD_SEMAPHORE) ? 1UL : 0UL);
        if (fd == QUARK_ERR) {
            return -LX_EMFILE;
        }
        if (flags & LX_EFD_NONBLOCK) {
            __quark_fd_set_nonblock((long)fd, 1);
        }
        return (long)fd;
    }
    case LX_pipe:
        return __quark_pipe((int *)a1, 0);
    case LX_pipe2:
        return __quark_pipe((int *)a1, a2);
    case LX_socketpair:
        return __quark_socketpair(a1, a2, a3, (int *)a4);
    case LX_sendmsg:
        return __quark_sendmsg(a1, (const void *)a2, a3);
    case LX_recvmsg:
        return __quark_recvmsg(a1, (void *)a2, a3);
    case LX_poll:
        return __quark_poll((void *)a1, a2, a3);
    case LX_ppoll:
        /* The signal mask is the only difference and there are no handlers
           here, so the timeout is the whole of it: a timespec rather than
           milliseconds. */
        if (a3) {
            const long *ts = (const long *)a3;
            return __quark_poll((void *)a1, a2, ts[0] * 1000 + ts[1] / 1000000);
        }
        return __quark_poll((void *)a1, a2, -1);
    case LX_epoll_create1:
        return __quark_epoll_create();
    case LX_epoll_ctl:
        return __quark_epoll_ctl(a1, a2, a3, (void *)a4);
    case LX_epoll_wait:
        return __quark_epoll_wait(a1, (void *)a2, a3, a4);

    /* statx carries more than the VFS knows, and musl falls back to the plain
       stat calls when it is refused. */
    case LX_statx:
        return -LX_ENOSYS;

    default:
        trace("nosys", n);
        return -LX_ENOSYS;
    }
}
