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

use crate::scheduler;
use crate::sync::IrqSpinLock;

/// Lists a waiter is on, by a hash of its word's key.
const BUCKETS: usize = 256;

/// The end of a list.
const END: u16 = u16::MAX;

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
fn key_of(cr3: usize, addr: u64) -> Option<Key> {
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
    /// The tasks either side of it on its list.
    next: u16,
    prev: u16,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask { waiting: false, key: NO_KEY, deadline: 0, expired: false, interrupted: false, next: END, prev: END }
    }
}

/// A list's first and last.
#[derive(Clone, Copy)]
struct Bucket {
    first: u16,
    last: u16,
}

struct FutexState {
    buckets: [Bucket; BUCKETS],
}

/// The lists, and through them every waiter's record: whoever changes
/// either holds this.
static FUTEX: IrqSpinLock<FutexState> =
    IrqSpinLock::new(FutexState { buckets: [Bucket { first: END, last: END }; BUCKETS] });

/// Task `t`'s record, unless it is the end of a list or no task.
///
/// # Safety
/// `FUTEX` is held, and with it interrupts are off.
unsafe fn rec(t: u16) -> Option<&'static mut PerTask> {
    if t == END {
        return None;
    }
    unsafe { scheduler::rec(t as usize).map(|r| &mut r.futex) }
}

/// `tid` waits on `key`, last on its list.
///
/// # Safety
/// `FUTEX` is held, and `tid` is on no list.
unsafe fn link(state: &mut FutexState, tid: usize, key: Key) {
    unsafe {
        let b = &mut state.buckets[key.bucket()];
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

/// `tid` is off its list, and waits on nothing.
///
/// # Safety
/// `FUTEX` is held.
unsafe fn unlink(state: &mut FutexState, tid: usize) {
    unsafe {
        let Some(me) = rec(tid as u16).filter(|me| me.waiting) else { return };
        let b = &mut state.buckets[me.key.bucket()];
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

/// The waiters on `key`'s list that wait on `key`, in the order they began
/// to, up to `most` of them: handed to `each`, which may take them off.
///
/// # Safety
/// `FUTEX` is held.
unsafe fn each_on(state: &mut FutexState, key: Key, most: u64, mut each: impl FnMut(&mut FutexState, usize)) -> u64 {
    unsafe {
        let mut t = state.buckets[key.bucket()].first;
        let mut done = 0u64;
        while done < most {
            let Some(r) = rec(t) else { break };
            let (next, mine) = (r.next, r.key == key);
            if mine {
                each(state, t as usize);
                done += 1;
            }
            t = next;
        }
        done
    }
}

const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

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
    let usable = unsafe {
        crate::paging::back_range(cr3, addr, 4, false).is_ok()
            && crate::paging::user_range_accessible(cr3, addr, 4, false)
    };
    if !usable {
        return None;
    }
    scheduler::pin(addr, 4);
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

    let mut state = FUTEX.lock();

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
        link(&mut state, tid, key);
    }

    // Block the task while holding the lock to prevent wake races
    scheduler::block_task(tid);
    if deadline != 0 {
        crate::clock::due(deadline);
    }
    drop(state);

    // Yield to let the scheduler pick another task
    scheduler::yield_now();

    // Woken: by a wake, which took it off its list; by its deadline or a
    // signal, which did too and said so; or by something else entirely,
    // which did not — it is taken off now, either way.
    let mut state = FUTEX.lock();
    let (timed_out, interrupted) = unsafe {
        unlink(&mut state, tid);
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
    drop(state);

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
    let mut state = FUTEX.lock();
    unsafe {
        if !rec(tid as u16).is_some_and(|me| me.waiting) {
            return false;
        }
        unlink(&mut state, tid);
        if let Some(me) = rec(tid as u16) {
            me.interrupted = true;
        }
    }
    scheduler::unblock_task(tid);
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
    let mut state = FUTEX.lock();
    unsafe {
        each_on(&mut state, key, max_wake, |state, t| {
            unlink(state, t);
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
    let mut state = FUTEX.lock();
    if let Some(expected) = expected {
        let now = {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { *(first as *const u32) }
        };
        if now != expected {
            return NOT_AS_SAID;
        }
    }
    unsafe {
        let woken = each_on(&mut state, from, nr_wake, |state, t| {
            unlink(state, t);
            scheduler::unblock_task(t);
        });
        if from == to {
            return woken;
        }
        let moved = each_on(&mut state, from, nr_requeue, |state, t| {
            unlink(state, t);
            link(state, t, to);
        });
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
    let mut state = FUTEX.lock();
    unsafe {
        for b in 0..BUCKETS {
            let mut t = state.buckets[b].first;
            while let Some(r) = rec(t) {
                let (here, after, deadline) = (t as usize, r.next, r.deadline);
                if deadline != 0 && now >= deadline {
                    unlink(&mut state, here);
                    if let Some(me) = rec(here as u16) {
                        me.expired = true;
                        me.deadline = 0;
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
/// never run again is one a waiter that would have was not given.
pub fn cleanup_task(tid: usize) {
    let mut state = FUTEX.lock();
    unsafe {
        unlink(&mut state, tid);
        if let Some(me) = rec(tid as u16) {
            *me = PerTask::new();
        }
    }
}
