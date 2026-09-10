/* Linux's shapes for a stream, a descriptor in flight, and waiting.
 *
 * Quark's calls are the same operations with the arguments in a different
 * order and none of the packaging, so most of this file is unwrapping: a
 * `struct msghdr` into a pointer and a length, a `cmsghdr` into one descriptor
 * number, a `struct pollfd` array into Quark's.
 *
 * The one piece of judgement is SCM_RIGHTS carrying more than one descriptor.
 * Quark's send takes one, Wayland sends one per message, and silently
 * delivering the first of three would be found somewhere else entirely — so
 * more than one is refused.
 */

#include <quark/syscall.h>

#include "abi.h"

typedef long ssize_t;
typedef unsigned long size_t;

#define NULL ((void *)0)

struct iovec {
    void *iov_base;
    unsigned long iov_len;
};

struct msghdr {
    void *msg_name;
    unsigned int msg_namelen;
    struct iovec *msg_iov;
    unsigned long msg_iovlen;
    void *msg_control;
    unsigned long msg_controllen;
    int msg_flags;
};

struct cmsghdr {
    unsigned long cmsg_len;
    int cmsg_level;
    int cmsg_type;
};

#define SOL_SOCKET  1
#define SCM_RIGHTS  1

/* The only message flags that change what a call does here. MSG_NOSIGNAL is
 * ignored on purpose: there are no signals, so a broken stream is already an
 * error return rather than a death. */
#define MSG_DONTWAIT 0x40


#define AF_UNIX      1
#define SOCK_STREAM  1

/* Linux's poll bits, which are not Quark's. */
#define LX_POLLIN   0x001
#define LX_POLLOUT  0x004
#define LX_POLLERR  0x008
#define LX_POLLHUP  0x010
#define LX_POLLNVAL 0x020

struct lx_pollfd {
    int fd;
    short events;
    short revents;
};

/* Quark's, from the ABI: 1 readable, 2 writable, 4 hangup, 8 invalid. */
struct qw_pollfd {
    unsigned int fd;
    unsigned int events;
    unsigned int revents;
    unsigned int pad;
};

#define QW_READABLE 1
#define QW_WRITABLE 2
#define QW_HANGUP   4
#define QW_INVALID  8

#define MAX_POLL 32

long __quark_memfd(const char *name, long flags) {
    (void)name;  /* Linux keeps it for /proc; there is no /proc here. */
    (void)flags;
    /* One page, because the size comes from the ftruncate that follows -- and
       on Linux it must, since memfd_create takes no size at all. */
    unsigned long fd = __syscall1(SYS_MEMFD_CREATE, 1);
    if (fd == QUARK_ERR) {
        return -LX_ENOMEM;
    }
    return (long)fd;
}

/* Set the size of memory named by a descriptor.
 *
 * Only memory: a file's size is the VFS's business and nothing here can change
 * it, so a file earns EINVAL rather than a lie about having been resized. The
 * memory case is the one that matters, because `memfd_create` then `ftruncate`
 * then `mmap` is how every Wayland client makes its buffer pool. */
long __quark_ftruncate(long fd, long length) {
    if (length < 0) {
        return -LX_EINVAL;
    }
    unsigned long r = __syscall2(SYS_MEMFD_TRUNCATE, (unsigned long)fd,
                                 (unsigned long)length);
    return r == QUARK_ERR ? -LX_EINVAL : 0;
}

long __quark_socketpair(long domain, long type, long protocol, int *sv) {
    /* Only a local stream, which is what a Wayland connection is. */
    if (domain != AF_UNIX || (type & 0xF) != SOCK_STREAM || protocol != 0) {
        return -LX_ENOSYS;
    }
    if (!sv) {
        return -LX_EFAULT;
    }
    unsigned long r = __syscall0(SYS_SOCKETPAIR);
    if (r == QUARK_ERR) {
        return -LX_EMFILE;
    }
    sv[0] = (int)(r >> 32);
    sv[1] = (int)(r & 0xFFFFFFFF);
    return 0;
}

/* The single descriptor a control message carries, or -1. */
static long control_fd(const struct msghdr *m, int *too_many) {
    *too_many = 0;
    if (!m->msg_control || m->msg_controllen < sizeof(struct cmsghdr)) {
        return -1;
    }
    const struct cmsghdr *c = m->msg_control;
    if (c->cmsg_level != SOL_SOCKET || c->cmsg_type != SCM_RIGHTS) {
        return -1;
    }
    unsigned long payload = c->cmsg_len - sizeof(struct cmsghdr);
    if (payload < sizeof(int)) {
        return -1;
    }
    if (payload > sizeof(int)) {
        *too_many = 1;
        return -1;
    }
    const int *fds = (const int *)((const char *)c + sizeof(struct cmsghdr));
    return fds[0];
}

long __quark_sendmsg(long fd, const void *msg, long flags) {
    /* Either the call or the descriptor may ask not to wait, and both mean the
       same thing to the kernel. */
    unsigned long fl = ((flags & MSG_DONTWAIT) || __quark_fd_is_nonblock(fd))
                           ? QUARK_DONTWAIT
                           : 0;
    const struct msghdr *m = msg;
    if (!m) {
        return -LX_EFAULT;
    }
    int too_many = 0;
    long pass = control_fd(m, &too_many);
    if (too_many) {
        /* Refusing is better than delivering the first and losing the rest,
           which would be found somewhere else entirely. */
        return -LX_EINVAL;
    }

    long total = 0;
    for (unsigned long i = 0; i < m->msg_iovlen; i++) {
        const struct iovec *v = &m->msg_iov[i];
        if (v->iov_len == 0) {
            continue;
        }
        /* The descriptor rides with the first piece that carries bytes, so it
           is queued before anything the peer can read. */
        unsigned long attach = (total == 0 && pass >= 0) ? (unsigned long)pass : QUARK_ERR;
        unsigned long w = __syscall5(SYS_FD_SEND, (unsigned long)fd,
                                     (unsigned long)v->iov_base, v->iov_len, attach, fl);
        if (w == QUARK_ERR) {
            return total ? total : -LX_EIO;
        }
        if (w == QUARK_WOULD_BLOCK) {
            return total ? total : -LX_EAGAIN;
        }
        total += (long)(w & 0xFFFFFFFF);
        if ((unsigned long)(w & 0xFFFFFFFF) < v->iov_len) {
            break;
        }
    }
    /* A control message with no data still has to hand the descriptor over. */
    if (total == 0 && pass >= 0) {
        unsigned long w = __syscall5(SYS_FD_SEND, (unsigned long)fd, 0, 0,
                                     (unsigned long)pass, fl);
        if (w == QUARK_ERR) {
            return -LX_EIO;
        }
    }
    return total;
}

long __quark_recvmsg(long fd, void *msg, long flags) {
    unsigned long fl = ((flags & MSG_DONTWAIT) || __quark_fd_is_nonblock(fd))
                           ? QUARK_DONTWAIT
                           : 0;
    struct msghdr *m = msg;
    if (!m) {
        return -LX_EFAULT;
    }

    /* Where a received descriptor should land. Linux hands back a number it
       chose, and so does Quark when asked with QUARK_ANY_FD — which is the
       only workable answer, because this layer cannot see the kernel's half
       of the table and probing for a free slot would mean reading, and
       reading is the thing recvmsg must do exactly once. */
    int room = m->msg_control && m->msg_controllen >= sizeof(struct cmsghdr) + sizeof(int);

    long total = 0;
    long landed_at = -1;
    for (unsigned long i = 0; i < m->msg_iovlen; i++) {
        struct iovec *v = &m->msg_iov[i];
        if (v->iov_len == 0) {
            continue;
        }
        /* Only the first read may collect a descriptor: one control message
           carries one, and a later piece asking for another would take the
           next sender's. */
        unsigned long at = (total == 0 && room && landed_at < 0)
                               ? QUARK_ANY_FD
                               : QUARK_ERR;
        unsigned long r = __syscall5(SYS_FD_RECV, (unsigned long)fd,
                                     (unsigned long)v->iov_base, v->iov_len, at, fl);
        if (r == QUARK_ERR) {
            return total ? total : -LX_EIO;
        }
        if (r == QUARK_WOULD_BLOCK) {
            /* Nothing there. A caller that loops until this happens -- which
               is what libwayland does -- is the reason MSG_DONTWAIT cannot be
               ignored: the last turn of the loop is always this one. */
            if (total) {
                break;
            }
            m->msg_controllen = 0;
            return -LX_EAGAIN;
        }
        if (r >> 32) {
            landed_at = (long)(r >> 32) - 1;
        }
        unsigned long n = r & 0xFFFFFFFF;
        total += (long)n;
        if (n < v->iov_len) {
            break;
        }
    }
    int got_fd = landed_at >= 0;
    long want_at = landed_at;

    if (got_fd) {
        struct cmsghdr *c = m->msg_control;
        c->cmsg_len = sizeof(struct cmsghdr) + sizeof(int);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        *(int *)((char *)c + sizeof(struct cmsghdr)) = (int)want_at;
        m->msg_controllen = c->cmsg_len;
    } else {
        m->msg_controllen = 0;
    }
    m->msg_flags = 0;
    return total;
}

long __quark_poll(void *fds, long nfds, long timeout_ms) {
    struct lx_pollfd *p = fds;
    if (!p || nfds < 0) {
        return -LX_EFAULT;
    }
    if (nfds > MAX_POLL) {
        return -LX_EINVAL;
    }

    struct qw_pollfd q[MAX_POLL];
    for (long i = 0; i < nfds; i++) {
        q[i].fd = (unsigned int)p[i].fd;
        q[i].events = 0;
        q[i].revents = 0;
        q[i].pad = 0;
        if (p[i].events & LX_POLLIN) {
            q[i].events |= QW_READABLE;
        }
        if (p[i].events & LX_POLLOUT) {
            q[i].events |= QW_WRITABLE;
        }
    }

    /* The PIT is 100 Hz, so a tick is ten milliseconds. A negative timeout
       means wait for ever, which here is as long as the counter allows. */
    unsigned long ticks;
    if (timeout_ms < 0) {
        ticks = 0xFFFFFFFFUL;
    } else {
        ticks = ((unsigned long)timeout_ms + 9) / 10;
    }

    unsigned long n = __syscall3(SYS_POLL, (unsigned long)q, (unsigned long)nfds, ticks);
    if (n == QUARK_ERR) {
        return -LX_EINVAL;
    }
    for (long i = 0; i < nfds; i++) {
        short rev = 0;
        if (q[i].revents & QW_READABLE) {
            rev |= LX_POLLIN;
        }
        if (q[i].revents & QW_WRITABLE) {
            rev |= LX_POLLOUT;
        }
        if (q[i].revents & QW_HANGUP) {
            rev |= LX_POLLHUP;
        }
        if (q[i].revents & QW_INVALID) {
            rev |= LX_POLLNVAL;
        }
        p[i].revents = rev;
    }
    return (long)n;
}

long __quark_epoll_create(void) {
    unsigned long fd = __syscall0(SYS_POLLSET_CREATE);
    if (fd == QUARK_ERR) {
        return -LX_EMFILE;
    }
    return (long)fd;
}

/* Linux's struct epoll_event is packed on x86-64: a u32 of events and eight
   bytes of user data with no padding between them. */
struct lx_epoll_event {
    unsigned int events;
    unsigned long long data;
} __attribute__((packed));

#define LX_EPOLL_CTL_ADD 1
#define LX_EPOLL_CTL_DEL 2
#define LX_EPOLL_CTL_MOD 3

long __quark_epoll_ctl(long epfd, long op, long fd, void *event) {
    struct lx_epoll_event *e = event;
    unsigned long qop;
    switch (op) {
    case LX_EPOLL_CTL_ADD: qop = 0; break;
    case LX_EPOLL_CTL_MOD: qop = 1; break;
    case LX_EPOLL_CTL_DEL: qop = 2; break;
    default: return -LX_EINVAL;
    }

    unsigned long events = 0;
    unsigned long token = 0;
    if (qop != 2) {
        if (!e) {
            return -LX_EFAULT;
        }
        if (e->events & LX_POLLIN) {
            events |= QW_READABLE;
        }
        if (e->events & LX_POLLOUT) {
            events |= QW_WRITABLE;
        }
        token = (unsigned long)e->data;
    }

    unsigned long r = __syscall5(SYS_POLLSET_CTL, (unsigned long)epfd, qop,
                                 (unsigned long)fd, events, token);
    return r == QUARK_ERR ? -LX_EINVAL : 0;
}

struct qw_ready {
    unsigned long token;
    unsigned int events;
    unsigned int pad;
};

long __quark_epoll_wait(long epfd, void *events, long maxevents, long timeout_ms) {
    struct lx_epoll_event *out = events;
    if (!out || maxevents <= 0) {
        return -LX_EINVAL;
    }
    if (maxevents > MAX_POLL) {
        maxevents = MAX_POLL;
    }

    struct qw_ready ready[MAX_POLL];
    unsigned long ticks;
    if (timeout_ms < 0) {
        ticks = 0xFFFFFFFFUL;
    } else {
        ticks = ((unsigned long)timeout_ms + 9) / 10;
    }

    unsigned long n = __syscall4(SYS_POLLSET_WAIT, (unsigned long)epfd,
                                 (unsigned long)ready, (unsigned long)maxevents, ticks);
    if (n == QUARK_ERR) {
        return -LX_EINVAL;
    }
    for (unsigned long i = 0; i < n; i++) {
        unsigned int ev = 0;
        if (ready[i].events & QW_READABLE) {
            ev |= LX_POLLIN;
        }
        if (ready[i].events & QW_WRITABLE) {
            ev |= LX_POLLOUT;
        }
        if (ready[i].events & QW_HANGUP) {
            ev |= LX_POLLHUP;
        }
        out[i].events = ev;
        out[i].data = ready[i].token;
    }
    return (long)n;
}
