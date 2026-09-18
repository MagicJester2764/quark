//! A counter with a descriptor: `eventfd`.
//!
//! One task adds to it, another waits until it is not zero and takes what is
//! there. That is all of it — and it is what every main loop in this system's
//! future is built on. glib reaches for `eventfd` before anything else to wake
//! a sleeping loop, and falls back to a pipe only when the call fails; so does
//! libwayland's, and so does GTK's.
//!
//! A pipe would do the same job, at the cost of two descriptors, a ring buffer
//! and a byte per wake that somebody has to drain. A counter is the honest
//! shape: the reader wants to know *that* something happened and how often,
//! not what was written.
//!
//! Semaphore mode is the one wrinkle. A plain read takes the whole count and
//! leaves zero; a semaphore read takes one and leaves the rest, which is what
//! turns the same object into "wait for one of N things".

use crate::scheduler;

pub const MAX_EVENTS: usize = 16;
const MAX_WAITERS: usize = 4;

/// The largest value a write may leave behind. Linux reserves the top value so
/// that a write of `u64::MAX` is always an error rather than a wrap.
const MAX_COUNT: u64 = u64::MAX - 1;

#[derive(Clone, Copy)]
struct Event {
    in_use: bool,
    creator: usize,
    refs: usize,
    count: u64,
    /// A read takes one rather than all of it.
    semaphore: bool,
    waiters: [usize; MAX_WAITERS],
    nwaiters: usize,
}

const NO_EVENT: Event = Event {
    in_use: false,
    creator: 0,
    refs: 0,
    count: 0,
    semaphore: false,
    waiters: [0; MAX_WAITERS],
    nwaiters: 0,
};

static mut EVENTS: [Event; MAX_EVENTS] = [NO_EVENT; MAX_EVENTS];

fn events() -> &'static mut [Event; MAX_EVENTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(EVENTS) }
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

pub fn create(creator: usize, initial: u64, semaphore: bool) -> Option<usize> {
    if initial > MAX_COUNT {
        return None;
    }
    let flags = irq_save();
    let out = (0..MAX_EVENTS).find(|&i| !events()[i].in_use).inspect(|&i| {
        events()[i] = NO_EVENT;
        events()[i].in_use = true;
        events()[i].creator = creator;
        events()[i].count = initial;
        events()[i].semaphore = semaphore;
    });
    irq_restore(flags);
    out
}

pub fn retain(ev: usize) {
    if ev >= MAX_EVENTS {
        return;
    }
    let flags = irq_save();
    if events()[ev].in_use {
        events()[ev].refs += 1;
    }
    irq_restore(flags);
}

pub fn release(ev: usize) {
    if ev >= MAX_EVENTS {
        return;
    }
    let flags = irq_save();
    let e = &mut events()[ev];
    if e.in_use {
        e.refs = e.refs.saturating_sub(1);
        if e.refs == 0 {
            *e = NO_EVENT;
        }
    }
    irq_restore(flags);
}

/// Throw away counters a task made and never installed anywhere.
pub fn cleanup_orphans(creator: usize) {
    let flags = irq_save();
    for e in events().iter_mut() {
        if e.in_use && e.creator == creator && e.refs == 0 {
            *e = NO_EVENT;
        }
    }
    irq_restore(flags);
}

pub fn readable(ev: usize) -> bool {
    if ev >= MAX_EVENTS {
        return false;
    }
    let flags = irq_save();
    let r = events()[ev].in_use && events()[ev].count > 0;
    irq_restore(flags);
    r
}

/// Writable while there is room for one more, which is every counter that is
/// not at its ceiling — so, in practice, always.
pub fn writable(ev: usize) -> bool {
    if ev >= MAX_EVENTS {
        return false;
    }
    let flags = irq_save();
    let w = events()[ev].in_use && events()[ev].count < MAX_COUNT;
    irq_restore(flags);
    w
}

/// Take what is there. `None` when the counter is zero, which is what the
/// caller turns into a wait or into `EAGAIN`.
pub fn take(ev: usize) -> Option<u64> {
    if ev >= MAX_EVENTS {
        return None;
    }
    let flags = irq_save();
    let out = {
        let e = &mut events()[ev];
        if !e.in_use || e.count == 0 {
            None
        } else if e.semaphore {
            e.count -= 1;
            Some(1)
        } else {
            let n = e.count;
            e.count = 0;
            Some(n)
        }
    };
    irq_restore(flags);
    if out.is_some() {
        crate::pollset::note_event();
    }
    out
}

/// Add to the counter and wake whoever is waiting. `false` when it would go
/// past the ceiling, which the caller turns into a wait or into `EAGAIN`.
pub fn add(ev: usize, n: u64) -> bool {
    if ev >= MAX_EVENTS || n == 0 {
        return false;
    }
    let mut wake = [0usize; MAX_WAITERS];
    let mut nwake = 0;
    let flags = irq_save();
    let ok = {
        let e = &mut events()[ev];
        if !e.in_use || MAX_COUNT - e.count < n {
            false
        } else {
            e.count += n;
            for i in 0..e.nwaiters {
                wake[nwake] = e.waiters[i];
                nwake += 1;
            }
            e.nwaiters = 0;
            true
        }
    };
    irq_restore(flags);
    if ok {
        for &tid in &wake[..nwake] {
            scheduler::unblock_task(tid);
        }
        crate::pollset::note_event();
    }
    ok
}

/// Park until the counter is not zero. `false` when there was no room to be
/// recorded as a waiter — which must not become a wait, since an unrecorded
/// waiter is never woken.
pub fn wait(ev: usize) -> bool {
    if ev >= MAX_EVENTS {
        return false;
    }
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let parked = {
        let e = &mut events()[ev];
        if !e.in_use || e.count > 0 || e.nwaiters >= MAX_WAITERS {
            false
        } else {
            e.waiters[e.nwaiters] = tid;
            e.nwaiters += 1;
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
