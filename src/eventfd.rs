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

/// # Safety
/// Interrupts off.
#[inline(always)]
unsafe fn events() -> &'static mut crate::table::Table<Event> {
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
    if initial > MAX_COUNT || !crate::reclaim::may_make() {
        return None;
    }
    let flags = irq_save();
    let made = unsafe {
        events().lowest_free(0).filter(|&i| {
            events().fill_at(i, Event { creator, refs: 0, count: initial, semaphore, waiters: Waiters::NONE }).is_ok()
        })
    };
    irq_restore(flags);
    made
}

pub fn retain(ev: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(e) = events().get(ev) {
            e.refs += 1;
        }
    }
    irq_restore(flags);
}

pub fn release(ev: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(e) = events().get(ev) {
            e.refs = e.refs.saturating_sub(1);
            if e.refs == 0 {
                gone(ev);
            }
        }
    }
    irq_restore(flags);
}

/// Counter `ev` goes: anybody still on its list — which a waiter's own
/// reference should have made nobody — looks again, and finds nothing.
///
/// # Safety
/// Interrupts off.
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
    let flags = irq_save();
    unsafe {
        let mut at = 0;
        while let Some(ev) = events().next_used(at) {
            at = ev + 1;
            if events().get(ev).is_some_and(|e| e.creator == creator && e.refs == 0) {
                gone(ev);
            }
        }
    }
    irq_restore(flags);
}

pub fn readable(ev: usize) -> bool {
    let flags = irq_save();
    let r = unsafe { events().get(ev).is_some_and(|e| e.count > 0) };
    irq_restore(flags);
    r
}

/// Writable while there is room for one more, which is every counter that is
/// not at its ceiling — so, in practice, always.
pub fn writable(ev: usize) -> bool {
    let flags = irq_save();
    let w = unsafe { events().get(ev).is_some_and(|e| e.count < MAX_COUNT) };
    irq_restore(flags);
    w
}

/// Take what is there. `None` when the counter is zero, which is what the
/// caller turns into a wait or into `EAGAIN`.
pub fn take(ev: usize) -> Option<u64> {
    let flags = irq_save();
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
    irq_restore(flags);
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
    let flags = irq_save();
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
    irq_restore(flags);
    if ok {
        crate::pollset::note_event();
    }
    ok
}

/// Counter `ev`'s list of waiters, for `waitlist::forget`.
///
/// # Safety
/// Interrupts off.
pub unsafe fn waiters(ev: usize) -> Option<&'static mut Waiters> {
    unsafe { events().get(ev).map(|e| &mut e.waiters) }
}

/// Park until the counter is not zero. `false` when there is no need, or a
/// signal has ended the wait before it began.
pub fn wait(ev: usize) -> bool {
    let tid = scheduler::current_tid();
    let flags = irq_save();
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
    irq_restore(flags);
    if parked {
        scheduler::yield_now();
    }
    parked
}
