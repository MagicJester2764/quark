/* The paths and the requests a terminal uses to get a pseudo-terminal.
 *
 * musl's `openpty` opens `/dev/ptmx`, asks it which pty it was given
 * (`TIOCGPTN`), unlocks it (`TIOCSPTLCK`) and opens `/dev/pts/N`. None of
 * those are files: a pty is a pair of kernel descriptors, so the paths are
 * caught here, ahead of the VFS, the way a Linux kernel catches them ahead of
 * its filesystems.
 *
 * `TIOCSPTLCK` is accepted and does nothing. Linux locks a slave until its
 * master says otherwise, so that nobody opens the slave of a pty that is
 * mid-allocation; here a slave can only be opened by number and only while its
 * master is held, which is the same guarantee arrived at from the other side.
 */

#include <quark/syscall.h>

#include "abi.h"

/* `SYS_PTY_CTL` operations, as the kernel numbers them. */
#define PTY_GET_TERMIOS 0
#define PTY_SET_TERMIOS 1
#define PTY_GET_WINSIZE 2
#define PTY_SET_WINSIZE 3
#define PTY_NUMBER      4

/* The requests a terminal emulator and a shell actually send. */
#define TCGETS     0x5401
#define TCSETS     0x5402
#define TCSETSW    0x5403
#define TCSETSF    0x5404
#define TIOCSCTTY  0x540E
#define TIOCGWINSZ 0x5413
#define TIOCSWINSZ 0x5414
#define TIOCGPTN   0x80045430
#define TIOCSPTLCK 0x40045431

static int streq(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

/* "/dev/pts/12" -> 12, or -1 for anything else. */
static long pts_number(const char *path) {
    const char *p = path;
    const char *want = "/dev/pts/";
    while (*want) {
        if (*p != *want) {
            return -1;
        }
        p++;
        want++;
    }
    if (!*p) {
        return -1;
    }
    long n = 0;
    for (; *p; p++) {
        if (*p < '0' || *p > '9') {
            return -1;
        }
        n = n * 10 + (*p - '0');
        if (n > 4096) {
            return -1;
        }
    }
    return n;
}

/* Whether this path is a terminal rather than a file, and the descriptor for
   it if so. Returns -1 when the path is not one of ours, which is not an
   error: the caller goes on to the VFS. */
long __quark_pty_open(const char *path) {
    if (!path) {
        return -1;
    }
    if (streq(path, "/dev/ptmx")) {
        unsigned long fd = __syscall0(SYS_PTY_CREATE);
        return fd == QUARK_ERR ? -LX_ENOSPC : (long)fd;
    }
    long n = pts_number(path);
    if (n < 0) {
        return -1;
    }
    unsigned long fd = __syscall1(SYS_PTY_OPEN, (unsigned long)n);
    return fd == QUARK_ERR ? -LX_ENOENT : (long)fd;
}

/* `/dev/pts` itself, for a program that stats it before using it. */
int __quark_pty_path(const char *path) {
    return path && (streq(path, "/dev/ptmx") || pts_number(path) >= 0);
}

/* The requests a terminal makes of a descriptor.
 *
 * Anything this does not answer is `ENOTTY`, which is the right answer to a
 * program asking a pipe about its window size — and is how `isatty` tells the
 * two apart. */
long __quark_ioctl(long fd, unsigned long request, unsigned long arg) {
    switch (request) {
    case TCGETS:
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_GET_TERMIOS, arg) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    case TCSETS:
    case TCSETSW:
    case TCSETSF:
        /* The three differ in when they take effect: now, after what has been
           written drains, or after that and with what has been typed thrown
           away. With a buffer this small and no hardware behind it, draining
           is already done and there is nothing to discard that a program has
           not already been given. */
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_SET_TERMIOS, arg) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    case TIOCGWINSZ:
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_GET_WINSIZE, arg) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    case TIOCSWINSZ:
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_SET_WINSIZE, arg) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    case TIOCGPTN: {
        unsigned long n = __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_NUMBER, 0);
        if (n == QUARK_ERR) {
            return -LX_ENOTTY;
        }
        if (arg) {
            *(unsigned int *)arg = (unsigned int)n;
        }
        return 0;
    }
    case TIOCSPTLCK:
        /* Accepted and nothing done: a slave here is openable only by number
           and only while its master is held, which is the guarantee the lock
           exists to give. */
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_NUMBER, 0) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    case TIOCSCTTY:
        /* There are no sessions here, so there is no controlling terminal to
           become. A program asks for one right after `setsid` and checks only
           that it worked; refusing would stop a shell that has done nothing
           wrong. */
        return __syscall3(SYS_PTY_CTL, (unsigned long)fd, PTY_NUMBER, 0) == QUARK_ERR
                   ? -LX_ENOTTY
                   : 0;
    default:
        return -LX_ENOTTY;
    }
}
