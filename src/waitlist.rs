//! Who is waiting on what.
//!
//! A task waits on one thing at a time, so where it waits is a link in its
//! own record ([`WaitLink`], `TaskRec::wait`): the list it is on, and the
//! tasks either side of it there. Each thing that can be waited on keeps the
//! two ends of each of its lists ([`Waiters`]) — a counter its readers, a
//! timer its readers, a pipe its readers, its writers and those waiting for
//! its other end — and adding, taking off and waking walk the links. No list
//! has a length: they were arrays of four and of eight, which refused the
//! fifth or the ninth, and a refused waiter's read came back at once.
//!
//! Everything here is done with interrupts off.

use crate::scheduler;

/// The end of a list.
pub const END: u16 = u16::MAX;

/// Which list a task is on: the thing, by its number, and which of its
/// lists.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum On {
    Nothing,
    /// A counter's readers (`eventfd.rs`).
    Event(u32),
    /// A timer's readers (`timerfd.rs`).
    Timer(u32),
    /// A pipe's readers, its writers, and those waiting for its other end
    /// to be opened (`pipe.rs`).
    PipeRead(u32),
    PipeWrite(u32),
    PipePeer(u32),
}

/// Where a task waits.
#[derive(Clone, Copy)]
pub struct WaitLink {
    on: On,
    next: u16,
    prev: u16,
}

impl WaitLink {
    pub const NONE: WaitLink = WaitLink { on: On::Nothing, next: END, prev: END };
}

/// A list's first and last.
#[derive(Clone, Copy)]
pub struct Waiters {
    first: u16,
    last: u16,
}

impl Waiters {
    pub const NONE: Waiters = Waiters { first: END, last: END };

    pub fn is_empty(&self) -> bool {
        self.first == END
    }
}

/// Task `t`'s link, unless it is the end of a list or no task.
///
/// # Safety
/// Interrupts off.
unsafe fn link(t: u16) -> Option<&'static mut WaitLink> {
    if t == END {
        return None;
    }
    unsafe { scheduler::rec(t as usize).map(|r| &mut r.wait) }
}

/// `tid` waits on `on`, last on `list`, which is `on`'s. A task on another
/// list is taken off that first: it waits on one thing at a time.
///
/// # Safety
/// Interrupts off, and `list` is the list `on` names.
pub unsafe fn add(list: &mut Waiters, tid: usize, on: On) {
    unsafe {
        if link(tid as u16).is_some_and(|l| l.on != On::Nothing) {
            forget(tid);
        }
        let Some(l) = link(tid as u16) else { return };
        *l = WaitLink { on, next: END, prev: list.last };
        match link(list.last) {
            Some(last) => last.next = tid as u16,
            None => list.first = tid as u16,
        }
        list.last = tid as u16;
    }
}

/// `tid` is off `list`, if it is on it — which its link says, by `on`.
/// True if it was.
///
/// # Safety
/// Interrupts off, and `list` is the list `on` names.
pub unsafe fn remove(list: &mut Waiters, tid: usize, on: On) -> bool {
    unsafe {
        let Some(l) = link(tid as u16) else { return false };
        if l.on != on || on == On::Nothing {
            return false;
        }
        let (next, prev) = (l.next, l.prev);
        match link(prev) {
            Some(p) => p.next = next,
            None => list.first = next,
        }
        match link(next) {
            Some(n) => n.prev = prev,
            None => list.last = prev,
        }
        *l = WaitLink::NONE;
        true
    }
}

/// The first on `list`, taken off it and made ready to look again.
///
/// # Safety
/// Interrupts off.
pub unsafe fn wake_one(list: &mut Waiters) -> Option<usize> {
    unsafe {
        let t = list.first;
        let on = link(t)?.on;
        remove(list, t as usize, on);
        scheduler::unblock_task(t as usize);
        Some(t as usize)
    }
}

/// Everyone on `list`, taken off it and made ready to look again.
///
/// # Safety
/// Interrupts off.
pub unsafe fn wake_all(list: &mut Waiters) {
    unsafe {
        while wake_one(list).is_some() {}
        // A list whose first is no task is one nothing is on.
        *list = Waiters::NONE;
    }
}

/// `tid` waits on nothing now: it is off the list its link names, if it is
/// on one. True if it was.
///
/// # Safety
/// Interrupts off.
pub unsafe fn forget(tid: usize) -> bool {
    unsafe {
        let Some(l) = link(tid as u16) else { return false };
        let on = l.on;
        let list = match on {
            On::Nothing => return false,
            On::Event(i) => crate::eventfd::waiters(i as usize),
            On::Timer(i) => crate::timerfd::waiters(i as usize),
            On::PipeRead(i) => crate::pipe::waiters(i as usize, 0),
            On::PipeWrite(i) => crate::pipe::waiters(i as usize, 1),
            On::PipePeer(i) => crate::pipe::waiters(i as usize, 2),
        };
        match list {
            Some(list) => remove(list, tid, on),
            // What it was on has gone; there is no list to be off.
            None => {
                *l = WaitLink::NONE;
                true
            }
        }
    }
}

/// What `tid` is waiting on.
///
/// # Safety
/// Interrupts off.
pub unsafe fn on(tid: usize) -> On {
    unsafe { link(tid as u16).map_or(On::Nothing, |l| l.on) }
}
