/* Errors in the values Linux uses, because they are what musl compares
 * against. Shared between the dispatcher and the parts it dispatches to.
 */
#ifndef _QUARK_LINUX_ABI_H
#define _QUARK_LINUX_ABI_H

#define LX_EPERM     1
#define LX_ENOENT    2
#define LX_EBADF     9
#define LX_ENOMEM   12
#define LX_ENOEXEC   8
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
#define LX_EAGAIN   11
#define LX_ECHILD   10
#define LX_EEXIST   17
#define LX_EXDEV    18
#define LX_ENAMETOOLONG 36
#define LX_ENOTEMPTY 39
#define LX_EOPNOTSUPP 95
#define LX_ERANGE   34
#define LX_ENOSPC   28
#define LX_EMLINK   31
#define LX_ELOOP    40
#define LX_EDEADLK  35
#define LX_ENOLCK   37
#define LX_ETIMEDOUT 110

/* Descriptors from here up are files.c's VFS files; below, the kernel's.
   The split sits at the kernel's MAX_FDS — see files.c. */
#define LX_FIRST_FILE_FD 32

/* There is no working directory anywhere in this system, so this is the only
   value the *at calls accept for one. */
#define LX_AT_FDCWD (-100)
#define LX_AT_SYMLINK_FOLLOW 0x400
#define LX_AT_SYMLINK_NOFOLLOW 0x100
#define LX_AT_EMPTY_PATH 0x1000

/* eventfd's flags. EFD_CLOEXEC is accepted and ignored, like O_CLOEXEC. */
#define LX_EFD_SEMAPHORE 1
#define LX_EFD_NONBLOCK  0x800

/* Open flags this layer acts on. Shared because `pipe2` and `fcntl` have to
   agree about what O_NONBLOCK means. */
#define LX_O_NONBLOCK  04000
#define LX_O_RDWR      2

/* Files, implemented in files.c. Each returns a Linux-style result: a count
   or a value on success, and a negated errno on failure. */
long __quark_open(const char *path, long flags);
long __quark_openat(long dirfd, const char *path, long flags);
long __quark_close(long fd);
long __quark_read(long fd, void *buf, unsigned long n);
long __quark_write(long fd, const void *buf, unsigned long n);
long __quark_lseek(long fd, long offset, long whence);
long __quark_fstat(long fd, void *statbuf);
long __quark_stat(long dirfd, const char *path, void *statbuf, int follow);
long __quark_access(long dirfd, const char *path, long mode);
long __quark_mkdir(long dirfd, const char *path);
long __quark_unlink(long dirfd, const char *path);
long __quark_rmdir(long dirfd, const char *path);
long __quark_rename(long fromfd, const char *from, long tofd, const char *to);
long __quark_link(long fromfd, const char *from, long tofd, const char *to, int follow);
long __quark_symlink(const char *target, long dirfd, const char *path);
long __quark_chdir(const char *path);
long __quark_flock(long fd, long op);
long __quark_file_map(long fd, int write_shared, unsigned long *cap);
long __quark_fchdir(long fd);
long __quark_getcwd(char *buf, unsigned long size);
long __quark_truncate(const char *path, long length);
long __quark_file_truncate(long fd, long length);
long __quark_getdents(long fd, void *buf, unsigned long count);
long __quark_dup(long fd, long to);
long __quark_pread(long fd, void *buf, unsigned long n, long offset);
long __quark_pwrite(long fd, const void *buf, unsigned long n, long offset);
long __quark_readlink(long dirfd, const char *path, char *buf, unsigned long size);
long __quark_statfs(const char *path, void *buf);
long __quark_fstatfs(long fd, void *buf);

/* Streams, descriptor passing and waiting, in net.c. */
long __quark_memfd(const char *name, long flags);
long __quark_execve(const char *path, char *const argv[], char *const envp[]);
long __quark_pty_open(const char *path);
int __quark_pty_path(const char *path);
long __quark_ioctl(long fd, unsigned long request, unsigned long arg);
long __quark_ftruncate(long fd, long length);
long __quark_fcntl(long fd, long cmd, long arg);
int __quark_fd_is_nonblock(long fd);
void __quark_fd_set_nonblock(long fd, int on);
long __quark_pipe(int *fds, long flags);
long __quark_socketpair(long domain, long type, long protocol, int *sv);
long __quark_sendmsg(long fd, const void *msg, long flags);
long __quark_recvmsg(long fd, void *msg, long flags);
long __quark_poll(void *fds, long nfds, long timeout_ms);
long __quark_epoll_create(void);
long __quark_epoll_ctl(long epfd, long op, long fd, void *event);
long __quark_epoll_wait(long epfd, void *events, long maxevents, long timeout_ms);

#endif
