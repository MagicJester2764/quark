//! A kernel built to test what it makes when there is no memory to make it
//! with (`failtest`): the next records of a kind are refused, as many as
//! `SYS_MEM_INFO` 7 and 8 ask, as if the heap had nothing left for them.
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

/// Refuse the next `n` records of `kind`.
pub fn refuse(kind: usize, n: usize) {
    OWED[kind].store(n, Ordering::SeqCst);
}

/// Whether this record of `kind` is refused: one owed, and taken.
pub fn refused(kind: usize) -> bool {
    OWED[kind].fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_ok()
}
