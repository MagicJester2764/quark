/* Making a thread, when the kernel does not do it Linux's way.
 *
 * Linux's `clone` has the child *return from the same system call* on a new
 * stack, which is why musl's `__clone` is assembly: the child continues at the
 * instruction after `syscall` with a different stack pointer. Quark starts a
 * task at an entry point instead, and cannot resume something that never ran.
 *
 * So musl's `__clone` is replaced by a tail call into here — its seven
 * arguments are already in the registers a C function wants — and this does
 * the whole thing: create a task in the address space we are already in, plant
 * the thread-pointer and the function on its stack, and start it at a
 * trampoline that picks them up.
 *
 * What the child needs and one register cannot carry: the TLS pointer, the
 * function, and its argument. The argument travels in RDI, which is what
 * `sys_task_start_arg` fills; the other two are written onto the child's own
 * stack, below the top musl gave us, where nothing else will look.
 */

#include <quark/syscall.h>

#include "abi.h"

/* Linux's clone flags, of which only these mean anything here. */
#define CLONE_VM      0x00000100
#define CLONE_THREAD  0x00010000
#define CLONE_SETTLS  0x00080000
#define CLONE_CHILD_CLEARTID 0x00200000

long __quark_set_fs(unsigned long tp);

/* Defined in clone-entry.s. Starts with RDI = the thread function's argument
   and RSP pointing at [tls][func]. */
void __quark_thread_entry(void);

/* Called by the trampoline when the thread function returns. */
void __quark_thread_exit(int code);

void __quark_set_clear_tid(unsigned long addr);
void __quark_set_clear_tid(unsigned long addr) {
    __syscall1(SYS_SET_CLEAR_TID, addr);
}

void __quark_thread_exit(int code) {
    __syscall1(SYS_EXIT_CODE, (unsigned long)code);
    for (;;) { }
}

long __quark_clone(int (*func)(void *), void *stack, int flags, void *arg,
                   int *ptid, void *tls, int *ctid) {
    /* Only a thread. A child with a *copy* of the address space is fork, and
       there is none here — saying so is better than making half of one. */
    if (!(flags & CLONE_VM) || !(flags & CLONE_THREAD)) {
        return -LX_ENOSYS;
    }
    if (!func || !stack) {
        return -LX_EINVAL;
    }

    unsigned long cr3 = __syscall0(SYS_ADDRSPACE_SELF);
    if (cr3 == QUARK_ERR) {
        return -LX_EAGAIN;
    }
    unsigned long tid = __syscall0(SYS_TASK_CREATE);
    if (tid == QUARK_ERR) {
        return -LX_EAGAIN;
    }

    /* Sixteen-aligned, and far enough below musl's top that the two words
       planted here are inside the mapping rather than one past it. The kernel
       aligns what it is given and subtracts eight, so the child wakes with RSP
       exactly at the first of them. */
    unsigned long top = ((unsigned long)stack & ~15UL) - 48;
    unsigned long *slot = (unsigned long *)top;
    slot[-1] = (flags & CLONE_SETTLS) ? (unsigned long)tls : 0;
    slot[0] = (unsigned long)func;
    // CLONE_CHILD_CLEARTID, which musl depends on to release the thread-list
    // lock it holds through its own exit. Registered by the child rather than
    // for it: the call names its caller, and only the child is the child.
    slot[1] = (flags & CLONE_CHILD_CLEARTID) ? (unsigned long)ctid : 0;

    /* musl reads the new thread's id out of *ptid, and it must be there before
       the thread can run — afterwards is a race with the thread exiting. */
    if (ptid) {
        *ptid = (int)tid;
    }
    unsigned long started = __syscall5(SYS_TASK_START_ARG, tid,
                                       (unsigned long)__quark_thread_entry,
                                       top, cr3, (unsigned long)arg);
    if (started == QUARK_ERR) {
        return -LX_EAGAIN;
    }
    return (long)tid;
}

/* CLONE_CHILD_CLEARTID turned out not to be optional, and the comment that
   used to be here said it was.
 *
 * musl's `__pthread_exit` takes the thread-list lock and never unlocks it. Its
 * own comment explains why: "This change will not be visible until the lock is
 * released, which only happens after SYS_exit has been called, via the exit
 * futex address pointing at the lock." The lock *is* the word at `ctid`, and
 * the kernel clearing it on exit is what publishes the dead thread's removal
 * from the list. Without it the list stays locked by a task that no longer
 * exists, and the next thread to touch it — a joiner, or the process
 * exiting — waits for ever. */
