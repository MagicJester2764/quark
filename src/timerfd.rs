//! Timers a program can wait on with everything else it waits on.
//!
//! A descriptor that becomes readable when its deadline passes, and reads as
//! the number of times it has. That shape exists because a program's event
//! loop already waits on descriptors: a toolkit with a cursor to blink and a
//! key to repeat would otherwise need a second kind of waiting, and the two
//! would have to be reconciled at every call.
//!
//! Its times are the clock's (`clock.rs`): nanoseconds, kept exactly and
//! fired when they are due where the machine has a timer to fire them with,
//! and on the next tick where it has not.

use crate::scheduler;
use crate::task::{FdKind, FD_MOST};
use crate::waitlist::{self, On, Waiters};

struct Timer {
    creator: usize,
    refs: usize,
    /// When it next expires, in the clock's nanoseconds, or 0 for a timer
    /// that is not armed.
    deadline: u64,
    /// Nanoseconds between expirations, or 0 for a timer that fires once.
    interval: u64,
    /// Expirations not yet read. A read takes them all and clears it, which
    /// is what makes a slow reader see "it fired four times" rather than four
    /// wake-ups it has to count itself.
    count: u64,
    /// Who is waiting for it to fire (`waitlist.rs`).
    waiters: Waiters,
}

/// Every timer, by its number: made when a program makes one, given back
/// when nothing names it (`table.rs`). Sixteen for the whole machine, they
/// were, and four waiting on each.
static mut TIMERS: crate::table::Table<Timer> = crate::table::Table::new(crate::table::MOST);

/// The timers' lock (`sync::RANK_TIMER`): the table, every timer in it and
/// the lists of those waiting on them.
static LOCK: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(crate::sync::RANK_TIMER, "the timers", ());

/// # Safety
/// [`LOCK`] held.
#[inline(always)]
unsafe fn timers() -> &'static mut crate::table::Table<Timer> {
    unsafe { &mut *core::ptr::addr_of_mut!(TIMERS) }
}



pub fn create(creator: usize) -> Option<usize> {
    if !crate::reclaim::may_make() {
        return None;
    }
    let held = LOCK.lock();
    let made = unsafe {
        timers().lowest_free(0).filter(|&i| {
            let t = Timer { creator, refs: 0, deadline: 0, interval: 0, count: 0, waiters: Waiters::NONE };
            timers().fill_at(i, t).is_ok()
        })
    };
    drop(held);
    made
}

pub fn retain(timer: usize) {
    let held = LOCK.lock();
    unsafe {
        if let Some(t) = timers().get(timer) {
            t.refs += 1;
        }
    }
    drop(held);
}

pub fn release(timer: usize) {
    let held = LOCK.lock();
    unsafe {
        if let Some(t) = timers().get(timer) {
            t.refs = t.refs.saturating_sub(1);
            if t.refs == 0 {
                gone(timer);
            }
        }
    }
    drop(held);
}

/// Timer `timer` goes: anybody still on its list looks again, and finds
/// nothing.
///
/// # Safety
/// [`LOCK`] held.
unsafe fn gone(timer: usize) {
    unsafe {
        if let Some(t) = timers().get(timer) {
            waitlist::wake_all(&mut t.waiters);
        }
        timers().empty(timer);
    }
}

pub fn cleanup_orphans(creator: usize) {
    let held = LOCK.lock();
    unsafe {
        let mut at = 0;
        while let Some(i) = timers().next_used(at) {
            at = i + 1;
            if timers().get(i).is_some_and(|t| t.creator == creator && t.refs == 0) {
                gone(i);
            }
        }
    }
    drop(held);
}

/// Arm or disarm. `first` is nanoseconds from now (0 disarms), `interval`
/// nanoseconds between expirations after that.
pub fn set(timer: usize, first: u64, interval: u64) -> bool {
    let held = LOCK.lock();
    let ok = unsafe {
        match timers().get(timer) {
            Some(t) => {
                t.deadline = if first == 0 { 0 } else { crate::clock::after(first) };
                t.interval = interval;
                t.count = 0;
                if t.deadline != 0 {
                    crate::clock::due(t.deadline);
                }
                true
            }
            None => false,
        }
    };
    drop(held);
    ok
}

/// What is left: nanoseconds until the next expiration, and the interval.
pub fn get(timer: usize) -> Option<(u64, u64)> {
    let now = crate::clock::now();
    let held = LOCK.lock();
    let out = unsafe { timers().get(timer).map(|t| (t.deadline.saturating_sub(now), t.interval)) };
    drop(held);
    out
}

/// Count what `t` has fired by `now` and has not been counted for, and set
/// it for the next time. Whether there was anything to count.
///
/// Done by the clock when it looks ([`expire`]), which is what makes a
/// timer readable and wakes whoever is waiting for that; and by a read
/// ([`take`]), which is answered as of when it asks — how many times a
/// timer has fired is a matter of what time it is, not of when the clock
/// last looked. A read takes what it counts, so nobody is owed a wake for
/// it.
fn catch_up(t: &mut Timer, now: u64) -> bool {
    if t.deadline == 0 || now < t.deadline {
        return false;
    }
    if t.interval > 0 {
        // Every interval that has gone by is counted, and the next deadline
        // is the next one after now on the timer's own beat: a timer that
        // fell behind does not fire in a burst catching up, and one that is
        // looked at late does not drift.
        let periods = (now - t.deadline) / t.interval + 1;
        t.count = t.count.saturating_add(periods);
        t.deadline = t.deadline.saturating_add(periods.saturating_mul(t.interval));
    } else {
        t.count = t.count.saturating_add(1);
        t.deadline = 0;
    }
    true
}

/// How many times it has fired since the last read.
pub fn pending(timer: usize) -> u64 {
    let held = LOCK.lock();
    let n = unsafe { timers().get(timer).map_or(0, |t| t.count) };
    drop(held);
    n
}

/// Take the count, or `None` if it has not fired yet.
pub fn take(timer: usize) -> Option<u64> {
    let now = crate::clock::now();
    let held = LOCK.lock();
    let out = unsafe {
        match timers().get(timer) {
            Some(t) => {
                catch_up(t, now);
                (t.count > 0).then(|| core::mem::replace(&mut t.count, 0))
            }
            None => None,
        }
    };
    drop(held);
    out
}

/// `tid` off the list of the timer it waits on, if it waits on one: under
/// the timers' lock (`waitlist::forget`).
///
/// # Safety
/// Interrupts off, and [`LOCK`] not held.
pub unsafe fn forget(tid: usize) -> bool {
    let _held = LOCK.lock();
    unsafe {
        waitlist::forget_held(tid, |on| match on {
            On::Timer(timer) => Some(timers().get(timer as usize).map(|t| &mut t.waiters)),
            _ => None,
        })
    }
}

/// Wait for it to fire. False when there is no need, it is gone, or a
/// signal has ended the wait before it began.
pub fn wait(timer: usize) -> bool {
    let tid = scheduler::current_tid();
    let held = LOCK.lock();
    let parked = unsafe {
        match timers().get(timer) {
            Some(t) if t.count == 0 && !crate::signal::ends_wait(tid) => {
                waitlist::add(&mut t.waiters, tid, On::Timer(timer as u32));
                scheduler::block_task(tid);
                true
            }
            _ => false,
        }
    };
    drop(held);
    if parked {
        scheduler::yield_now();
    }
    parked
}

/// Fire what is due at `now`, and say when the next one is: the earliest
/// deadline left, or `u64::MAX` if no timer is armed.
///
/// Called with interrupts already off, from the clock (`clock::expire`).
pub fn expire(now: u64) -> u64 {
    let mut fired = false;
    let mut next = u64::MAX;
    let held = LOCK.lock();
    unsafe {
        let mut at = 0;
        while let Some(i) = timers().next_used(at) {
            at = i + 1;
            let Some(t) = timers().get(i) else { continue };
            if catch_up(t, now) {
                fired = true;
                waitlist::wake_all(&mut t.waiters);
            }
            if t.deadline != 0 {
                next = next.min(t.deadline);
            }
        }
    }
    drop(held);
    if fired {
        crate::pollset::note_timer();
    }
    next
}

/// The timer a descriptor names.
pub fn of_fd(tid: usize, fd: usize) -> Option<usize> {
    if fd >= FD_MOST {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        FdKind::Timer { timer } => Some(timer),
        _ => None,
    }
}
