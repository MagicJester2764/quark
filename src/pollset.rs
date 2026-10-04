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
use crate::{pipe, stream};

const MAX_SETS: usize = 64;
const MAX_WATCHED: usize = 32;

pub const READABLE: u32 = 1;
pub const WRITABLE: u32 = 2;
pub const HANGUP: u32 = 4;
/// A descriptor that cannot be waited on at all.
///
/// `poll(2)` reports this in `revents` rather than failing the call, because
/// one bad entry should not deny the caller the answer about the others. The
/// set-shaped interface refuses at `ctl` time instead: there the caller is
/// building something to reuse, and a watch that can never fire is a mistake
/// worth hearing about once rather than on every wait.
pub const INVALID: u32 = 8;

#[derive(Clone, Copy)]
struct Watch {
    fd: usize,
    events: u32,
    token: u64,
    used: bool,
}

struct PollSet {
    in_use: bool,
    /// The descriptor table its watches are numbers in — a program's, so any
    /// thread of the program that made the set may use it.
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
    let table = crate::fdtable::table_of(tid);
    if table == usize::MAX {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match sets().iter().position(|s| !s.in_use) {
            Some(i) => {
                sets()[i] = PollSet::empty();
                sets()[i].in_use = true;
                sets()[i].owner = table;
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
    matches!(
        crate::fdtable::get(tid, fd),
        FdKind::PipeRead(_)
            | FdKind::PipeWrite(_)
            | FdKind::StreamEnd { .. }
            | FdKind::PtyEnd { .. }
            | FdKind::Timer { .. }
            | FdKind::Event { .. }
            | FdKind::Served { .. }
            | FdKind::Signals { .. }
    )
}

/// op: 0 add, 1 modify, 2 remove.
pub fn ctl(set: usize, tid: usize, op: u64, fd: usize, events: u32, token: u64) -> bool {
    if set >= MAX_SETS {
        return false;
    }
    let table = crate::fdtable::table_of(tid);
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut sets()[set];
        if !s.in_use || s.owner != table {
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
    let kind = crate::fdtable::get(tid, fd);
    let mut out = 0;
    match kind {
        FdKind::PipeRead(h) => {
            if pipe::readable(h) {
                out |= READABLE;
            }
            if pipe::ended(h) {
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
        FdKind::Timer { timer } => {
            if crate::timerfd::pending(timer) > 0 {
                out |= READABLE;
            }
        }
        FdKind::Event { ev } => {
            if crate::eventfd::readable(ev) {
                out |= READABLE;
            }
            if crate::eventfd::writable(ev) {
                out |= WRITABLE;
            }
        }
        FdKind::PtyEnd { pty, end } => {
            // A read that would return at once is readable, whether it has
            // bytes or an end of file somebody typed. The other end having
            // gone is that and a hangup — which a terminal emulator reads as
            // its shell having exited.
            use crate::pty::Pending;
            match crate::pty::readable(pty, end) {
                Pending::Bytes(0) => {}
                Pending::Bytes(_) | Pending::End => out |= READABLE,
                Pending::Gone => out |= READABLE | HANGUP,
            }
            if crate::pty::writable(pty, end) {
                out |= WRITABLE;
            }
        }
        // A file never keeps anybody waiting: a read answers with what is
        // there, the end included, and a write is taken.
        FdKind::Served { .. } => out |= READABLE | WRITABLE,
        // Whose signals are the reader's: here the one asking.
        FdKind::Signals { sfd } => {
            if crate::signal::waiting_for(tid) & crate::sigfd::mask(sfd) != 0 {
                out |= READABLE;
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
    let kind = crate::fdtable::get(tid, fd);
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
    note(|tid, fd| names_pipe(tid, fd, handle));
}

/// Wake every task parked on a set one of whose watches `names` what changed.
///
/// The watches are copied out with the lock held and matched without it,
/// because matching reaches into the descriptor tables and the stream table.
/// The set a task is parked on is the one whose watches are looked at — it
/// used to be the first set that task owned, which is a different set for a
/// program holding two.
fn note(names: impl Fn(usize, usize) -> bool) {
    // Which sets have somebody parked on them, and who. Only the pairs: a
    // set's watches are most of a kilobyte, and this runs on the kernel stack
    // of whatever wrote to the pipe.
    let mut parked = [(0usize, usize::MAX); MAX_SETS];
    let mut n = 0;

    let flags = irq_save();
    unsafe {
        let waiters = &*core::ptr::addr_of!(WAITERS);
        for i in 0..MAX_SETS {
            let waiter = waiters[i];
            if waiter == usize::MAX || !sets()[i].in_use {
                continue;
            }
            parked[n] = (i, waiter);
            n += 1;
        }
    }
    irq_restore(flags);

    for &(set, tid) in parked[..n].iter() {
        let flags = irq_save();
        let watches = unsafe {
            let s = &sets()[set];
            if s.in_use { Some(s.watches) } else { None }
        };
        irq_restore(flags);
        let Some(watches) = watches else { continue };
        if watches.iter().any(|w| w.used && names(tid, w.fd)) {
            crate::ipc::wake_sleeper(tid);
        }
    }
}

/// Something changed at one end of a pty: wake whoever is waiting on a set
/// that watches it.
///
/// The twin of `note_pipe`, and for the same reason — a set is waited on by a
/// task that is asleep, and nothing else would tell it that a shell has
/// printed something or gone.
pub fn note_pty(pty: usize) {
    note(|tid, fd| names_pty(tid, fd, pty));
}

/// A timer fired: wake whoever is waiting on a set, and let the scan decide
/// whether it was one of theirs. Unlike a pipe or a pty there is no handle to
/// match on here, because the tick fires every armed timer there is and the
/// scan is cheaper than working out whose.
pub fn note_timer() {
    let mut wake = [usize::MAX; MAX_SETS];
    let mut n = 0;
    let flags = irq_save();
    unsafe {
        let waiters = &*core::ptr::addr_of!(WAITERS);
        for i in 0..MAX_SETS {
            if waiters[i] != usize::MAX && sets()[i].in_use {
                wake[n] = waiters[i];
                n += 1;
            }
        }
    }
    irq_restore(flags);
    for i in 0..n {
        crate::ipc::wake_sleeper(wake[i]);
    }
}

/// A signal has come to wait for a program, where a signal descriptor's
/// reader would take it: wake whoever is waiting on a set that watches one,
/// and let the scan say whose it was.
pub fn note_signals() {
    note(|tid, fd| fd < crate::task::MAX_FDS && matches!(crate::fdtable::get(tid, fd), FdKind::Signals { .. }));
}

/// A counter was added to or taken from: same reasoning as `note_timer`, and
/// the same scan. There are sixteen of these in the machine; finding out which
/// sets name this one costs more than waking them to look.
pub fn note_event() {
    note_timer();
}

fn names_pty(tid: usize, fd: usize, pty: usize) -> bool {
    if fd >= crate::task::MAX_FDS {
        return false;
    }
    let kind = crate::fdtable::get(tid, fd);
    matches!(kind, FdKind::PtyEnd { pty: p, .. } if p == pty)
}

/// Collect what is ready. Returns how many entries of `out` were filled.
pub fn scan(set: usize, tid: usize, out: &mut [(u64, u32)]) -> usize {
    if set >= MAX_SETS {
        return 0;
    }
    let table = crate::fdtable::table_of(tid);
    let flags = irq_save();
    let watches = unsafe {
        let s = &sets()[set];
        if !s.in_use || s.owner != table {
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
