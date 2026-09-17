//! Timers a program can wait on with everything else it waits on.
//!
//! A descriptor that becomes readable when its deadline passes, and reads as
//! the number of times it has. That shape exists because a program's event
//! loop already waits on descriptors: a toolkit with a cursor to blink and a
//! key to repeat would otherwise need a second kind of waiting, and the two
//! would have to be reconciled at every call.
//!
//! The resolution is the tick, which is ten milliseconds here. A timer asked
//! for less than that gets the next tick, because that is the next time
//! anything happens.

use crate::scheduler;
use crate::task::{FdKind, MAX_FDS};

pub const MAX_TIMERS: usize = 16;
const MAX_WAITERS: usize = 4;

#[derive(Clone, Copy)]
struct Timer {
    in_use: bool,
    creator: usize,
    refs: usize,
    /// The tick it next expires on, or 0 for a timer that is not armed.
    deadline: u64,
    /// Ticks between expirations, or 0 for a timer that fires once.
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

/// Arm or disarm. `first` is ticks from now (0 disarms), `interval` ticks
/// between expirations after that.
pub fn set(timer: usize, first: u64, interval: u64) -> bool {
    if timer >= MAX_TIMERS {
        return false;
    }
    let now = crate::pit::ticks();
    let flags = irq_save();
    let ok = {
        let t = &mut timers()[timer];
        if !t.in_use {
            false
        } else {
            t.deadline = if first == 0 { 0 } else { now + first };
            t.interval = interval;
            t.count = 0;
            true
        }
    };
    irq_restore(flags);
    ok
}

/// What is left: ticks until the next expiration, and the interval.
pub fn get(timer: usize) -> Option<(u64, u64)> {
    if timer >= MAX_TIMERS {
        return None;
    }
    let now = crate::pit::ticks();
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
    let flags = irq_save();
    let out = {
        let t = &mut timers()[timer];
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
        if !t.in_use || t.count > 0 || t.nwaiters >= MAX_WAITERS {
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

/// The tick handler: fire what is due.
///
/// Called with interrupts already off, from the timer interrupt, so it takes
/// the waiters out and wakes them after — waking is a scheduler operation and
/// this is not the place for one.
pub fn tick(now: u64) {
    let mut wake = [0usize; MAX_TIMERS * MAX_WAITERS];
    let mut n = 0;
    let mut fired = false;
    for t in timers().iter_mut() {
        if !t.in_use || t.deadline == 0 || now < t.deadline {
            continue;
        }
        t.count += 1;
        fired = true;
        if t.interval > 0 {
            // However long the reader took, the next deadline is the next one
            // after now: a timer that fell behind does not then fire in a
            // burst catching up.
            t.deadline = now + t.interval;
        } else {
            t.deadline = 0;
        }
        for i in 0..t.nwaiters {
            wake[n] = t.waiters[i];
            n += 1;
        }
        t.nwaiters = 0;
    }
    for &tid in &wake[..n] {
        scheduler::unblock_task(tid);
    }
    if fired {
        crate::pollset::note_timer();
    }
}

/// The timer a descriptor names.
pub fn of_fd(tid: usize, fd: usize) -> Option<usize> {
    if fd >= MAX_FDS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => match t.fds[fd] {
                FdKind::Timer { timer } => Some(timer),
                _ => None,
            },
            None => None,
        }
    };
    irq_restore(flags);
    out
}
