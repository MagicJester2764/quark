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
use crate::task::{FdKind, MAX_FDS};

pub const MAX_TIMERS: usize = 16;
const MAX_WAITERS: usize = 4;

#[derive(Clone, Copy)]
struct Timer {
    in_use: bool,
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
    waiters: [usize; MAX_WAITERS],
    nwaiters: usize,
}

const NO_TIMER: Timer = Timer {
    in_use: false,
    creator: 0,
    refs: 0,
    deadline: 0,
    interval: 0,
    count: 0,
    waiters: [0; MAX_WAITERS],
    nwaiters: 0,
};

static mut TIMERS: [Timer; MAX_TIMERS] = [NO_TIMER; MAX_TIMERS];

fn timers() -> &'static mut [Timer; MAX_TIMERS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TIMERS) }
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

pub fn create(creator: usize) -> Option<usize> {
    let flags = irq_save();
    let out = (0..MAX_TIMERS).find(|&i| !timers()[i].in_use).inspect(|&i| {
        timers()[i] = NO_TIMER;
        timers()[i].in_use = true;
        timers()[i].creator = creator;
    });
    irq_restore(flags);
    out
}

pub fn retain(timer: usize) {
    if timer >= MAX_TIMERS {
        return;
    }
    let flags = irq_save();
    if timers()[timer].in_use {
        timers()[timer].refs += 1;
    }
    irq_restore(flags);
}

pub fn release(timer: usize) {
    if timer >= MAX_TIMERS {
        return;
    }
    let flags = irq_save();
    let t = &mut timers()[timer];
    if t.in_use {
        t.refs = t.refs.saturating_sub(1);
        if t.refs == 0 {
            *t = NO_TIMER;
        }
    }
    irq_restore(flags);
}

pub fn cleanup_orphans(creator: usize) {
    let flags = irq_save();
    for t in timers().iter_mut() {
        if t.in_use && t.creator == creator && t.refs == 0 {
            *t = NO_TIMER;
        }
    }
    irq_restore(flags);
}

/// Arm or disarm. `first` is nanoseconds from now (0 disarms), `interval`
/// nanoseconds between expirations after that.
pub fn set(timer: usize, first: u64, interval: u64) -> bool {
    if timer >= MAX_TIMERS {
        return false;
    }
    let flags = irq_save();
    let ok = {
        let t = &mut timers()[timer];
        if !t.in_use {
            false
        } else {
            t.deadline = if first == 0 { 0 } else { crate::clock::after(first) };
            t.interval = interval;
            t.count = 0;
            if t.deadline != 0 {
                crate::clock::due(t.deadline);
            }
            true
        }
    };
    irq_restore(flags);
    ok
}

/// What is left: nanoseconds until the next expiration, and the interval.
pub fn get(timer: usize) -> Option<(u64, u64)> {
    if timer >= MAX_TIMERS {
        return None;
    }
    let now = crate::clock::now();
    let flags = irq_save();
    let out = {
        let t = &timers()[timer];
        if !t.in_use {
            None
        } else {
            let left = if t.deadline > now { t.deadline - now } else { 0 };
            Some((left, t.interval))
        }
    };
    irq_restore(flags);
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
    if timer >= MAX_TIMERS {
        return 0;
    }
    let flags = irq_save();
    let n = if timers()[timer].in_use { timers()[timer].count } else { 0 };
    irq_restore(flags);
    n
}

/// Take the count, or `None` if it has not fired yet.
pub fn take(timer: usize) -> Option<u64> {
    if timer >= MAX_TIMERS {
        return None;
    }
    let now = crate::clock::now();
    let flags = irq_save();
    let out = {
        let t = &mut timers()[timer];
        if t.in_use {
            catch_up(t, now);
        }
        if !t.in_use || t.count == 0 {
            None
        } else {
            let n = t.count;
            t.count = 0;
            Some(n)
        }
    };
    irq_restore(flags);
    out
}

/// A task parked on this timer has died: it is waiting for nothing now.
pub fn forget_waiter(timer: usize, tid: usize) -> bool {
    if timer >= MAX_TIMERS {
        return false;
    }
    let flags = irq_save();
    let t = &mut timers()[timer];
    let found = t.in_use && crate::pipe::forget_in(&mut t.waiters, &mut t.nwaiters, tid);
    irq_restore(flags);
    found
}

/// Wait for it to fire. False when there was no room to be recorded as a
/// waiter, which must not become a wait.
pub fn wait(timer: usize) -> bool {
    if timer >= MAX_TIMERS {
        return false;
    }
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let parked = {
        let t = &mut timers()[timer];
        if !t.in_use || t.count > 0 || t.nwaiters >= MAX_WAITERS || crate::signal::ends_wait(tid) {
            false
        } else {
            t.waiters[t.nwaiters] = tid;
            t.nwaiters += 1;
            scheduler::block_task(tid);
            true
        }
    };
    irq_restore(flags);
    if parked {
        scheduler::yield_now();
    }
    parked
}

/// Fire what is due at `now`, and say when the next one is: the earliest
/// deadline left, or `u64::MAX` if no timer is armed.
///
/// Called with interrupts already off, from the clock (`clock::expire`), so
/// it takes the waiters out and wakes them after — waking is a scheduler
/// operation and this is not the place for one.
pub fn expire(now: u64) -> u64 {
    let mut wake = [0usize; MAX_TIMERS * MAX_WAITERS];
    let mut n = 0;
    let mut fired = false;
    let mut next = u64::MAX;
    for t in timers().iter_mut() {
        if !t.in_use {
            continue;
        }
        if catch_up(t, now) {
            fired = true;
            for i in 0..t.nwaiters {
                wake[n] = t.waiters[i];
                n += 1;
            }
            t.nwaiters = 0;
        }
        if t.deadline != 0 {
            next = next.min(t.deadline);
        }
    }
    for &tid in &wake[..n] {
        scheduler::unblock_task(tid);
    }
    if fired {
        crate::pollset::note_timer();
    }
    next
}

/// The timer a descriptor names.
pub fn of_fd(tid: usize, fd: usize) -> Option<usize> {
    if fd >= MAX_FDS {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        FdKind::Timer { timer } => Some(timer),
        _ => None,
    }
}
