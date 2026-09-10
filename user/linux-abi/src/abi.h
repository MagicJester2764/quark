/* Errors in the values Linux uses, because they are what musl compares
 * against. Shared between the dispatcher and the parts it dispatches to.
 */
#ifndef _QUARK_LINUX_ABI_H
#define _QUARK_LINUX_ABI_H

#define LX_EPERM     1
#define LX_ENOENT    2
#define LX_EBADF     9
#define LX_ENOMEM   12
#define LX_EFAULT   14
#define LX_EINVAL   22
#define LX_EMFILE   24
#define LX_ENOTTY   25
#define LX_ESPIPE   29
#define LX_EROFS    30
#define LX_ENOSYS   38
#define LX_EACCES   13
#define LX_EIO       5
#define LX_ENOTDIR  20
#define LX_EISDIR   21
#define LX_ENODEV   19

/* There is no working directory anywhere in this system, so this is the only
   value the *at calls accept for one. */
#define LX_AT_FDCWD (-100)

/* Files, implemented in files.c. Each returns a Linux-style result: a count
   or a value on success, and a negated errno on failure. */
long __quark_open(const char *path, long flags);
long __quark_openat(long dirfd, const char *path, long flags);
long __quark_close(long fd);
long __quark_read(long fd, void *buf, unsigned long n);
long __quark_write(long fd, const void *buf, unsigned long n);
long __quark_lseek(long fd, long offset, long whence);
long __quark_fstat(long fd, void *statbuf);
long __quark_stat(const char *path, void *statbuf);
long __quark_access(const char *path, long mode);
long __quark_kernel_fd_candidate(int nth);

/* Streams, descriptor passing and waiting, in net.c. */
long __quark_memfd(const char *name, long flags);
long __quark_socketpair(long domain, long type, long protocol, int *sv);
long __quark_sendmsg(long fd, const void *msg, long flags);
long __quark_recvmsg(long fd, void *msg, long flags);
long __quark_poll(void *fds, long nfds, long timeout_ms);
long __quark_epoll_create(void);
long __quark_epoll_ctl(long epfd, long op, long fd, void *event);
long __quark_epoll_wait(long epfd, void *events, long maxevents, long timeout_ms);

#endif
