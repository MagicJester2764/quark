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
