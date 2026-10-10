//! The kernel's locks.
//!
//! `IrqSpinLock` keeps interrupts off for as long as it is held — an
//! interrupt handler that wanted the same lock would wait for itself — and
//! is what the kernel shares between processors is kept under, below the
//! one lock (`klock.rs`) that still keeps most of it.
//!
//! **It waits.** Under the one lock nothing else could be in here, and a
//! lock found taken could only be the same code come round again, so it
//! panicked. Two processors in the kernel at once make a taken lock an
//! ordinary thing. Whoever finds it so reads it until it looks free, and
//! only then tries for it — a read is shared, a try is not, and trying in
//! a loop takes the line from every other waiter's cache each time — and
//! between reads waits a little longer each time, up to a limit, answering
//! what other processors ask of it meanwhile (`smp::while_waiting`):
//! whoever has the lock may be waiting for this one to forget a mapping.
//! It is not a queue, for the reason the one lock is not: a processor in
//! one that the machine underneath has stopped running holds up everybody
//! behind it.
//!
//! **Every lock has a rank, and a processor takes them in order.** A lock
//! is taken only if its rank is above that of every lock its processor
//! holds. Anything else is a deadlock waiting for its moment — two
//! processors each holding what the other wants — and is stopped at once,
//! naming both (`[KLOCK order: <held> held, <taken> taken]`), whether or not
//! the moment came. The ranks are the `RANK_*` here, outermost first, and
//! `docs/smp.md` has them as a table with what each keeps. The one lock is
//! rank 0: taken first or not at all. Two locks of one kind — two tasks'
//! records, two run queues — are taken in the order of where they are in
//! memory, which is the same order on every processor
//! ([`IrqSpinLock::lock_second`]); the second is held at the rank after its
//! kind's, and each kind that may be taken twice has that rank to itself.
//!
//! **Interrupts come back on when the last lock goes**, if they were on
//! when the first was taken (`percpu::locks_irq`) — not when whichever lock
//! was taken with them on goes: guards given back in another order than
//! they were taken would let an interrupt in while a lock was still held,
//! or leave interrupts off for good.
//!
//! **A wait of thirty seconds is a machine that has stopped.** Nothing the
//! kernel does under a lock takes that long. The waiter says which lock it
//! is and which processor has it, and stops the machine; and a processor
//! that was itself waiting for a lock when it was stopped says which — a
//! ring of them is a deadlock, and their lines say where it is.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// The one lock (`klock.rs`): outermost, taken first or not at all.
pub const RANK_KERNEL: u8 = 0;
/// A program's descriptor table.
pub const RANK_FDTABLE: u8 = 2;
/// The poll sets, which ask what they watch whether it is ready.
pub const RANK_POLLSET: u8 = 4;
/// What a program makes, a lock for each kind — its table and every one
/// of it — in the order one kind's may be taken under another's: a local
/// socket's connection makes a stream.
pub const RANK_LOCAL: u8 = 6;
pub const RANK_STREAM: u8 = 7;
pub const RANK_PTY: u8 = 8;
pub const RANK_PIPE: u8 = 9;
pub const RANK_EVENT: u8 = 10;
pub const RANK_TIMER: u8 = 11;
pub const RANK_SIGFD: u8 = 12;
pub const RANK_SERVED: u8 = 13;
pub const RANK_SHMEM: u8 = 14;
/// The watches (`ipc.rs`): who is to be told of which death, and what is
/// owed. Waking a watcher takes its record, after.
pub const RANK_NOTICES: u8 = 15;
/// The futex's waiters, a bucket at a time. Two, for a requeue.
pub const RANK_FUTEX: u8 = 16;
/// A capability space (`cap.rs`: one of 64, the one its number picks). Two,
/// for a thread joining its program's or a fork's copy.
pub const RANK_CSPACE: u8 = 20;
/// A program's record besides its descriptors (`fdtable.rs`): its signals,
/// its alarm, what it has used, its name and limits — asked about under
/// whatever a wait holds, so after the things waited on.
pub const RANK_PROGRAM: u8 = 22;
/// Which numbers have a capability space, and the making of a number's
/// counts of revocations (`cap.rs`): after a space's lock, which is held
/// when a space is given up or a capability first minted from a slot.
pub const RANK_CAP_TABLE: u8 = 23;
/// An address space's tables and reservations. Two, for a fork or a move.
pub const RANK_SPACE: u8 = 24;
/// A task's record (`scheduler::record_lock`: one of 256, the one its number
/// picks): its IPC state, and its state as the scheduler has it — blocked,
/// ready, dead — and the place it is lent. After whatever parks or wakes a
/// task, which does so holding its own lock — an object, a futex's list, an
/// address space whose unmapping leaves an object to its pager — and before
/// the run queues it is put on. Two, for a call.
pub const RANK_TASK: u8 = 26;
/// A processor's run queues. Two, for a move between processors.
pub const RANK_RUNQ: u8 = 28;
/// What is due, and when the clock is set to look.
pub const RANK_CLOCK: u8 = 32;
/// Who is told of which interrupt.
pub const RANK_IRQ: u8 = 36;
/// The displays drivers have been given memory for, which takes frames.
pub const RANK_DISPLAY: u8 = 40;
/// The kernel's heap, which takes frames when it grows.
pub const RANK_HEAP: u8 = 44;
/// Who owns which frame.
pub const RANK_FRAME_OWNER: u8 = 48;
/// The frames, free and given out.
pub const RANK_FRAMES: u8 = 52;
/// The console's screen, which anything may print to: innermost.
pub const RANK_CONSOLE: u8 = 60;

/// What every lock is besides what it keeps, at the start of it whatever it
/// keeps, so that a processor can name the locks it holds by where they
/// are: who has it, where it comes, and what it is called.
#[repr(C)]
pub struct Head {
    /// The processor that has it, and one; 0 when nobody has.
    owner: AtomicU32,
    rank: u8,
    name: &'static str,
}

impl Head {
    const fn new(rank: u8, name: &'static str) -> Self {
        assert!(rank < 64, "a lock's rank is a bit of a word");
        Head { owner: AtomicU32::new(0), rank, name }
    }

    fn at(&self) -> usize {
        self as *const Head as usize
    }

    /// Take it, waiting for whoever has it. Interrupts are off.
    fn take(&self) {
        let me = crate::percpu::index() as u32 + 1;
        let mut backoff = 1u32;
        let mut turns = 0u32;
        let (mut since, mut late) = (0u64, false);
        loop {
            if self.owner.compare_exchange_weak(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                return;
            }
            loop {
                let owner = self.owner.load(Ordering::Relaxed);
                if owner == 0 {
                    break;
                }
                if owner == me {
                    // The order check lets nothing through that could come
                    // to this; if it is here anyway, waiting would be
                    // waiting for itself.
                    say_lock(b"\n[KLOCK ", self, b" taken twice by one processor]\n");
                    panic!("a lock taken twice by one processor");
                }
                if crate::smp::halting() {
                    self.stopped_waiting(owner);
                }
                for _ in 0..backoff {
                    crate::smp::while_waiting();
                    core::hint::spin_loop();
                }
                backoff = (backoff * 2).min(MOST_BACKOFF);
                turns = turns.wrapping_add(1);
                if turns % LOOK == 0 && waited_long(&mut since, &mut late) {
                    self.stuck(owner);
                }
            }
        }
    }

    /// The machine is being stopped while this processor waits for the lock:
    /// if it is for a lock that was held too long, it says what it was
    /// waiting for, and what it was holding.
    #[cold]
    fn stopped_waiting(&self, owner: u32) -> ! {
        if STUCK.load(Ordering::Relaxed) {
            crate::serial::puts(b"[KSTUCK processor ");
            crate::serial::put_usize(crate::percpu::index());
            say_lock(b" was waiting for ", self, b", which processor ");
            crate::serial::put_usize(owner.saturating_sub(1) as usize);
            crate::serial::puts(b" has; it held");
            say_held();
            crate::serial::puts(b"]\n");
        }
        crate::smp::halt_here()
    }

    /// Waited thirty seconds for it: say which and whose, and stop.
    #[cold]
    fn stuck(&self, owner: u32) -> ! {
        say_lock(b"\n[KSTUCK ", self, b" has been processor ");
        crate::serial::put_usize(owner.saturating_sub(1) as usize);
        crate::serial::puts(b"'s for thirty seconds; processor ");
        crate::serial::put_usize(crate::percpu::index());
        crate::serial::puts(b" is waiting for it, holding");
        say_held();
        crate::serial::puts(b"]\n");
        stop_stuck("a lock was held for thirty seconds")
    }
}

/// How long the wait between two reads of a lock grows to, in turns.
const MOST_BACKOFF: u32 = 64;
/// How many reads between looks at the clock.
const LOOK: u32 = 1 << 16;

/// How long a processor waits for a lock before it calls it stuck, and how
/// much longer it then gives it: a machine that was itself stopped for a
/// while — it is usually a virtual one — comes back with the wait looking
/// long and the holder a moment from done.
const STUCK_NS: u64 = 30_000_000_000;
const GRACE_NS: u64 = 1_000_000_000;

/// A processor has waited that long for a lock — the one lock or another:
/// whoever has it says where it is if it can, and whoever was waiting for a
/// lock when they were stopped says which.
static STUCK: AtomicBool = AtomicBool::new(false);

/// Whether the machine is being stopped because a lock was held too long.
pub fn stuck() -> bool {
    STUCK.load(Ordering::Relaxed)
}

/// Whether a wait that began at `since` (nought: not yet looked at) has
/// gone on too long — by the fine clock only: the tick is counted by the
/// first processor, which may be the one waiting.
pub(crate) fn waited_long(since: &mut u64, late: &mut bool) -> bool {
    if !crate::clock::fine() {
        return false;
    }
    let now = crate::clock::now();
    if *since == 0 {
        *since = now;
        return false;
    }
    if now.saturating_sub(*since) < STUCK_NS {
        return false;
    }
    if !*late {
        // Once more, a little later, before believing it.
        *late = true;
        *since = now.saturating_sub(STUCK_NS - GRACE_NS);
        return false;
    }
    true
}

/// Stop the machine for a lock held too long: every other processor told,
/// half a second for them to say where they were, and a panic.
pub(crate) fn stop_stuck(why: &'static str) -> ! {
    STUCK.store(true, Ordering::SeqCst);
    crate::smp::halt_others();
    let until = crate::clock::now() + 500_000_000;
    while crate::clock::now() < until {
        core::hint::spin_loop();
    }
    panic!("{}", why)
}

/// What the one lock is called when the order is said.
static KERNEL: Head = Head::new(RANK_KERNEL, "the kernel");

/// This processor is about to wait for the one lock (`klock::acquire`): it
/// must hold no other, or the order is broken. Interrupts are off.
pub(crate) fn kernel_taking() {
    in_order(RANK_KERNEL, &KERNEL);
}

/// This processor has the one lock. Interrupts are off.
pub(crate) fn kernel_taken() {
    crate::percpu::lock_taken(RANK_KERNEL, KERNEL.at());
}

/// The one lock is given up by this processor (`klock::release`).
pub(crate) fn kernel_given() {
    crate::percpu::lock_given(RANK_KERNEL);
}

/// The rank of the innermost lock in `held`.
fn innermost(held: u64) -> u8 {
    63 - held.leading_zeros() as u8
}

/// That taking `taking` at `rank` keeps the order, or the machine stops
/// saying it does not. Interrupts are off.
fn in_order(rank: u8, taking: &Head) {
    let held = crate::percpu::locks_held();
    if held != 0 && rank <= innermost(held) {
        let at = crate::percpu::lock_held(innermost(held));
        crate::serial::puts(b"\n[KLOCK order: ");
        say_name(at);
        crate::serial::puts(b" held, ");
        crate::serial::puts(taking.name.as_bytes());
        crate::serial::puts(b" taken]\n");
        panic!("a lock taken out of order");
    }
}

/// Whether a lock of `rank` would be refused if this processor took it
/// now: whether it holds one of that rank or past it.
pub fn would_refuse(rank: u8) -> bool {
    let flags = irq_save();
    let held = crate::percpu::locks_held();
    irq_restore(flags);
    held != 0 && rank <= innermost(held)
}

/// Whether this processor holds any lock but the one lock. A task is never
/// switched away from while it does (`scheduler::switch_to`): the lock
/// would be held by a processor its task had left, and said to be held by
/// whatever ran there next. Interrupts are off.
pub fn holding_any() -> bool {
    crate::percpu::locks_held() & !(1 << RANK_KERNEL) != 0
}

/// Say on the serial line what the lock at `at` is called.
fn say_name(at: usize) {
    if at == 0 {
        crate::serial::puts(b"nothing");
    } else {
        crate::serial::puts(unsafe { (*(at as *const Head)).name }.as_bytes());
    }
}

/// Say `before`, what `lock` is called and its rank, and `after`.
fn say_lock(before: &[u8], lock: &Head, after: &[u8]) {
    crate::serial::puts(before);
    crate::serial::puts(b"the lock of ");
    crate::serial::puts(lock.name.as_bytes());
    crate::serial::puts(b" (rank ");
    crate::serial::put_usize(lock.rank as usize);
    crate::serial::puts(b")");
    crate::serial::puts(after);
}

/// Say the locks this processor holds, by name.
fn say_held() {
    let held = crate::percpu::locks_held();
    if held == 0 {
        crate::serial::puts(b" nothing");
    }
    for rank in (0..64u8).filter(|r| held >> r & 1 == 1) {
        crate::serial::puts(b" ");
        say_name(crate::percpu::lock_held(rank));
    }
}

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    if flags & (1 << 9) != 0 {
        unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
    }
}

/// A lock that keeps interrupts off while it is held, and waits for it
/// (above).
#[repr(C)]
pub struct IrqSpinLock<T> {
    head: Head,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for IrqSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// A lock of `rank`, called `name` when the order is broken or it is
    /// held too long.
    pub const fn new(rank: u8, name: &'static str, data: T) -> Self {
        IrqSpinLock { head: Head::new(rank, name), data: UnsafeCell::new(data) }
    }

    /// Take it, waiting while another processor has it. Interrupts are off
    /// until it is given back, and come back on with the last lock this
    /// processor gives back if they were on when it was taken.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let flags = irq_save();
        in_order(self.head.rank, &self.head);
        self.head.take();
        first_lock(flags);
        crate::percpu::lock_taken(self.head.rank, self.head.at());
        IrqSpinLockGuard { lock: self, rank: self.head.rank }
    }

    /// Take it, unless this processor holds it already — at its kind's rank
    /// or as a second — in which case nothing is taken: for a step that may
    /// be begun inside another that has it (a walk of an address space's
    /// tables inside another of the same space's).
    pub fn lock_unless_held(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let flags = irq_save();
        let rank = self.head.rank;
        let mine = crate::percpu::lock_held(rank) == self.head.at()
            || (rank < 63 && crate::percpu::lock_held(rank + 1) == self.head.at());
        irq_restore(flags);
        if mine { None } else { Some(self.lock()) }
    }

    /// The second lock of a kind its processor holds one of: after that one
    /// in memory, so that every processor takes any two of the kind in the
    /// same order, and at the rank after the kind's. Anything else is the
    /// order broken.
    pub fn lock_second(&self) -> IrqSpinLockGuard<'_, T> {
        let flags = irq_save();
        let first = crate::percpu::lock_held(self.head.rank);
        if first == 0 || first >= self.head.at() {
            crate::serial::puts(b"\n[KLOCK order: ");
            say_name(first);
            crate::serial::puts(b" held, ");
            crate::serial::puts(self.head.name.as_bytes());
            crate::serial::puts(b" taken as the second of its kind, and not after it]\n");
            panic!("a second lock of a kind taken out of order");
        }
        let rank = self.head.rank + 1;
        in_order(rank, &self.head);
        self.head.take();
        first_lock(flags);
        crate::percpu::lock_taken(rank, self.head.at());
        IrqSpinLockGuard { lock: self, rank }
    }
}

/// If this processor holds no lock yet but the one lock, whether
/// interrupts were on before the one it is taking, for the last it gives
/// back to put back. Interrupts are off.
fn first_lock(flags: u64) {
    if !holding_any() {
        crate::percpu::set_locks_irq(flags & (1 << 9) != 0);
    }
}

pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    /// The rank it is held at: its kind's, or the one after for a second.
    rank: u8,
}

impl<T> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        crate::percpu::lock_given(self.rank);
        self.lock.head.owner.store(0, Ordering::Release);
        // Back on with the last lock given back, if they were on before the
        // first — not before then, whatever order the guards go in.
        if !holding_any() && crate::percpu::locks_irq() {
            unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
        }
    }
}
