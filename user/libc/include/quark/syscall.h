/* Raw Quark system calls.
 *
 * The numbers come from docs/abi.md, which is the contract: a libc is a
 * consumer of that ABI exactly as the Rust runtime is, and neither is
 * privileged over the other.
 */
#ifndef _QUARK_SYSCALL_H
#define _QUARK_SYSCALL_H

#include <stddef.h>

/* 0x00  process */
#define SYS_EXIT            0
#define SYS_EXIT_CODE       1
#define SYS_YIELD           2
#define SYS_GETPID          3
#define SYS_WAIT            4

/* 0x10  IPC */
#define SYS_SEND            16
#define SYS_RECV            17
#define SYS_CALL            18
#define SYS_REPLY           19

/* 0x20  memory */
#define SYS_MMAP            32
#define SYS_MUNMAP          33
#define SYS_PHYS_ALLOC      34
#define SYS_MAP_PHYS        38
#define SYS_MMAP_FD         42

/* 0x40  file descriptors */
#define SYS_FD_READ         64
#define SYS_FD_WRITE        65
#define SYS_FD_CLOSE        71
#define SYS_SOCKETPAIR      72
#define SYS_FD_SEND         73
#define SYS_FD_RECV         74
#define SYS_POLLSET_CREATE  75
#define SYS_POLLSET_CTL     76
#define SYS_POLLSET_WAIT    77
#define SYS_POLL            78

/* 0x30  shared memory */
#define SYS_MEMFD_CREATE    53

/* 0x60  task */
#define SYS_SET_FS_BASE     102

/* 0x80  futex */
#define SYS_FUTEX_WAIT      128
#define SYS_FUTEX_WAKE      129

/* 0x90  time */
#define SYS_TICKS           144

/* 0xA0  kernel console */
#define SYS_WRITE           160

/* 0xF0  introspection */
#define SYS_ABI_VERSION     240

/* What the kernel returns for "no". Not an errno: each call says what it
   means, and the wrappers here translate. */
#define QUARK_ERR ((unsigned long)-1)

/* The system call wrappers.
 *
 * The kernel's entry path does not preserve the argument registers: it
 * shuffles them into the C ABI its dispatcher expects and leaves rdi, rsi,
 * rdx, r8, r9 and r10 as whatever that left behind. So every one of them is
 * declared written, not just the rcx and r11 the `syscall` instruction itself
 * takes. Getting this wrong does not fail near the call — it fails wherever
 * the compiler had chosen to keep a live value, which was `&free_list` inside
 * malloc the first time.
 *
 * Argument registers are `+` rather than inputs for the same reason: an input
 * cannot also be a clobber, and these are both.
 */
#define __SYSCALL_CLOBBERS "rcx", "r11", "memory"

static inline unsigned long __syscall0(unsigned long n) {
    unsigned long r;
    __asm__ volatile("syscall"
                     : "=a"(r)
                     : "a"(n)
                     : "rdi", "rsi", "rdx", "r8", "r9", "r10", __SYSCALL_CLOBBERS);
    return r;
}
static inline unsigned long __syscall1(unsigned long n, unsigned long a) {
    unsigned long r;
    __asm__ volatile("syscall"
                     : "=a"(r), "+D"(a)
                     : "a"(n)
                     : "rsi", "rdx", "r8", "r9", "r10", __SYSCALL_CLOBBERS);
    return r;
}
static inline unsigned long __syscall2(unsigned long n, unsigned long a, unsigned long b) {
    unsigned long r;
    __asm__ volatile("syscall"
                     : "=a"(r), "+D"(a), "+S"(b)
                     : "a"(n)
                     : "rdx", "r8", "r9", "r10", __SYSCALL_CLOBBERS);
    return r;
}
static inline unsigned long __syscall3(unsigned long n, unsigned long a, unsigned long b,
                                       unsigned long c) {
    unsigned long r;
    __asm__ volatile("syscall"
                     : "=a"(r), "+D"(a), "+S"(b), "+d"(c)
                     : "a"(n)
                     : "r8", "r9", "r10", __SYSCALL_CLOBBERS);
    return r;
}
/* arg3 travels in r10, not rcx: the syscall instruction overwrites rcx with
   the return address before the kernel ever sees it. */
static inline unsigned long __syscall4(unsigned long n, unsigned long a, unsigned long b,
                                       unsigned long c, unsigned long d) {
    unsigned long r;
    register unsigned long r10 __asm__("r10") = d;
    __asm__ volatile("syscall"
                     : "=a"(r), "+D"(a), "+S"(b), "+d"(c), "+r"(r10)
                     : "a"(n)
                     : "r8", "r9", __SYSCALL_CLOBBERS);
    return r;
}

/* arg4 travels in r8. */
static inline unsigned long __syscall5(unsigned long n, unsigned long a, unsigned long b,
                                       unsigned long c, unsigned long d, unsigned long e) {
    unsigned long r;
    register unsigned long r10 __asm__("r10") = d;
    register unsigned long r8 __asm__("r8") = e;
    __asm__ volatile("syscall"
                     : "=a"(r), "+D"(a), "+S"(b), "+d"(c), "+r"(r10), "+r"(r8)
                     : "a"(n)
                     : "r9", __SYSCALL_CLOBBERS);
    return r;
}

/* A fixed-size IPC message: the only shape the kernel carries. */
struct quark_msg {
    unsigned long sender;
    unsigned long tag;
    unsigned long data[6];
};

/* Send and wait for the reply. Returns 0, or -1 if the call could not be made. */
int quark_call(size_t dest, const struct quark_msg *msg, struct quark_msg *reply);

/* Find a service by name. Returns its task ID, or 0 if there is none. */
size_t quark_lookup(const char *name);

/* The task ID of the VFS, looked up once and remembered. 0 if there is none. */
size_t quark_vfs(void);

#endif
