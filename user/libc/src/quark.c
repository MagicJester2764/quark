/* Talking to the rest of the system.
 *
 * Almost nothing a program wants is a system call on Quark. The kernel does
 * scheduling, memory and message passing; files, the console and the network
 * are services reached by sending a message to a task. So the parts of libc
 * that look like system calls elsewhere are IPC here, and this file is where
 * that translation lives.
 */

#include <string.h>
#include <quark/syscall.h>

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

    memset(&msg, 0, sizeof msg);
    memset(packed, 0, sizeof packed);

    size_t len = strlen(name);
    if (len > sizeof packed) {
        len = sizeof packed;
    }
    memcpy(packed, name, len);
    memcpy(msg.data, packed, sizeof packed);
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
