/// Futex (fast userspace mutex) support.
///
/// Provides wait/wake operations on user-space atomic words,
/// enabling efficient blocking synchronization primitives.
///
/// A waiter is its task's record (`TaskRec::futex`): the word it waits on,
/// its deadline, why it woke, and a link into one of [`BUCKETS`] lists, the
/// one its word's key hashes to. No wait is refused for room: there were
/// sixty-four waiters for the whole machine, and the sixty-fifth was told no
/// at once, which a lock's user takes for a wake and goes round again — a
/// program of a hundred threads waiting on one condition had thirty-six of
/// them spinning. And a waiter can be moved from one word to another
/// ([`requeue`]), which is how a condition variable's broadcast hands its
/// waiters to the mutex instead of waking them all to fight for it.
///
/// A word can also be a lock that lends its holder the place of whoever
/// waits for it (`SYS_FUTEX_PI`, Linux's FUTEX_LOCK_PI): it holds the
/// holder's task id, and the kernel hands it from holder to waiter. A waiter
/// for one is a waiter like any other, on the same lists, that knows whose
/// the word is (`PerTask::pi_owner`): the scheduler asks it
/// ([`pi_waits_on`]) when it works out where a task runs, as it asks IPC.

use core::sync::atomic::{AtomicU32, Ordering::SeqCst};

use crate::scheduler;
use crate::sync::IrqSpinLock;

/// Lists a waiter is on, by a hash of its word's key.
const BUCKETS: usize = 256;

/// The end of a list.
const END: u16 = u16::MAX;

/// A priority-inheriting word: its holder's task id, and two bits — somebody
/// waits for it in the kernel, and its holder died holding it (a robust
/// mutex's). Linux's FUTEX_WAITERS, FUTEX_OWNER_DIED and FUTEX_TID_MASK.
pub const PI_WAITERS: u32 = 0x8000_0000;
pub const PI_OWNER_DIED: u32 = 0x4000_0000;
pub const PI_OWNER: u32 = 0x3FFF_FFFF;

/// How long a chain of holders waiting for holders is followed: lent down,
/// and searched for the task about to wait, which would be waiting for
/// itself. A longer one is answered as a cycle.
pub const PI_DEPTH: usize = 32;

/// `SYS_FUTEX_PI`'s answers, beside 0, [`TIMED_OUT`], `u64::MAX` and the
/// signal's: a try found it held (Linux's EAGAIN); the caller holds it, or
/// would wait for itself (EDEADLK); the task the word names is not there
/// (ESRCH); and a caller that does not hold what it unlocks.
pub const PI_BUSY: u64 = 1;
pub const PI_DEADLOCK: u64 = 3;
pub const PI_NO_OWNER: u64 = 4;
pub const PI_NOT_YOURS: u64 = u64::MAX - 1;

/// Which word a task is waiting on.
///
/// A word in memory the program does not own — shared memory, a file mapped
/// shared — is the same word in every program that maps it, wherever each
/// has it: it is named by its frame. Keying on (cr3, vaddr) made a futex
/// inside a shared-memory region a *different* object in every task that
/// mapped it, so cross-process synchronisation through shmem silently never
/// woke anyone.
///
/// A word in memory the program owns is that program's and nobody else's:
/// it is named by the address space and the address. Its frame says less
/// than that and more. After a `fork` the parent and the child have the
/// page in one frame until one of them writes it, and are not waiting on
/// one word; and the one that writes has it in another frame from then on,
/// so a thread that waited before the write was never found by the wake
/// that came after it — which is every wake, since what is waited for is a
/// write to that word.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Key {
    /// The address space, for a word in memory it owns; 0 for a frame.
    space: usize,
    /// The address there, or the frame's.
    at: usize,
}

const NO_KEY: Key = Key { space: 0, at: 0 };

impl Key {
    /// The list a waiter on this word is on.
    fn bucket(&self) -> usize {
        let mixed = (self.space as u64 ^ (self.at as u64).rotate_left(17)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> 56) as usize % BUCKETS
    }
}

/// The key of the word at `addr` in `cr3`, if there is a page there.
pub fn key_of(cr3: usize, addr: u64) -> Option<Key> {
    unsafe {
        let own = crate::paging::leaf_flags(cr3, addr as usize)
            .is_some_and(|f| f & crate::paging::OWNED != 0);
        if own {
            return Some(Key { space: cr3, at: addr as usize });
        }
        crate::paging::translate(cr3, addr as usize).map(|at| Key { space: 0, at })
    }
}

/// What this module keeps about a task, in its record (`TaskRec::futex`).
pub struct PerTask {
    /// On a list, waiting.
    waiting: bool,
    /// The word it waits on.
    key: Key,
    /// When to give up, in the clock's nanoseconds. 0 = wait indefinitely.
    deadline: u64,
    /// Why it woke, other than a wake: its deadline passed, or a signal the
    /// kernel runs a handler for ended the wait. Read by the waiter when it
    /// runs again, and cleared then.
    expired: bool,
    interrupted: bool,
    /// Waiting to lock a priority-inheriting word, held by `pi_owner` — lent
    /// this task's place while it waits — or by nobody it can lend to any
    /// more ([`END`]: that one has gone).
    pi: bool,
    pi_owner: u16,
    /// Handed the word by its holder's unlock or death: it has it now.
    pi_got: bool,
    /// The tasks either side of it on its list.
    next: u16,
    prev: u16,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            waiting: false,
            key: NO_KEY,
            deadline: 0,
            expired: false,
            interrupted: false,
            pi: false,
            pi_owner: END,
            pi_got: false,
            next: END,
            prev: END,
        }
    }
}

/// A list's first and last.
#[derive(Clone, Copy)]
struct Bucket {
    first: u16,
    last: u16,
}

/// The lists, a lock each (`sync::RANK_FUTEX`): a list's links, and what
/// this module keeps of a task while it is on one, are its list's lock's.
/// A requeue takes two, in the order of where they are (`lock_second`).
static LISTS: [IrqSpinLock<Bucket>; BUCKETS] =
    [const { IrqSpinLock::new(crate::sync::RANK_FUTEX, "a futex's waiters", Bucket { first: END, last: END }) }; BUCKETS];

type Held = crate::sync::IrqSpinLockGuard<'static, Bucket>;

/// The list `key`'s waiters are on, held.
fn list_of(key: Key) -> Held {
    LISTS[key.bucket()].lock()
}

/// The list `tid` waits on, held, if it waits on one — found by its key and
/// looked at again once held, since a requeue may have moved it meanwhile.
///
/// # Safety
/// Interrupts off.
unsafe fn list_of_waiter(tid: usize) -> Option<Held> {
    unsafe {
        loop {
            let at = rec(tid as u16).filter(|me| me.waiting)?.key.bucket();
            let held = LISTS[at].lock();
            match rec(tid as u16) {
                Some(me) if me.waiting && me.key.bucket() == at => return Some(held),
                Some(me) if me.waiting => continue,
                _ => return None,
            }
        }
    }
}

/// Task `t`'s record, unless it is the end of a list or no task.
///
/// # Safety
/// The lock of the list it is on is held, if it is on one; and with it
/// interrupts are off.
unsafe fn rec(t: u16) -> Option<&'static mut PerTask> {
    if t == END {
        return None;
    }
    unsafe { scheduler::rec(t as usize).map(|r| &mut r.futex) }
}

/// `tid` waits on `key`, last on its list, `b`.
///
/// # Safety
/// `b` is `key`'s list, held, and `tid` is on no list.
unsafe fn link(b: &mut Bucket, tid: usize, key: Key) {
    unsafe {
        let Some(me) = rec(tid as u16) else { return };
        me.waiting = true;
        me.key = key;
        me.next = END;
        me.prev = b.last;
        match rec(b.last) {
            Some(last) => last.next = tid as u16,
            None => b.first = tid as u16,
        }
        b.last = tid as u16;
    }
}

/// `tid` is off its list, `b`, and waits on nothing.
///
/// # Safety
/// `b` is the list `tid` is on, held.
unsafe fn unlink(b: &mut Bucket, tid: usize) {
    unsafe {
        let Some(me) = rec(tid as u16).filter(|me| me.waiting) else { return };
        let (next, prev) = (me.next, me.prev);
        match rec(prev) {
            Some(p) => p.next = next,
            None => b.first = next,
        }
        match rec(next) {
            Some(n) => n.prev = prev,
            None => b.last = prev,
        }
        me.waiting = false;
        me.next = END;
        me.prev = END;
    }
}

/// The waiters on `key`'s list that wait on `key` — to lock it, if `pi`, and
/// for a wake if not — in the order they began to, up to `most` of them:
/// handed to `each`, which may take them off. A wake is not for a waiter to
/// lock: that one is handed the word, and a requeue would take it from
/// the holder it lends to.
///
/// # Safety
/// `b` is `key`'s list, held.
unsafe fn each_on(
    b: &mut Bucket,
    key: Key,
    pi: bool,
    most: u64,
    mut each: impl FnMut(&mut Bucket, usize),
) -> u64 {
    unsafe {
        let mut t = b.first;
        let mut done = 0u64;
        while done < most {
            let Some(r) = rec(t) else { break };
            let (next, mine) = (r.next, r.key == key && r.pi == pi);
            if mine {
                each(b, t as usize);
                done += 1;
            }
            t = next;
        }
        done
    }
}

const USER_ADDR_LIMIT: u64 = crate::paging::USER_ADDR_LIMIT;

/// Wait on a futex word. If `*addr == expected`, block the calling task.
/// Returns 0 on wake, 1 if value mismatch, u64::MAX on error.
pub fn futex_wait(addr: u64, expected: u32) -> u64 {
    wait(addr, expected, None)
}

/// As [`futex_wait`], giving up after `timeout_ns` nanoseconds.
///
/// Returns 2 if the deadline passed before anything woke the task. A timeout
/// of 0 makes this a plain check of the word: it returns 1 immediately if the
/// value differs and 2 if it does not, without ever blocking.
///
/// Without this, a timed wait has to be built out of polling and yielding,
/// which burns a core for the length of the wait and cannot see a wake any
/// sooner than the next poll. That is what `Condvar::wait_timeout` and
/// `thread::park_timeout` are.
pub fn futex_wait_timeout(addr: u64, expected: u32, timeout_ns: u64) -> u64 {
    wait(addr, expected, Some(timeout_ns))
}

/// Whether `addr` names a word a program may wait on, or name to be woken.
fn word_ok(addr: u64) -> bool {
    addr != 0 && addr % 4 == 0 && addr.checked_add(4).is_some_and(|end| end <= USER_ADDR_LIMIT)
}

/// The word at `addr` in the caller's memory, made present and kept so for
/// the rest of the call, and its key. It is read below with a lock held:
/// faulting on it there would be a fault in the kernel with interrupts off,
/// and a page written out and brought back is not the frame it was.
fn held_word(cr3: usize, addr: u64) -> Option<Key> {
    // Pinned before it is looked at, as `validate_user_range` pins: what
    // the look finds is then what stays.
    scheduler::pin(addr, 4);
    // Given its memory under the one lock, as a call's buffer is
    // (`syscall::validate_user_range`), if it has none.
    let usable = unsafe {
        crate::paging::user_range_accessible(cr3, addr, 4, false)
            || scheduler::with_kernel(|| {
                crate::paging::back_range(cr3, addr, 4, false).is_ok()
                    && crate::paging::user_range_accessible(cr3, addr, 4, false)
            })
    };
    if !usable {
        return None;
    }
    key_of(cr3, addr)
}

fn wait(addr: u64, expected: u32, timeout_ns: Option<u64>) -> u64 {
    if !word_ok(addr) {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    let cr3 = scheduler::current_task_cr3();
    let Some(key) = held_word(cr3, addr) else {
        return u64::MAX;
    };

    let mut list = list_of(key);

    // Read the user word — we're in the same address space (syscall context)
    let current_val = {
        let _ua = crate::cpu::UserAccess::begin();
        unsafe { *(addr as *const u32) }
    };
    if current_val != expected {
        return 1;
    }

    // A zero timeout means "check, do not wait". The value still matches, so
    // there is nothing to report but the expiry — and blocking here, with no
    // time to wait, would mean never waking.
    if timeout_ns == Some(0) {
        return TIMED_OUT;
    }

    // 0 means "no deadline", which is why the arithmetic saturates rather
    // than wrapping: a far-future deadline must stay far-future, not land
    // back on the sentinel.
    let deadline = match timeout_ns {
        Some(t) => crate::clock::after(t),
        None => 0,
    };

    // A signal the kernel runs a handler for ends the wait, or stops it
    // beginning: looked for with the lock held, which is the same step as
    // being recorded, so that one raised a moment later finds this parked.
    if crate::signal::ends_wait(tid) {
        return crate::signal::INTERRUPTED;
    }

    unsafe {
        let Some(me) = rec(tid as u16) else {
            return u64::MAX;
        };
        me.deadline = deadline;
        me.expired = false;
        me.interrupted = false;
        link(&mut list, tid, key);
    }

    // Block the task while holding the lock to prevent wake races
    scheduler::block_task(tid);
    if deadline != 0 {
        crate::clock::due(deadline);
    }
    drop(list);

    // Yield to let the scheduler pick another task
    scheduler::yield_now();

    // Woken: by a wake, which took it off its list; by its deadline or a
    // signal, which did too and said so; or by something else entirely,
    // which did not — it is taken off now, either way, from whichever list
    // it is on (a requeue may have moved it). Off a list, what the module
    // keeps of it is its own: nobody writes it.
    let flags = irq_save();
    let (timed_out, interrupted) = unsafe {
        if let Some(mut list) = list_of_waiter(tid) {
            unlink(&mut list, tid);
        }
        match rec(tid as u16) {
            Some(me) => {
                let why = (me.expired, me.interrupted);
                me.expired = false;
                me.interrupted = false;
                me.deadline = 0;
                why
            }
            None => (false, false),
        }
    };
    irq_restore(flags);

    if timed_out {
        TIMED_OUT
    } else if interrupted {
        crate::signal::INTERRUPTED
    } else {
        0
    }
}

/// A signal has arrived for `tid`, which may be waiting on a futex: if it
/// is, it is woken to say so. True if it was.
pub fn interrupt(tid: usize) -> bool {
    let flags = irq_save();
    unsafe {
        let Some(mut list) = list_of_waiter(tid) else {
            irq_restore(flags);
            return false;
        };
        let lent_to = lends_to(tid);
        unlink(&mut list, tid);
        if let Some(me) = rec(tid as u16) {
            me.interrupted = true;
        }
        // What it lent, it lends no more: now, and not when it next runs —
        // a holder running at its place could keep it from running.
        if let Some(owner) = lent_to {
            scheduler::refresh_priority(owner);
        }
        scheduler::unblock_task(tid);
    }
    irq_restore(flags);
    true
}

/// Returned by a timed wait whose deadline passed.
pub const TIMED_OUT: u64 = 2;

/// Wake up to `max_wake` tasks waiting on the futex at `addr`.
/// Returns the number of tasks woken.
pub fn futex_wake(addr: u64, max_wake: u64) -> u64 {
    wake_in(scheduler::current_task_cr3(), addr, max_wake)
}

/// [`futex_wake`] of the word at `addr` in address space `cr3`, which need
/// not be the caller's: a dying task's robust mutexes are woken in its own.
pub fn wake_in(cr3: usize, addr: u64, max_wake: u64) -> u64 {
    if addr == 0 {
        return 0;
    }
    // No page there is no waiter there: a wait gives the page its memory
    // before it waits.
    let Some(key) = key_of(cr3, addr) else {
        return 0;
    };
    let mut list = list_of(key);
    unsafe {
        each_on(&mut list, key, false, max_wake, |list, t| {
            unlink(list, t);
            scheduler::unblock_task(t);
        })
    }
}

/// `SYS_FUTEX_REQUEUE`'s answer when the first word does not hold what the
/// caller said it would: Linux's `EAGAIN`.
pub const NOT_AS_SAID: u64 = 0xFFFF_FFFE;

/// Wake up to `nr_wake` of the waiters on the word at `first`, and move up to
/// `nr_requeue` of the rest to wait on the word at `second` instead, as if
/// they had waited there: Linux's `FUTEX_REQUEUE`, and with `expected`, its
/// `FUTEX_CMP_REQUEUE`, which does nothing — and answers [`NOT_AS_SAID`] —
/// unless the first word still holds that. How many were woken and moved.
pub fn requeue(first: u64, second: u64, nr_wake: u64, nr_requeue: u64, expected: Option<u32>) -> u64 {
    if !word_ok(first) || !word_ok(second) {
        return u64::MAX;
    }
    let cr3 = scheduler::current_task_cr3();
    // Both words keyed as a wait keys them: the second as the waiters moved
    // to it would have keyed it, had they waited there.
    let (Some(from), Some(to)) = (held_word(cr3, first), held_word(cr3, second)) else {
        return u64::MAX;
    };
    // Both lists, in the order of where they are, or the one if they are one.
    let (at, there) = (from.bucket(), to.bucket());
    let mut first_held = LISTS[at.min(there)].lock();
    let mut second_held = (at != there).then(|| LISTS[at.max(there)].lock_second());
    if let Some(expected) = expected {
        let now = {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { *(first as *const u32) }
        };
        if now != expected {
            return NOT_AS_SAID;
        }
    }
    let (from_list, to_list): (&mut Bucket, Option<&mut Bucket>) = match second_held.as_deref_mut() {
        None => (&mut *first_held, None),
        Some(second) if at < there => (&mut *first_held, Some(second)),
        Some(second) => (second, Some(&mut *first_held)),
    };
    unsafe {
        let woken = each_on(from_list, from, false, nr_wake, |list, t| {
            unlink(list, t);
            scheduler::unblock_task(t);
        });
        if from == to {
            return woken;
        }
        let moved = match to_list {
            None => each_on(from_list, from, false, nr_requeue, |list, t| {
                unlink(list, t);
                link(list, t, to);
            }),
            Some(to_list) => each_on(from_list, from, false, nr_requeue, |list, t| {
                unlink(list, t);
                link(to_list, t, to);
            }),
        };
        woken + moved
    }
}

/// Expire waits whose deadline has passed at `now`, and say when the next
/// one does: `u64::MAX` if none is waiting on a time. Called from the clock
/// (`clock::expire`).
///
/// An expired waiter is taken off its list, and says so in its record, which
/// it reads when it runs again.
pub fn check_timeouts(now: u64) -> u64 {
    // Interrupt context, so interrupts are already off.
    let mut next = u64::MAX;
    unsafe {
        for b in 0..BUCKETS {
            let mut list = LISTS[b].lock();
            let mut t = list.first;
            while let Some(r) = rec(t) {
                let (here, after, deadline) = (t as usize, r.next, r.deadline);
                if deadline != 0 && now >= deadline {
                    let lent_to = lends_to(here);
                    unlink(&mut list, here);
                    if let Some(me) = rec(here as u16) {
                        me.expired = true;
                        me.deadline = 0;
                    }
                    if let Some(owner) = lent_to {
                        scheduler::refresh_priority(owner);
                    }
                    scheduler::unblock_task(here);
                } else if deadline != 0 {
                    next = next.min(deadline);
                }
                t = after;
            }
        }
    }
    next
}

/// A task has died, or is being taken apart: it waits on nothing now. Where
/// it dies and not only at its reap — a wake counted for a task that will
/// never run again is one a waiter that would have was not given. What it
/// lent a holder, it lends no more; and what it held, nobody lends it.
pub fn cleanup_task(tid: usize) {
    let flags = irq_save();
    unsafe {
        let lent_to = lends_to(tid);
        if let Some(mut list) = list_of_waiter(tid) {
            unlink(&mut list, tid);
        }
        if let Some(me) = rec(tid as u16) {
            *me = PerTask::new();
        }
        if let Some(owner) = lent_to {
            scheduler::refresh_priority(owner);
        }
    }
    irq_restore(flags);
    owner_gone(tid);
}

/// The holder `t` lends its place to, if it waits to lock a word whose holder
/// is still there: the scheduler asks, as it asks whom a task is calling.
/// Read without the lock, which may be held by whoever is asking.
///
/// Interrupts off.
pub fn pi_waits_on(t: usize) -> Option<usize> {
    unsafe {
        let r = &scheduler::rec(t)?.futex;
        (r.waiting && r.pi && r.pi_owner != END).then_some(r.pi_owner as usize)
    }
}

/// [`pi_waits_on`], for a task about to stop waiting.
///
/// # Safety
/// Interrupts off.
unsafe fn lends_to(t: usize) -> Option<usize> {
    pi_waits_on(t)
}

/// Whether `from`, through holders waiting for holders, comes to `to` within
/// [`PI_DEPTH`] of them, or goes on further than that.
///
/// # Safety
/// Interrupts off.
unsafe fn leads_to(from: usize, to: usize) -> bool {
    let mut t = from;
    for _ in 0..PI_DEPTH {
        match pi_waits_on(t) {
            Some(next) if next == to => return true,
            Some(next) => t = next,
            None => return false,
        }
    }
    true
}

/// The waiters to lock `key`: the best placed, the first of equals; and how
/// many there are.
///
/// # Safety
/// `b` is `key`'s list, held.
unsafe fn pi_waiters(b: &mut Bucket, key: Key) -> (Option<usize>, u64) {
    let mut best: Option<(usize, (u8, u8))> = None;
    let n = unsafe {
        each_on(b, key, true, u64::MAX, |_, t| {
            let place = scheduler::place_of(t);
            if best.is_none_or(|(_, b)| scheduler::better(place, b)) {
                best = Some((t, place));
            }
        })
    };
    (best.map(|(t, _)| t), n)
}

/// The word at `addr` in `cr3`, reached by its frame, for the kernel to
/// write: the page made its address space's own first, as anything the
/// kernel writes for a program is, and none if it cannot be. Through the
/// frame, so that nothing here faults with a lock held.
///
/// # Safety
/// Interrupts off; the page was given its memory (`pi_word`, or the robust
/// list's walk, which reads it).
pub unsafe fn word_at(cr3: usize, addr: u64) -> Option<&'static AtomicU32> {
    unsafe {
        let _ = crate::paging::own(cr3, addr as usize);
        let writable = crate::paging::walk_flags(cr3, addr as usize)
            .is_some_and(|f| f & crate::paging::USER != 0 && f & crate::paging::WRITABLE != 0);
        let phys = crate::paging::translate(cr3, addr as usize)?;
        if !writable || phys + 4 > crate::paging::identity_end() {
            return None;
        }
        Some(&*(phys as *const AtomicU32))
    }
}

/// The priority-inheriting word at `addr` in the caller's memory, made
/// present, its own and kept so for the rest of the call — it is written —
/// and its key.
fn pi_word(cr3: usize, addr: u64) -> Option<Key> {
    scheduler::pin(addr, 4);
    let usable = unsafe {
        crate::paging::back_range(cr3, addr, 4, true).is_ok()
            && crate::paging::user_range_accessible(cr3, addr, 4, true)
    };
    if !usable {
        return None;
    }
    key_of(cr3, addr)
}

/// The priority-inheriting word `key`, holding `was`, is let go of by `from`,
/// its holder: by an unlock, or by its death (`died`). The best of its
/// waiters has it — the word says so, with FUTEX_WAITERS if more wait and
/// FUTEX_OWNER_DIED if `died` — and the rest wait for that one now, lending
/// it their places; with nobody waiting the word is nought, or
/// FUTEX_OWNER_DIED. What `from` was lent for it is worked out again from
/// what it still holds. False, with nothing done, if the word did not hold
/// `was` any more.
///
/// # Safety
/// `b` is `key`'s list, held.
unsafe fn hand_on(b: &mut Bucket, key: Key, word: &AtomicU32, was: u32, from: usize, died: bool) -> bool {
    unsafe {
        let (best, n) = pi_waiters(b, key);
        let dead = if died { PI_OWNER_DIED } else { 0 };
        let now = match best {
            Some(w) => w as u32 | dead | if n > 1 { PI_WAITERS } else { 0 },
            None => dead,
        };
        if word.compare_exchange(was, now, SeqCst, SeqCst).is_err() {
            return false;
        }
        if let Some(w) = best {
            unlink(b, w);
            if let Some(r) = rec(w as u16) {
                r.pi_got = true;
                r.pi_owner = END;
            }
            each_on(b, key, true, u64::MAX, |_, t| {
                if let Some(r) = rec(t as u16) {
                    r.pi_owner = w as u16;
                }
            });
            scheduler::unblock_task(w);
            scheduler::refresh_priority(w);
        }
        scheduler::refresh_priority(from);
        true
    }
}

/// `SYS_FUTEX_PI` ops 0 and 1: lock the priority-inheriting word at `addr`
/// for the caller — at once if nobody holds it, keeping FUTEX_OWNER_DIED if
/// its last holder died, and otherwise, unless `only_try`, by waiting until
/// its holder hands it over, lending the holder the caller's place while it
/// does. `span` is how long to wait at most (`clock::span`), 0 for no end.
///
/// The word is nobody's at nought and its holder's task id otherwise, as
/// Linux's; a C library takes it from nought in its program and calls this
/// only when somebody holds it. 0 when the caller has it; [`PI_BUSY`] for a
/// try that found it held; [`PI_DEADLOCK`] if the caller holds it or would
/// wait for itself through holders waiting for holders; [`PI_NO_OWNER`] if
/// the task it names is not there; [`TIMED_OUT`]; a signal's
/// `INTERRUPTED`; and `u64::MAX` for a word that is not one.
pub fn lock_pi(addr: u64, span: u64, only_try: bool) -> u64 {
    if !word_ok(addr) {
        return u64::MAX;
    }
    let me = scheduler::current_tid();
    let cr3 = scheduler::current_task_cr3();
    let Some(key) = pi_word(cr3, addr) else {
        return u64::MAX;
    };
    let deadline = if span == 0 { 0 } else { crate::clock::after(crate::clock::span(span)) };
    loop {
        let mut list = list_of(key);
        let Some(word) = (unsafe { word_at(cr3, addr) }) else {
            return u64::MAX;
        };
        let was = word.load(SeqCst);
        let owner = (was & PI_OWNER) as usize;
        if owner == 0 {
            // Nobody's, or its holder died: the caller's, keeping that it
            // died, and saying whether others still wait.
            let (_, n) = unsafe { pi_waiters(&mut list, key) };
            let now = me as u32 | (was & PI_OWNER_DIED) | if n > 0 { PI_WAITERS } else { 0 };
            if word.compare_exchange(was, now, SeqCst, SeqCst).is_ok() {
                return 0;
            }
            continue;
        }
        if owner == me {
            return PI_DEADLOCK;
        }
        if only_try {
            return PI_BUSY;
        }
        if !scheduler::task_is_live(owner) {
            return PI_NO_OWNER;
        }
        if unsafe { leads_to(owner, me) } {
            return PI_DEADLOCK;
        }
        // Somebody waits, so that the holder's unlock comes here.
        if was & PI_WAITERS == 0 && word.compare_exchange(was, was | PI_WAITERS, SeqCst, SeqCst).is_err() {
            continue;
        }
        if deadline != 0 && crate::clock::now() >= deadline {
            return TIMED_OUT;
        }
        if crate::signal::ends_wait(me) {
            return crate::signal::INTERRUPTED;
        }
        unsafe {
            let Some(r) = rec(me as u16) else {
                return u64::MAX;
            };
            r.deadline = deadline;
            r.expired = false;
            r.interrupted = false;
            r.pi = true;
            r.pi_owner = owner as u16;
            r.pi_got = false;
            link(&mut list, me, key);
        }
        scheduler::block_task(me);
        if deadline != 0 {
            crate::clock::due(deadline);
        }
        // The holder runs at the caller's place while it waits, where that
        // is better than its own, and so does whatever it waits for.
        scheduler::refresh_priority(owner);
        drop(list);
        scheduler::yield_now();

        // Handed the word; or its deadline or a signal, each of which took it
        // off its list and said so; or woken for nothing, and it looks again.
        let flags = irq_save();
        let (got, expired, interrupted) = unsafe {
            let lent_to = lends_to(me);
            if let Some(mut list) = list_of_waiter(me) {
                unlink(&mut list, me);
            }
            let why = match rec(me as u16) {
                Some(r) => {
                    let why = (r.pi_got, r.expired, r.interrupted);
                    *r = PerTask::new();
                    why
                }
                None => (false, false, false),
            };
            if let Some(owner) = lent_to {
                scheduler::refresh_priority(owner);
            }
            why
        };
        irq_restore(flags);
        if got {
            return 0;
        }
        if expired {
            return TIMED_OUT;
        }
        if interrupted {
            return crate::signal::INTERRUPTED;
        }
    }
}

/// `SYS_FUTEX_PI` op 2: the caller lets go of the priority-inheriting word at
/// `addr`, which goes to the best of whoever waits for it ([`hand_on`]). 0;
/// [`PI_NOT_YOURS`] if the caller does not hold it; `u64::MAX` for a word
/// that is not one.
pub fn unlock_pi(addr: u64) -> u64 {
    if !word_ok(addr) {
        return u64::MAX;
    }
    let me = scheduler::current_tid();
    let cr3 = scheduler::current_task_cr3();
    let Some(key) = pi_word(cr3, addr) else {
        return u64::MAX;
    };
    loop {
        let mut list = list_of(key);
        let Some(word) = (unsafe { word_at(cr3, addr) }) else {
            return u64::MAX;
        };
        let was = word.load(SeqCst);
        if (was & PI_OWNER) as usize != me {
            return PI_NOT_YOURS;
        }
        if unsafe { hand_on(&mut list, key, word, was, me, false) } {
            return 0;
        }
    }
}

/// The word at `at` in `cr3`, held by `tid`, which has died or become another
/// program: if anybody waits in the kernel to lock it, the best of them has
/// it, told its holder died. False if nobody does, and the robust list's
/// walk marks it as it marks any other.
pub fn pi_owner_died(cr3: usize, at: u64, tid: usize) -> bool {
    let Some(key) = key_of(cr3, at) else {
        return false;
    };
    let mut list = list_of(key);
    unsafe {
        if pi_waiters(&mut list, key).1 == 0 {
            return false;
        }
        loop {
            let Some(word) = word_at(cr3, at) else {
                return false;
            };
            let was = word.load(SeqCst);
            if (was & PI_OWNER) as usize != tid {
                return false;
            }
            if hand_on(&mut list, key, word, was, tid, true) {
                return true;
            }
        }
    }
}

/// `tid` has died or become another program: whoever waits to lock a word it
/// held that its robust list did not say — a mutex that is not robust —
/// lends it nothing more, since its number may be somebody else's by the time
/// they stop waiting; and it runs at no place it was lent for one.
pub fn owner_gone(tid: usize) {
    let mut lent = false;
    unsafe {
        for b in 0..BUCKETS {
            let list = LISTS[b].lock();
            let mut t = list.first;
            while let Some(r) = rec(t) {
                if r.pi && r.pi_owner == tid as u16 {
                    r.pi_owner = END;
                    lent = true;
                }
                t = r.next;
            }
        }
        if lent {
            scheduler::refresh_priority(tid);
        }
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
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}
