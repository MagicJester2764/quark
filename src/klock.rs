//! The kernel lock: one processor in the kernel at a time.
//!
//! Everything this kernel knows about being safe it knows in one form: with
//! interrupts off, nothing else runs. On one processor that is simply true.
//! On several it stays true of the *kernel* if only one of them is ever in
//! it, and that is what this lock is. Programs run on every processor at
//! once; whichever of them makes a system call, takes an interrupt or
//! faults waits here until the kernel is empty.
//!
//! It is the oldest way to put a kernel on a second processor, and it suits
//! a microkernel better than most: the servers and the drivers are
//! programs, and run side by side. What is serialised is what is left.
//!
//! The rules, which are few and each of which is kept in one place:
//!
//! - **Taken on the way in from ring 3**, by whoever arrives: the system
//!   call's dispatch, and the interrupt and exception handlers. A handler
//!   that interrupted the kernel itself finds the lock held by its own
//!   processor and takes nothing. Each remembers in its own frame whether it
//!   took the lock ([`enter`]), and gives back exactly that on the way out
//!   ([`leave`]).
//! - **Carried across a switch.** A processor that switches tasks in the
//!   kernel goes on holding the lock; the lock is the processor's and not
//!   the task's. The frames of the task it switches *to* say what to give
//!   back on the way out, and they were written when that task came in. A
//!   task that is switched back in on another processor is switched in by a
//!   processor that holds the lock, so its frames are still right.
//! - **Given up before ring 3**, every way there is to get there: the end
//!   of a system call, the end of an interrupt or a fault taken in ring 3,
//!   and the first entry of a new task, a forked child and an `exec`.
//! - **Not held by a processor with nothing to do.** The idle loop gives it
//!   up around its `hlt`. An interrupt taken there is the one case of a
//!   handler that interrupted the kernel and still has the lock to take.
//!
//! So a processor holds the lock exactly while it runs kernel code, the
//! `hlt` aside, and every context switch happens with it held.
//!
//! Waiting for it is spinning with interrupts off. Whoever waits has
//! nothing else it may do: it is on its way into the kernel.

use core::sync::atomic::{AtomicU32, Ordering};

/// The processor in the kernel: its index and one, or 0 for none.
static OWNER: AtomicU32 = AtomicU32::new(0);

/// What this processor is called here.
///
/// Interrupts must be off: the answer is this processor's only until the
/// task is moved.
#[inline(always)]
fn me() -> u32 {
    crate::percpu::index() as u32 + 1
}

/// Whether this processor has the lock. Interrupts must be off.
#[inline(always)]
pub fn held() -> bool {
    OWNER.load(Ordering::Relaxed) == me()
}

/// Take the lock, waiting for it. Interrupts must be off, and stay off.
pub fn acquire() {
    let me = me();
    loop {
        match OWNER.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => return,
            // Twice by one processor is a way out to ring 3 that forgot to
            // give it up, or a way in that did not ask. Waiting would be
            // waiting for itself, with interrupts off and nothing said.
            Err(owner) if owner == me => panic!("kernel lock: taken twice by one processor"),
            Err(_) => {}
        }
        while OWNER.load(Ordering::Relaxed) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// Give the lock up. Interrupts must be off.
pub fn release() {
    if OWNER.load(Ordering::Relaxed) != me() {
        panic!("kernel lock: given up by a processor that does not have it");
    }
    OWNER.store(0, Ordering::Release);
}

/// On the way into a handler: take the lock unless the code that was
/// interrupted had it. Says whether it was taken, for [`leave`].
#[inline]
pub fn enter() -> bool {
    if held() {
        false
    } else {
        acquire();
        true
    }
}

/// On the way out of a handler: give back what [`enter`] took.
///
/// Turns interrupts off first. A handler may have turned them on to wait
/// for something, and from here to the `iretq` nothing must arrive: the
/// processor is in the kernel and no longer has the lock.
#[inline]
pub fn leave(took: bool) {
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    if took {
        release();
    }
}
