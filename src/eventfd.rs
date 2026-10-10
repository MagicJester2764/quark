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
use crate::waitlist::{self, On, Waiters};

/// The largest value a write may leave behind. Linux reserves the top value so
/// that a write of `u64::MAX` is always an error rather than a wrap.
const MAX_COUNT: u64 = u64::MAX - 1;

struct Event {
    creator: usize,
    refs: usize,
    count: u64,
    /// A read takes one rather than all of it.
    semaphore: bool,
    /// Who is waiting for it not to be zero (`waitlist.rs`).
    waiters: Waiters,
}

/// Every counter, by its number: made when a program makes one, given back
/// when nothing names it (`table.rs`). Sixteen for the whole machine, they
/// were, and four waiting on each.
static mut EVENTS: crate::table::Table<Event> = crate::table::Table::new(crate::table::MOST);

/// The counters' lock (`sync::RANK_EVENT`): the table, every counter in it
/// and the lists of those waiting on them.
static LOCK: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(crate::sync::RANK_EVENT, "the counters", ());

/// # Safety
/// [`LOCK`] held.
#[inline(always)]
unsafe fn events() -> &'static mut crate::table::Table<Event> {
    unsafe { &mut *core::ptr::addr_of_mut!(EVENTS) }
}

pub fn create(creator: usize, initial: u64, semaphore: bool) -> Option<usize> {
    if initial > MAX_COUNT || !crate::reclaim::may_make() {
        return None;
    }
    let held = LOCK.lock();
    let made = unsafe {
        events().lowest_free(0).filter(|&i| {
            events().fill_at(i, Event { creator, refs: 0, count: initial, semaphore, waiters: Waiters::NONE }).is_ok()
        })
    };
    drop(held);
    made
}

pub fn retain(ev: usize) {
    let _held = LOCK.lock();
    unsafe {
        if let Some(e) = events().get(ev) {
            e.refs += 1;
        }
    }
}

pub fn release(ev: usize) {
    let _held = LOCK.lock();
    unsafe {
        if let Some(e) = events().get(ev) {
            e.refs = e.refs.saturating_sub(1);
            if e.refs == 0 {
                gone(ev);
            }
        }
    }
}

/// Counter `ev` goes: anybody still on its list — which a waiter's own
/// reference should have made nobody — looks again, and finds nothing.
///
/// # Safety
/// [`LOCK`] held.
unsafe fn gone(ev: usize) {
    unsafe {
        if let Some(e) = events().get(ev) {
            waitlist::wake_all(&mut e.waiters);
        }
        events().empty(ev);
    }
}

/// Throw away counters a task made and never installed anywhere.
pub fn cleanup_orphans(creator: usize) {
    let _held = LOCK.lock();
    unsafe {
        let mut at = 0;
        while let Some(ev) = events().next_used(at) {
            at = ev + 1;
            if events().get(ev).is_some_and(|e| e.creator == creator && e.refs == 0) {
                gone(ev);
            }
        }
    }
}

pub fn readable(ev: usize) -> bool {
    let _held = LOCK.lock();
    unsafe { events().get(ev).is_some_and(|e| e.count > 0) }
}

/// Writable while there is room for one more, which is every counter that is
/// not at its ceiling — so, in practice, always.
pub fn writable(ev: usize) -> bool {
    let _held = LOCK.lock();
    unsafe { events().get(ev).is_some_and(|e| e.count < MAX_COUNT) }
}

/// Take what is there. `None` when the counter is zero, which is what the
/// caller turns into a wait or into `EAGAIN`.
pub fn take(ev: usize) -> Option<u64> {
    let held = LOCK.lock();
    let out = unsafe {
        match events().get(ev) {
            Some(e) if e.count > 0 && e.semaphore => {
                e.count -= 1;
                Some(1)
            }
            Some(e) if e.count > 0 => Some(core::mem::replace(&mut e.count, 0)),
            _ => None,
        }
    };
    drop(held);
    if out.is_some() {
        crate::pollset::note_event();
    }
    out
}

/// Add to the counter and wake whoever is waiting. `false` when it would go
/// past the ceiling, which the caller turns into a wait or into `EAGAIN`.
pub fn add(ev: usize, n: u64) -> bool {
    if n == 0 {
        return false;
    }
    let held = LOCK.lock();
    let ok = unsafe {
        match events().get(ev) {
            Some(e) if MAX_COUNT - e.count >= n => {
                e.count += n;
                waitlist::wake_all(&mut e.waiters);
                true
            }
            _ => false,
        }
    };
    drop(held);
    if ok {
        crate::pollset::note_event();
    }
    ok
}

/// `tid` off the list of the counter it waits on, if it waits on one: under
/// the counters' lock (`waitlist::forget`).
///
/// # Safety
/// Interrupts off, and [`LOCK`] not held.
pub unsafe fn forget(tid: usize) -> bool {
    let _held = LOCK.lock();
    unsafe {
        waitlist::forget_held(tid, |on| match on {
            On::Event(ev) => Some(events().get(ev as usize).map(|e| &mut e.waiters)),
            _ => None,
        })
    }
}

/// Park until the counter is not zero. `false` when there is no need, or a
/// signal has ended the wait before it began.
pub fn wait(ev: usize) -> bool {
    let tid = scheduler::current_tid();
    let held = LOCK.lock();
    let parked = unsafe {
        match events().get(ev) {
            Some(e) if e.count == 0 && !crate::signal::ends_wait(tid) => {
                waitlist::add(&mut e.waiters, tid, On::Event(ev as u32));
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
