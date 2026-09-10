//! Waiting on more than one descriptor.
//!
//! A set is itself a descriptor, which is what makes it composable and is why
//! this is epoll-shaped rather than a call taking an array: the set is built
//! once and waited on many times, instead of being marshalled across the
//! boundary on every turn of a loop that runs once per frame. The call that
//! takes an array exists too, beside this one, because `poll(2)` is what most
//! software actually calls.
//!
//! Readiness is evaluated at wake-up rather than stored. A stored bit has to be
//! invalidated by everything that could change it, and the way to get that
//! wrong is a task that sleeps through data already waiting for it. Thirty-two
//! entries scanned is cheaper than being wrong.

use crate::task::FdKind;
use crate::{pipe, scheduler, stream};

const MAX_SETS: usize = 64;
const MAX_WATCHED: usize = 32;

pub const READABLE: u32 = 1;
pub const WRITABLE: u32 = 2;
pub const HANGUP: u32 = 4;

#[derive(Clone, Copy)]
struct Watch {
    fd: usize,
    events: u32,
    token: u64,
    used: bool,
}

struct PollSet {
    in_use: bool,
    owner: usize,
    watches: [Watch; MAX_WATCHED],
}

impl PollSet {
    const fn empty() -> Self {
        PollSet {
            in_use: false,
            owner: 0,
            watches: [Watch { fd: 0, events: 0, token: 0, used: false }; MAX_WATCHED],
        }
    }
}

static mut SETS: [PollSet; MAX_SETS] = {
    const S: PollSet = PollSet::empty();
    [S; MAX_SETS]
};

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

#[inline(always)]
unsafe fn sets() -> &'static mut [PollSet; MAX_SETS] { unsafe {
    &mut *core::ptr::addr_of_mut!(SETS)
}}

pub fn create(tid: usize) -> Option<usize> {
    let flags = irq_save();
    let out = unsafe {
        match sets().iter().position(|s| !s.in_use) {
            Some(i) => {
                sets()[i] = PollSet::empty();
                sets()[i].in_use = true;
                sets()[i].owner = tid;
                Some(i)
            }
            None => None,
        }
    };
    irq_restore(flags);
    out
}

pub fn destroy(set: usize) {
    if set >= MAX_SETS {
        return;
    }
    let flags = irq_save();
    unsafe { sets()[set] = PollSet::empty() };
    irq_restore(flags);
}

/// Can this descriptor ever become ready?
///
/// Only the kinds with a buffer can. An IPC endpoint has nothing that becomes
/// ready, so adding one is refused here rather than reported as never ready —
/// a caller waiting for ever on something that cannot arrive deserves an error
/// and not silence.
pub fn watchable(tid: usize, fd: usize) -> bool {
    if fd >= crate::task::MAX_FDS {
        return false;
    }
    unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => matches!(
                t.fds[fd],
                FdKind::PipeRead(_) | FdKind::PipeWrite(_) | FdKind::StreamEnd { .. }
            ),
            None => false,
        }
    }
}

/// op: 0 add, 1 modify, 2 remove.
pub fn ctl(set: usize, tid: usize, op: u64, fd: usize, events: u32, token: u64) -> bool {
    if set >= MAX_SETS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut sets()[set];
        if !s.in_use || s.owner != tid {
            false
        } else {
            match op {
                2 => {
                    let mut found = false;
                    for w in s.watches.iter_mut() {
                        if w.used && w.fd == fd {
                            w.used = false;
                            found = true;
                        }
                    }
                    found
                }
                1 => {
                    let mut found = false;
                    for w in s.watches.iter_mut() {
                        if w.used && w.fd == fd {
                            w.events = events;
                            w.token = token;
                            found = true;
                        }
                    }
                    found
                }
                _ => {
                    if s.watches.iter().any(|w| w.used && w.fd == fd) {
                        false
                    } else {
                        match s.watches.iter_mut().find(|w| !w.used) {
                            Some(w) => {
                                *w = Watch { fd, events, token, used: true };
                                true
                            }
                            None => false,
                        }
                    }
                }
            }
        }
    };
    irq_restore(flags);
    ok
}

/// What a descriptor can do right now.
fn readiness(tid: usize, fd: usize) -> u32 {
    if fd >= crate::task::MAX_FDS {
        return 0;
    }
    let kind = unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => t.fds[fd],
            None => return 0,
        }
    };
    let mut out = 0;
    match kind {
        FdKind::PipeRead(h) => {
            if pipe::readable(h) {
                out |= READABLE;
            }
            if pipe::no_writers(h) {
                out |= HANGUP;
            }
        }
        FdKind::PipeWrite(h) => {
            if pipe::writable(h) {
                out |= WRITABLE;
            }
            if pipe::no_readers(h) {
                out |= HANGUP;
            }
        }
        FdKind::StreamEnd { stream: s, end } => {
            if stream::readable(s, end) {
                out |= READABLE;
            }
            if stream::writable(s, end) {
                out |= WRITABLE;
            }
            if stream::peer_gone(s, end) {
                out |= HANGUP;
            }
        }
        _ => {}
    }
    out
}

/// What one descriptor can do now, for callers with no set.
pub fn readiness_of(tid: usize, fd: usize) -> u32 {
    readiness(tid, fd)
}

/// The task blocked in a wait on this set, if any.
///
/// One waiter per set. Two tasks waiting on one set would each have to be told
/// which of them takes an event, and nothing here shares a set.
static mut WAITERS: [usize; MAX_SETS] = [usize::MAX; MAX_SETS];

/// Register as the waiter on a set, before the last scan and the block.
///
/// The order matters and is the whole of why this is correct: anything that
/// becomes ready after this either happened before the scan that follows — so
/// the scan sees it and we never block — or after it, and then `note_pipe`
/// finds us parked and wakes us. There is no window between looking and
/// sleeping.
pub fn park(set: usize, tid: usize) {
    if set < MAX_SETS {
        let flags = irq_save();
        unsafe { (*core::ptr::addr_of_mut!(WAITERS))[set] = tid };
        irq_restore(flags);
    }
}

pub fn unpark(set: usize) {
    if set < MAX_SETS {
        let flags = irq_save();
        unsafe { (*core::ptr::addr_of_mut!(WAITERS))[set] = usize::MAX };
        irq_restore(flags);
    }
}

/// Does this task's descriptor `fd` name pipe `handle`?
fn names_pipe(tid: usize, fd: usize, handle: usize) -> bool {
    if fd >= crate::task::MAX_FDS {
        return false;
    }
    let kind = unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => t.fds[fd],
            None => return false,
        }
    };
    match kind {
        FdKind::PipeRead(h) | FdKind::PipeWrite(h) => h == handle,
        FdKind::StreamEnd { stream: s, end } => match stream::pipes_for(s, end) {
            Some((rd, wr)) => rd == handle || wr == handle,
            None => false,
        },
        _ => false,
    }
}

/// A pipe changed state. Wake any set watching a descriptor that names it.
///
/// Sets are scanned rather than pipes carrying a list of their watchers: there
/// are sixty-four sets, and the alternative puts a back pointer in every pipe
/// for the benefit of the rare one anybody watches. The parked check comes
/// first, so a system with nobody waiting pays sixty-four comparisons.
pub fn note_pipe(handle: usize) {
    let mut wake = [usize::MAX; MAX_SETS];
    let mut n = 0;

    let flags = irq_save();
    unsafe {
        let waiters = &*core::ptr::addr_of!(WAITERS);
        for i in 0..MAX_SETS {
            let waiter = waiters[i];
            if waiter == usize::MAX || !sets()[i].in_use {
                continue;
            }
            wake[n] = waiter;
            n += 1;
        }
    }
    irq_restore(flags);

    // Deciding *which* of them care is done outside the lock, because
    // `names_pipe` reaches into the task table and the stream table.
    for i in 0..n {
        let tid = wake[i];
        let watches = {
            let flags = irq_save();
            let w = unsafe {
                match sets().iter().position(|s| s.in_use && s.owner == tid) {
                    Some(idx) => Some(sets()[idx].watches),
                    None => None,
                }
            };
            irq_restore(flags);
            w
        };
        let Some(watches) = watches else { continue };
        if watches
            .iter()
            .any(|w| w.used && names_pipe(tid, w.fd, handle))
        {
            crate::ipc::wake_sleeper(tid);
        }
    }
}

/// Collect what is ready. Returns how many entries of `out` were filled.
pub fn scan(set: usize, tid: usize, out: &mut [(u64, u32)]) -> usize {
    if set >= MAX_SETS {
        return 0;
    }
    let flags = irq_save();
    let watches = unsafe {
        let s = &sets()[set];
        if !s.in_use || s.owner != tid {
            irq_restore(flags);
            return 0;
        }
        s.watches
    };
    irq_restore(flags);

    let mut n = 0;
    for w in watches.iter() {
        if !w.used || n == out.len() {
            continue;
        }
        // Hangup is reported whether it was asked for or not: a caller waiting
        // for readable on a descriptor whose peer has gone would otherwise be
        // waiting for something that can never arrive.
        let r = readiness(tid, w.fd);
        let hit = (r & w.events) | (r & HANGUP);
        if hit != 0 {
            out[n] = (w.token, hit);
            n += 1;
        }
    }
    n
}
