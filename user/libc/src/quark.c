/* Talking to the rest of the system.
 *
 * Almost nothing a program wants is a system call on Quark. The kernel does
 * scheduling, memory and message passing; files, the console and the network
 * are services reached by sending a message to a task. So the parts of libc
 * that look like system calls elsewhere are IPC here, and this file is where
 * that translation lives.
 */

#include <quark/syscall.h>
#include <quark/vfs.h>

/* No <string.h>: this file is compiled into the Linux translation layer as
   well, which sits *underneath* a C library and so cannot use one. */
static void zero(void *p, unsigned long n) {
    unsigned char *b = p;
    while (n--) {
        *b++ = 0;
    }
}

static void copy(void *dst, const void *src, unsigned long n) {
    unsigned char *d = dst;
    const unsigned char *s = src;
    while (n--) {
        *d++ = *s++;
    }
}

static unsigned long length(const char *s) {
    unsigned long n = 0;
    while (s[n]) {
        n++;
    }
    return n;
}

/* The nameserver, started first by init so this number is stable. Everything
   else is found by asking it. */
#define NAMESERVER_TID 2
#define TAG_NS_LOOKUP  2

int quark_call(size_t dest, const struct quark_msg *msg, struct quark_msg *reply) {
    unsigned long r = __syscall3(SYS_CALL, (unsigned long)dest, (unsigned long)msg,
                                 (unsigned long)reply);
    return r == QUARK_ERR ? -1 : 0;
}

size_t quark_lookup(const char *name) {
    struct quark_msg msg;
    struct quark_msg reply;
    unsigned char packed[24];

    zero(&msg, sizeof msg);
    zero(packed, sizeof packed);

    size_t len = length(name);
    if (len > sizeof packed) {
        len = sizeof packed;
    }
    copy(packed, name, len);
    copy(msg.data, packed, sizeof packed);
    msg.tag = TAG_NS_LOOKUP;

    if (quark_call(NAMESERVER_TID, &msg, &reply) != 0) {
        return 0;
    }
    /* The nameserver answers with the TID in the tag, and all-ones for "no
       such service" — which is not a task ID anyone could have. */
    if (reply.tag == (unsigned long)-1) {
        return 0;
    }
    return (size_t)reply.tag;
}

static size_t vfs_tid;

size_t quark_vfs(void) {
    /* Services come up alongside their clients, so an answer of "not yet" is
       worth asking about again; an answer of "here it is" is not. */
    if (vfs_tid == 0) {
        vfs_tid = quark_lookup("vfs");
    }
    return vfs_tid;
}

/* ------------------------------------------------------------------------ */
/* The VFS, as a client.                                                     */
/* ------------------------------------------------------------------------ */

int quark_call_lend(size_t dest, const struct quark_msg *msg, struct quark_msg *reply,
                    void *buf, unsigned long len, unsigned long access) {
    unsigned long r = __syscall5(SYS_CALL_LEND, (unsigned long)dest, (unsigned long)msg,
                                 (unsigned long)reply, (unsigned long)buf, len | access);
    return r == QUARK_ERR ? -1 : 0;
}

/* Ask the VFS one question and get its answer, translating "the call did not
   happen" and "the server said no" into the same small integers. `buf`, if
   there is one, is lent to the VFS for the call with `access`. */
static int vfs_call_lend(struct quark_msg *msg, struct quark_msg *reply,
                         void *buf, unsigned long len, unsigned long access) {
    size_t vfs = quark_vfs();
    if (vfs == 0) {
        return QUARK_VFS_UNREACHABLE;
    }
    int r = buf ? quark_call_lend(vfs, msg, reply, buf, len, access)
                : quark_call(vfs, msg, reply);
    if (r != 0) {
        /* A thread holds the capabilities its program held when the thread
           started, not ones gained since, so a call that works from one
           thread can be refused from another. Looking the server up again
           grants this one its own, and finds a server that has restarted. */
        vfs = quark_lookup("vfs");
        if (vfs == 0) {
            return QUARK_VFS_UNREACHABLE;
        }
        vfs_tid = vfs;
        r = buf ? quark_call_lend(vfs, msg, reply, buf, len, access)
                : quark_call(vfs, msg, reply);
        if (r != 0) {
            return QUARK_VFS_UNREACHABLE;
        }
    }
    if (reply->tag == QUARK_ERR) {
        /* The server puts its code in the first word. A zero there would be
           "no error", which it would not have sent, so treat it as I/O. */
        unsigned long code = reply->data[0];
        return code ? (int)code : QUARK_VFS_IO;
    }
    return 0;
}

static int vfs_call(struct quark_msg *msg, struct quark_msg *reply) {
    return vfs_call_lend(msg, reply, 0, 0, 0);
}

/* Ask the VFS something about a path, which is lent with the call. A
   relative path starts from `base`: 0 for the program's working directory,
   or an open directory's handle plus one. */
static int vfs_path_call(unsigned long tag, unsigned long base, const char *path,
                         struct quark_msg *msg, struct quark_msg *reply) {
    unsigned long len = length(path);
    if (len == 0) {
        return QUARK_VFS_INVALID_PATH;
    }
    if (len > QUARK_VFS_MAX_PATH) {
        return QUARK_VFS_NAME_TOO_LONG;
    }
    msg->tag = tag;
    msg->data[0] = len;
    msg->data[5] = base;
    /* Lent for reading only; the cast drops a const the kernel keeps. */
    return vfs_call_lend(msg, reply, (void *)path, len, QUARK_LEND_READ);
}

int quark_vfs_open(const char *path, unsigned long flags, struct quark_vfs_file *out) {
    return quark_vfs_open_at(0, path, flags, out);
}

int quark_vfs_open_at(unsigned long base, const char *path, unsigned long flags,
                      struct quark_vfs_file *out) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.data[1] = flags;
    int err = vfs_path_call(QUARK_VFS_TAG_OPEN, base, path, &msg, &reply);
    if (err) {
        return err;
    }
    if (out) {
        out->handle = reply.data[0];
        out->size = reply.data[1];
        out->is_dir = reply.data[2] != 0;
        out->mode = (unsigned int)reply.data[3];
        out->access = (unsigned int)reply.data[4];
        out->id = reply.data[5];
    }
    return 0;
}

/* A request that names one path and wants nothing back. */
static int vfs_path_only(unsigned long tag, unsigned long base, const char *path) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    return vfs_path_call(tag, base, path, &msg, &reply);
}

int quark_vfs_mkdir(const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_MKDIR, 0, path);
}

int quark_vfs_mkdir_at(unsigned long base, const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_MKDIR, base, path);
}

int quark_vfs_unlink(const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_UNLINK, 0, path);
}

int quark_vfs_unlink_at(unsigned long base, const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_UNLINK, base, path);
}

int quark_vfs_rmdir(const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_RMDIR, 0, path);
}

int quark_vfs_rmdir_at(unsigned long base, const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_RMDIR, base, path);
}

int quark_vfs_chdir(const char *path) {
    return vfs_path_only(QUARK_VFS_TAG_CHDIR, 0, path);
}

int quark_vfs_fchdir(unsigned long handle) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_FCHDIR;
    msg.data[0] = handle;
    return vfs_call(&msg, &reply);
}

long quark_vfs_getcwd(char *out, unsigned long len) {
    struct quark_msg msg;
    struct quark_msg reply;
    char path[QUARK_VFS_MAX_PATH + 1];

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_GETCWD;
    int err = vfs_call_lend(&msg, &reply, path, sizeof path, QUARK_LEND_WRITE);
    if (err) {
        return -err;
    }
    unsigned long n = reply.data[0];
    if (n > sizeof path) {
        return -QUARK_VFS_IO;
    }
    if (n > len) {
        return -QUARK_VFS_NAME_TOO_LONG;
    }
    copy(out, path, n);
    return (long)n;
}

int quark_vfs_lock(unsigned long handle, unsigned long kind, unsigned long start,
                   unsigned long len, unsigned long flags, unsigned long out[4]) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_LOCK;
    msg.data[0] = handle;
    msg.data[1] = kind;
    msg.data[2] = start;
    msg.data[3] = len;
    msg.data[4] = flags;
    int err = vfs_call(&msg, &reply);
    if (!err && out) {
        for (int i = 0; i < 4; i++) {
            out[i] = reply.data[i];
        }
    }
    return err;
}

int quark_vfs_give_cwd(unsigned long child) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_GIVE_CWD;
    msg.data[0] = child;
    return vfs_call(&msg, &reply);
}

/* A request naming two paths, lent in one buffer, one after the other. */
static int vfs_two_paths(unsigned long tag, unsigned long from_base, const char *from,
                         unsigned long to_base, const char *to, unsigned long extra) {
    struct quark_msg msg;
    struct quark_msg reply;
    char both[2 * QUARK_VFS_MAX_PATH];

    unsigned long a = length(from);
    unsigned long b = length(to);
    if (a == 0 || b == 0) {
        return QUARK_VFS_INVALID_PATH;
    }
    if (a > QUARK_VFS_MAX_PATH || b > QUARK_VFS_MAX_PATH) {
        return QUARK_VFS_NAME_TOO_LONG;
    }
    copy(both, from, a);
    copy(both + a, to, b);
    zero(&msg, sizeof msg);
    msg.tag = tag;
    msg.data[0] = a;
    msg.data[1] = b;
    msg.data[2] = extra;
    msg.data[4] = to_base;
    msg.data[5] = from_base;
    return vfs_call_lend(&msg, &reply, both, a + b, QUARK_LEND_READ);
}

int quark_vfs_rename(const char *from, const char *to) {
    return vfs_two_paths(QUARK_VFS_TAG_RENAME, 0, from, 0, to, 0);
}

int quark_vfs_rename_at(unsigned long from_base, const char *from, unsigned long to_base,
                        const char *to) {
    return vfs_two_paths(QUARK_VFS_TAG_RENAME, from_base, from, to_base, to, 0);
}

int quark_vfs_link(const char *from, const char *to, int follow) {
    return vfs_two_paths(QUARK_VFS_TAG_LINK, 0, from, 0, to, follow ? 1 : 0);
}

int quark_vfs_link_at(unsigned long from_base, const char *from, unsigned long to_base,
                      const char *to, int follow) {
    return vfs_two_paths(QUARK_VFS_TAG_LINK, from_base, from, to_base, to, follow ? 1 : 0);
}

int quark_vfs_symlink(const char *target, const char *path) {
    return vfs_two_paths(QUARK_VFS_TAG_SYMLINK, 0, target, 0, path, 0);
}

/* The link's path is the one looked up, and `base` is where it starts. */
int quark_vfs_symlink_at(const char *target, unsigned long base, const char *path) {
    return vfs_two_paths(QUARK_VFS_TAG_SYMLINK, base, target, 0, path, 0);
}

long quark_vfs_readlink(const char *path, char *out, unsigned long len) {
    return quark_vfs_readlink_at(0, path, out, len);
}

long quark_vfs_readlink_at(unsigned long base, const char *path, char *out, unsigned long len) {
    struct quark_msg msg;
    struct quark_msg reply;
    /* The path, then the room for the answer, lent as one buffer. */
    char both[QUARK_VFS_MAX_PATH + 4096];

    unsigned long a = length(path);
    if (a == 0) {
        return -QUARK_VFS_INVALID_PATH;
    }
    if (a > QUARK_VFS_MAX_PATH) {
        return -QUARK_VFS_NAME_TOO_LONG;
    }
    unsigned long room = len < 4096 ? len : 4096;
    copy(both, path, a);
    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_READLINK;
    msg.data[0] = a;
    msg.data[1] = room;
    msg.data[5] = base;
    int err = vfs_call_lend(&msg, &reply, both, a + room, QUARK_LEND_READ | QUARK_LEND_WRITE);
    if (err) {
        return -err;
    }
    unsigned long target = reply.data[0];
    copy(out, both + a, target < room ? target : room);
    return (long)target;
}

int quark_vfs_truncate(unsigned long handle, unsigned long size) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_TRUNCATE;
    msg.data[0] = handle;
    msg.data[1] = size;
    return vfs_call(&msg, &reply);
}

int quark_vfs_readdir(unsigned long handle, unsigned long start, void *buf, unsigned long len,
                      unsigned long *used, unsigned long *next, int *end) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_READDIR;
    msg.data[0] = handle;
    msg.data[1] = start;
    msg.data[2] = len;
    int err = vfs_call_lend(&msg, &reply, buf, len, QUARK_LEND_WRITE);
    if (err) {
        return err;
    }
    *used = reply.data[0] < len ? reply.data[0] : len;
    *next = reply.data[1];
    *end = reply.data[2] != 0;
    return 0;
}

int quark_vfs_statfs(struct quark_vfs_statfs *out) {
    struct quark_msg msg;
    struct quark_msg reply;
    struct quark_vfs_statfs rec;

    zero(&msg, sizeof msg);
    zero(&rec, sizeof rec);
    msg.tag = QUARK_VFS_TAG_STATFS;
    int err = vfs_call_lend(&msg, &reply, &rec, sizeof rec, QUARK_LEND_WRITE);
    if (err) {
        return err;
    }
    copy(out, &rec, sizeof rec);
    return 0;
}

int quark_vfs_stat(unsigned long handle, struct quark_vfs_stat *out) {
    struct quark_msg msg;
    struct quark_msg reply;
    struct quark_vfs_stat rec;

    zero(&msg, sizeof msg);
    zero(&rec, sizeof rec);
    msg.tag = QUARK_VFS_TAG_STAT;
    msg.data[0] = handle;
    int err = vfs_call_lend(&msg, &reply, &rec, sizeof rec, QUARK_LEND_WRITE);
    if (err) {
        return err;
    }
    if (out) {
        copy(out, &rec, sizeof rec);
    }
    return 0;
}

int quark_vfs_read(unsigned long handle, void *buf, unsigned long offset,
                   unsigned long len, unsigned long *got) {
    struct quark_msg msg;
    struct quark_msg reply;

    if (len > QUARK_VFS_MAX_IO) {
        len = QUARK_VFS_MAX_IO;
    }
    if (len == 0) {
        if (got) {
            *got = 0;
        }
        return 0;
    }
    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_READ;
    msg.data[0] = handle;
    msg.data[2] = offset;
    msg.data[3] = len;

    int err = vfs_call_lend(&msg, &reply, buf, len, QUARK_LEND_WRITE);
    if (err) {
        return err;
    }
    if (got) {
        *got = reply.data[0];
    }
    return 0;
}

int quark_vfs_write(unsigned long handle, const void *buf, unsigned long offset,
                    unsigned long len, unsigned long *put) {
    struct quark_msg msg;
    struct quark_msg reply;

    if (len > QUARK_VFS_MAX_IO) {
        len = QUARK_VFS_MAX_IO;
    }
    if (len == 0) {
        if (put) {
            *put = 0;
        }
        return 0;
    }
    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_WRITE;
    msg.data[0] = handle;
    msg.data[2] = offset;
    msg.data[3] = len;

    /* Lent for reading only; the cast drops a const the kernel keeps. */
    int err = vfs_call_lend(&msg, &reply, (void *)buf, len, QUARK_LEND_READ);
    if (err) {
        return err;
    }
    if (put) {
        *put = reply.data[0];
    }
    return 0;
}

int quark_vfs_close(unsigned long handle) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_CLOSE;
    msg.data[0] = handle;
    return vfs_call(&msg, &reply);
}
