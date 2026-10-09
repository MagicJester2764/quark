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
//! nothing else it may do: it is on its way into the kernel. Two things
//! cannot wait for it, though, and it looks for both each time round
//! (`smp::while_waiting`): the holder may be waiting for *this* processor
//! to forget a mapping, and the kernel may have faulted and be stopping
//! the machine.
//!
//! **It is not a queue, and it is not unfair either.** A processor that
//! makes one system call after another gives the lock up and asks for it
//! back within a few dozen instructions, and would beat a processor that
//! has been waiting to it every time: the waiter has to see that the lock
//! is free before it can ask. The clock is an interrupt on one processor,
//! and a clock that cannot get into the kernel stops. So a processor that
//! had the lock last, and finds somebody waiting, stands aside for a
//! moment first. A queue would be fairer still and worse here: a processor
//! in it that the machine underneath has stopped running — this is usually
//! a virtual machine — holds up everybody behind it.
//!
//! **A processor that has waited half a minute says so, and the machine
//! stops.** Nothing the kernel does takes that long. What does is a loop in
//! it that is waiting for somebody else to do something — and nobody else
//! can do anything: they are all here. That is a machine with every
//! processor busy and nothing on the screen or the serial line to say why,
//! which is what a terminal's write was for half an hour before it was
//! found by asking the machine's monitor where each processor was. So the
//! waiter says which processor has the kernel, the holder is stopped and
//! says where it was (`[KSTUCK ...]`, read as a kernel fault is), and the
//! waiter panics. By the fine clock only: the tick is counted by the first
//! processor, which may be the one waiting.
//!
//! **It is rank 0 of the kernel's locks** (`sync.rs`): the outermost, taken
//! first or not at all. A processor holding any other lock that came to
//! take this one would be holding what another processor, in here, may be
//! waiting for — and the order check stops it saying so. The locks inside
//! the kernel wait now, as this one does; while nearly everything is still
//! under this one, they are seldom found taken.

use core::sync::atomic::{AtomicU32, Ordering};

/// The processor in the kernel: its index and one, or 0 for none.
static OWNER: AtomicU32 = AtomicU32::new(0);
/// How many processors are waiting for it.
static WAITERS: AtomicU32 = AtomicU32::new(0);
/// Who had it last.
static LAST: AtomicU32 = AtomicU32::new(0);

/// How long a processor that had the lock last stands aside for one that
/// is waiting: this many turns of a loop, and no longer. The waiter may be
/// a processor that is not running at all just now.
const STAND_ASIDE: u32 = 4096;

/// How many turns of the wait between looks at the clock, which says when
/// the wait has been too long (`sync::waited_long`: thirty seconds).
const LOOK: u32 = 1 << 20;

/// Whether this processor has the kernel while the machine is stopped for a
/// lock held too long: it says where it was. Interrupts must be off.
pub fn stuck() -> bool {
    crate::sync::stuck() && held()
}

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
    // First, or not at all: with another lock held this is a wait that the
    // holder of the kernel may be waiting for in turn.
    crate::sync::kernel_taking();
    // Let whoever is waiting go first, if this processor went last.
    if LAST.load(Ordering::Relaxed) == me {
        let mut turns = 0;
        while WAITERS.load(Ordering::Relaxed) != 0 && turns < STAND_ASIDE {
            crate::smp::while_waiting();
            core::hint::spin_loop();
            turns += 1;
        }
    }
    WAITERS.fetch_add(1, Ordering::Relaxed);
    let mut turns: u32 = 0;
    let mut since: u64 = 0;
    let mut late = false;
    loop {
        match OWNER.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => break,
            // Twice by one processor is a way out to ring 3 that forgot to
            // give it up, or a way in that did not ask. Waiting would be
            // waiting for itself, with interrupts off and nothing said.
            Err(owner) if owner == me => panic!("kernel lock: taken twice by one processor"),
            Err(_) => {}
        }
        while OWNER.load(Ordering::Relaxed) != 0 {
            if crate::smp::halting() && crate::sync::stuck() {
                // Stopped for a lock held too long while it waited here.
                crate::serial::puts(b"[KSTUCK processor ");
                crate::serial::put_usize(me.saturating_sub(1) as usize);
                crate::serial::puts(b" was waiting for the kernel, which processor ");
                crate::serial::put_usize(OWNER.load(Ordering::Relaxed).saturating_sub(1) as usize);
                crate::serial::puts(b" has]\n");
                crate::smp::halt_here();
            }
            crate::smp::while_waiting();
            core::hint::spin_loop();
            turns = turns.wrapping_add(1);
            if turns % LOOK == 0 && crate::sync::waited_long(&mut since, &mut late) {
                waited();
            }
        }
    }
    WAITERS.fetch_sub(1, Ordering::Relaxed);
    crate::sync::kernel_taken();
}

/// This processor has waited thirty seconds for the kernel: it says which
/// processor has it, that one is stopped with the rest and says where it
/// is (`[KSTUCK ...]`, read as a kernel fault is) if its interrupts are on,
/// and the machine stops.
#[cold]
fn waited() -> ! {
    let owner = OWNER.load(Ordering::Relaxed);
    crate::serial::puts(b"\n[KSTUCK the kernel has been processor ");
    crate::serial::put_usize(owner.saturating_sub(1) as usize);
    crate::serial::puts(b"'s for thirty seconds; processor ");
    crate::serial::put_usize(me().saturating_sub(1) as usize);
    crate::serial::puts(b" is waiting for it]\n");
    crate::sync::stop_stuck("the kernel was one processor's for thirty seconds")
}

/// Give the lock up. Interrupts must be off.
///
/// Not before every other processor has been told of the mappings this one
/// took away while it had it (`tlb.rs`): once the lock is free they may be
/// anywhere.
pub fn release() {
    let me = me();
    if OWNER.load(Ordering::Relaxed) != me {
        panic!("kernel lock: given up by a processor that does not have it");
    }
    crate::tlb::sync();
    crate::sync::kernel_given();
    LAST.store(me, Ordering::Relaxed);
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
