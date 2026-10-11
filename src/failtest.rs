//! A kernel built to test what it makes when there is no memory to make it
//! with (`failtest`): the next records of a kind are refused, as many as
//! `SYS_MEM_INFO` 7 and 8 ask, as if the heap had nothing left for them; a
//! task can be made nothing more it asks for (9), as if what is free were
//! what is kept back; and the heap says how much it has in use (10).
//!
//! Running out of memory at the one moment that matters is not something a
//! test can arrange on a real machine; this is where it is arranged.

use core::sync::atomic::{AtomicUsize, Ordering};

/// A program's record: its descriptors, signals and the rest
/// (`fdtable::attach_new`).
pub const PROGRAM: usize = 0;
/// A task's capability space (`cap::task_made`).
pub const HOLDING: usize = 1;

static OWED: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];

/// The task made nothing more (`SYS_MEM_INFO` 9), as its number and one;
/// nought for none.
static MAKES_NOTHING: AtomicUsize = AtomicUsize::new(0);

/// Refuse the next `n` records of `kind`.
pub fn refuse(kind: usize, n: usize) {
    OWED[kind].store(n, Ordering::SeqCst);
}

/// Whether this record of `kind` is refused: one owed, and taken.
pub fn refused(kind: usize) -> bool {
    OWED[kind].fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
}

/// Have `tid` made nothing more it asks for, as if what is free were what
/// is kept back (`reclaim::may_make`) — or, `on` false, let it again.
pub fn make_nothing(tid: usize, on: bool) {
    MAKES_NOTHING.store(if on { tid + 1 } else { 0 }, Ordering::SeqCst);
}

/// Whether `tid` is made nothing more.
pub fn makes_nothing(tid: usize) -> bool {
    MAKES_NOTHING.load(Ordering::SeqCst) == tid + 1
}
