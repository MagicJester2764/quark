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
//! **`IrqSpinLock` still means what it says.** The locks inside the kernel
//! (`sync.rs`) panic if they are found taken, on the reasoning that with
//! interrupts off nobody else could have taken them. Under this lock that
//! is still so: only one processor is in the kernel.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

/// How long a processor waits for the kernel before it calls it stuck, and
/// how much longer it then gives it: a machine that was itself stopped for
/// a while — it is usually a virtual one — comes back with the wait
/// looking long and the holder a moment from done.
const STUCK_NS: u64 = 30_000_000_000;
const GRACE_NS: u64 = 1_000_000_000;
/// How many turns of the wait between looks at the clock.
const LOOK: u32 = 1 << 20;

/// A processor has waited that long: whoever has the kernel says where it
/// is when it is stopped.
static STUCK: AtomicBool = AtomicBool::new(false);

/// Whether this processor is the one that has had the kernel too long.
/// Interrupts must be off.
pub fn stuck() -> bool {
    STUCK.load(Ordering::Relaxed) && held()
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
            crate::smp::while_waiting();
            core::hint::spin_loop();
            turns = turns.wrapping_add(1);
            if turns % LOOK == 0 {
                waited(&mut since, &mut late);
            }
        }
    }
    WAITERS.fetch_sub(1, Ordering::Relaxed);
}

/// This processor has been waiting a while: for how long, and whether that
/// is too long.
#[cold]
fn waited(since: &mut u64, late: &mut bool) {
    if !crate::clock::fine() {
        return;
    }
    let now = crate::clock::now();
    if *since == 0 {
        *since = now;
        return;
    }
    if now.saturating_sub(*since) < STUCK_NS {
        return;
    }
    if !*late {
        // Once more, a little later, before believing it.
        *late = true;
        *since = now.saturating_sub(STUCK_NS - GRACE_NS);
        return;
    }
    let owner = OWNER.load(Ordering::Relaxed);
    crate::serial::puts(b"\n[KSTUCK the kernel has been processor ");
    crate::serial::put_usize(owner.saturating_sub(1) as usize);
    crate::serial::puts(b"'s for thirty seconds; processor ");
    crate::serial::put_usize(me().saturating_sub(1) as usize);
    crate::serial::puts(b" is waiting for it]\n");
    // The holder is stopped with the rest, and says where it was — if it
    // can be interrupted at all. Half a second for it to say so.
    STUCK.store(true, Ordering::SeqCst);
    crate::smp::halt_others();
    let until = crate::clock::now() + 500_000_000;
    while crate::clock::now() < until {
        core::hint::spin_loop();
    }
    panic!("the kernel was one processor's for thirty seconds");
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
