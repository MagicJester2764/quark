//! Record locks, as Linux has them.
//!
//! A lock belongs to a program (POSIX `fcntl` locks) or to one open handle
//! (open-file-description locks, and `flock`). An owner's own locks never
//! conflict with each other: a new one replaces what the owner held over its
//! range, and joins neighbours of the same kind. A program's locks go when it
//! closes any handle on the file or dies; a handle's go when it closes.
//!
//! A request that may wait and cannot be granted is kept, unanswered, and
//! answered when a release makes room. Before a program waits, the chain of
//! programs waiting on each other is followed; if it comes back to the
//! program asking, the answer is `DEADLOCK` instead.

use crate::protocol::{ERR_DEADLOCK, ERR_NO_SPACE};

#[derive(Clone, Copy, PartialEq)]
pub enum Owner {
    Program(u64),
    Handle(usize),
}

/// A locked byte range, `start..end`, of one inode. `end` is `u64::MAX` for
/// "to the end of the file, however long it gets".
#[derive(Clone, Copy)]
pub struct Range {
    pub inode: u32,
    pub owner: Owner,
    pub start: u64,
    pub end: u64,
    pub exclusive: bool,
}

impl Range {
    fn overlaps(&self, other: &Range) -> bool {
        self.inode == other.inode && self.start < other.end && other.start < self.end
    }

    fn conflicts(&self, other: &Range) -> bool {
        self.owner != other.owner && self.overlaps(other) && (self.exclusive || other.exclusive)
    }
}

/// A request waiting for a lock: who is blocked in the call, their program,
/// and what they asked for.
#[derive(Clone, Copy)]
pub struct Waiter {
    pub sender: usize,
    pub space: u64,
    pub want: Range,
}

const MAX_LOCKS: usize = 256;
const MAX_WAITERS: usize = 64;

static mut LOCKS: [Option<Range>; MAX_LOCKS] = [None; MAX_LOCKS];
static mut WAITERS: [Option<Waiter>; MAX_WAITERS] = [None; MAX_WAITERS];

fn locks() -> &'static mut [Option<Range>; MAX_LOCKS] {
    unsafe { &mut *core::ptr::addr_of_mut!(LOCKS) }
}

fn waiters() -> &'static mut [Option<Waiter>; MAX_WAITERS] {
    unsafe { &mut *core::ptr::addr_of_mut!(WAITERS) }
}

/// The first lock that keeps `want` out, if one does.
pub fn conflict(want: &Range) -> Option<Range> {
    locks().iter().flatten().find(|held| held.conflicts(want)).copied()
}

/// Give `want`'s owner `want` over its range — or, with `unlock`, nothing
/// there — replacing whatever the owner held over it. Fails only if the table
/// cannot hold the result, in which case nothing has changed.
pub fn apply(want: &Range, unlock: bool) -> Result<(), u64> {
    let table = locks();
    let mine = |r: &Range| r.owner == want.owner && r.inode == want.inode;

    // Room: a range of the owner's sticking out on both sides of the new one
    // becomes two, and the new range is one more; one it covers entirely
    // gives its slot back. Merging may free more, which is not counted.
    let split = table
        .iter()
        .flatten()
        .filter(|r| mine(r) && r.start < want.start && r.end > want.end)
        .count();
    let covered = table
        .iter()
        .flatten()
        .filter(|r| mine(r) && r.start >= want.start && r.end <= want.end)
        .count();
    let free = table.iter().filter(|r| r.is_none()).count();
    let needed = split + if unlock { 0 } else { 1 };
    if needed > free + covered {
        return Err(ERR_NO_SPACE);
    }

    // Cut the owner's ranges back to what lies outside the new one.
    let mut tails = [None; MAX_LOCKS];
    let mut ntails = 0;
    for slot in table.iter_mut() {
        let Some(r) = *slot else { continue };
        if !mine(&r) || !r.overlaps(want) {
            continue;
        }
        *slot = None;
        if r.start < want.start {
            let mut head = r;
            head.end = want.start;
            *slot = Some(head);
        }
        if r.end > want.end {
            let mut tail = r;
            tail.start = want.end;
            tails[ntails] = Some(tail);
            ntails += 1;
        }
    }
    for tail in tails.iter().take(ntails).flatten() {
        insert(*tail);
    }
    if unlock {
        return Ok(());
    }

    // The new range, joined with any of the same kind it touches.
    let mut merged = *want;
    for slot in table.iter_mut() {
        let Some(r) = *slot else { continue };
        if mine(&r) && r.exclusive == want.exclusive && r.start <= merged.end && merged.start <= r.end {
            merged.start = merged.start.min(r.start);
            merged.end = merged.end.max(r.end);
            *slot = None;
        }
    }
    insert(merged);
    Ok(())
}

fn insert(range: Range) {
    if let Some(slot) = locks().iter_mut().find(|r| r.is_none()) {
        *slot = Some(range);
    }
}

/// Drop everything `owner` holds — on `inode`, or everywhere.
pub fn release(owner: Owner, inode: Option<u32>) {
    for slot in locks().iter_mut() {
        if let Some(r) = slot {
            if r.owner == owner && inode.is_none_or(|i| i == r.inode) {
                *slot = None;
            }
        }
    }
}

/// Whether program `asker` waiting for `want` would wait for ever: whether
/// the programs holding what it wants are, one after another, waiting on it.
fn would_deadlock(asker: u64, want: &Range) -> bool {
    // Programs to look at, and how many have been; a chain longer than the
    // waiters there can be has gone round.
    let mut pending = [*want; MAX_WAITERS + 1];
    let mut len = 1;
    let mut steps = 0;
    while len > 0 && steps <= MAX_WAITERS {
        len -= 1;
        let wanted = pending[len];
        steps += 1;
        for held in locks().iter().flatten().filter(|h| h.conflicts(&wanted)) {
            let Owner::Program(holder) = held.owner else { continue };
            if holder == asker {
                return true;
            }
            for w in waiters().iter().flatten().filter(|w| w.space == holder) {
                if len < pending.len() {
                    pending[len] = w.want;
                    len += 1;
                }
            }
        }
    }
    false
}

/// Keep `w` until its lock can be granted. `DEADLOCK` if it never could be.
pub fn wait(w: Waiter) -> Result<(), u64> {
    if let Owner::Program(_) = w.want.owner {
        if would_deadlock(w.space, &w.want) {
            return Err(ERR_DEADLOCK);
        }
    }
    match waiters().iter_mut().find(|s| s.is_none()) {
        Some(slot) => {
            *slot = Some(w);
            Ok(())
        }
        None => Err(ERR_NO_SPACE),
    }
}

/// A waiter whose lock can now be granted, taken off the list. The caller
/// applies it and answers.
pub fn grantable() -> Option<Waiter> {
    for slot in waiters().iter_mut() {
        if let Some(w) = slot {
            if conflict(&w.want).is_none() {
                return slot.take();
            }
        }
    }
    None
}

/// A program has gone: its locks, and its waiting requests, go too.
pub fn drop_space(space: u64) {
    release(Owner::Program(space), None);
    for slot in waiters().iter_mut() {
        if slot.is_some_and(|w| w.space == space) {
            *slot = None;
        }
    }
}

/// A task has gone: nobody is left to answer if it was waiting.
pub fn drop_task(tid: usize) {
    for slot in waiters().iter_mut() {
        if slot.is_some_and(|w| w.sender == tid) {
            *slot = None;
        }
    }
}

/// A handle has closed: its locks go, and so does anything asked for through
/// it. Returns one such waiter at a time, for the caller to answer, until
/// there are none.
pub fn drop_handle(handle: usize) -> Option<Waiter> {
    release(Owner::Handle(handle), None);
    waiters()
        .iter_mut()
        .find(|slot| slot.is_some_and(|w| w.want.owner == Owner::Handle(handle)))
        .and_then(|slot| slot.take())
}

/// Whether `tid` is waiting for a lock.
pub fn is_waiting(tid: usize) -> bool {
    waiters().iter().flatten().any(|w| w.sender == tid)
}
