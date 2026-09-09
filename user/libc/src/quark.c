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

size_t quark_vfs(void) {
    static size_t tid;
    static int looked;

    /* Services come up alongside their clients, so an answer of "not yet" is
       worth asking about again; an answer of "here it is" is not. */
    if (!looked || tid == 0) {
        tid = quark_lookup("vfs");
        looked = 1;
    }
    return tid;
}

/* ------------------------------------------------------------------------ */
/* The VFS, as a client.                                                     */
/* ------------------------------------------------------------------------ */

/* Ask the VFS one question and get its answer, translating "the call did not
   happen" and "the server said no" into the same small integers. */
static int vfs_call(struct quark_msg *msg, struct quark_msg *reply) {
    size_t vfs = quark_vfs();
    if (vfs == 0) {
        return QUARK_VFS_UNREACHABLE;
    }
    if (quark_call(vfs, msg, reply) != 0) {
        return QUARK_VFS_UNREACHABLE;
    }
    if (reply->tag == QUARK_ERR) {
        /* The server puts its code in the first word. A zero there would be
           "no error", which it would not have sent, so treat it as I/O. */
        unsigned long code = reply->data[0];
        return code ? (int)code : QUARK_VFS_IO;
    }
    return 0;
}

int quark_vfs_open(const char *path, int create, struct quark_vfs_file *out) {
    struct quark_msg msg;
    struct quark_msg reply;

    unsigned long len = length(path);
    if (len == 0 || len > QUARK_VFS_MAX_PATH) {
        return QUARK_VFS_INVALID_PATH;
    }

    zero(&msg, sizeof msg);
    msg.tag = create ? QUARK_VFS_TAG_CREATE : QUARK_VFS_TAG_OPEN;
    copy(msg.data, path, len);
    if (create) {
        /* A file rather than a directory. The path can never reach this word:
           one longer than the protocol carries is refused above. */
        msg.data[5] = 0;
    }

    int err = vfs_call(&msg, &reply);
    if (err) {
        return err;
    }
    if (out) {
        out->handle = reply.data[0];
        out->size = reply.data[1];
        out->is_dir = reply.data[2] != 0;
        out->mode = (unsigned int)reply.data[3];
        out->access = (unsigned int)reply.data[4];
    }
    return 0;
}

int quark_vfs_stat(unsigned long handle, struct quark_vfs_file *out) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_STAT;
    msg.data[0] = handle;

    int err = vfs_call(&msg, &reply);
    if (err) {
        return err;
    }
    if (out) {
        out->handle = handle;
        out->size = reply.data[0];
        out->is_dir = reply.data[1] != 0;
        out->mode = (unsigned int)reply.data[3];
        out->access = (unsigned int)reply.data[4];
    }
    return 0;
}

int quark_vfs_read(unsigned long handle, unsigned long phys, unsigned long offset,
                   unsigned long len, unsigned long *got) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_READ;
    msg.data[0] = handle;
    msg.data[1] = phys;
    msg.data[2] = offset;
    msg.data[3] = len;

    int err = vfs_call(&msg, &reply);
    if (err) {
        return err;
    }
    if (got) {
        *got = reply.data[0];
    }
    return 0;
}

int quark_vfs_write(unsigned long handle, unsigned long phys, unsigned long offset,
                    unsigned long len, unsigned long *put) {
    struct quark_msg msg;
    struct quark_msg reply;

    zero(&msg, sizeof msg);
    msg.tag = QUARK_VFS_TAG_WRITE;
    msg.data[0] = handle;
    msg.data[1] = phys;
    msg.data[2] = offset;
    msg.data[3] = len;

    int err = vfs_call(&msg, &reply);
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
