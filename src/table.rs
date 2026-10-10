//! A table that grows: up to [`MOST`] slots, each slot's record made when the
//! slot is filled and given back when it is emptied, reached by its number.
//!
//! What the tasks are kept in (`scheduler.rs`), and later what a program makes.
//! A fixed array of sixty-four spent every slot's memory whether or not
//! anything was in it, and sixty-four was a desktop's limit; this spends a
//! pointer on a slot until it is used.
//!
//! A record comes from the heap, by a fallible allocation: a slot that cannot
//! be filled says so, and never stops the machine. Records of one type are
//! one size, so a slot given back is the next one's room.
//!
//! Which slots hold a record is a bit each, so that a walk of what is there
//! ([`Table::next_used`]) goes past sixty-four empty slots at a step: the
//! highest slot ever filled stays where it was, and a machine that once ran
//! four thousand tasks walks no more slowly for it.
//!
//! Everything here is under the one lock, as what it holds always was.
//! [`Table::slot`] answers for a slot that was never made with a `None` of the
//! table's own, which is what a fixed array answered for an empty slot: a
//! record written into that is a mistake, and the next look says so.

use core::alloc::Layout;

/// The most slots a table can have.
pub const MOST: usize = 32_768;

const PER_BLOCK: usize = 256;
const BLOCKS: usize = MOST / PER_BLOCK;

type Block<T> = [*mut Option<T>; PER_BLOCK];

/// What a record is made from by [`Table::fill_from`]: a whole one, copied
/// into a slot's room. It owns nothing — whatever it would point to is null —
/// so a copy of it is a record of its own.
pub struct Template<T>(pub Option<T>);

unsafe impl<T> Sync for Template<T> {}

pub struct Table<T> {
    /// Blocks of slots, a block made the first time one of its slots is.
    blocks: [*mut Block<T>; BLOCKS],
    /// A bit for each slot that holds a record.
    filled: [u64; MOST / 64],
    /// One past the highest slot that may be filled.
    limit: usize,
    /// One past the highest slot ever filled: a walk goes no further.
    high: usize,
    /// What a slot never made reads as.
    missing: Option<T>,
}

impl<T> Table<T> {
    /// A table with no slots made, whose slots below `limit` may be filled.
    pub const fn new(limit: usize) -> Self {
        Table {
            blocks: [core::ptr::null_mut(); BLOCKS],
            filled: [0; MOST / 64],
            limit: if limit < MOST { limit } else { MOST },
            high: 0,
            missing: None,
        }
    }

    /// Slot `i`'s cell, or null if it was never made.
    fn cell(&self, i: usize) -> *mut Option<T> {
        if i >= self.high {
            return core::ptr::null_mut();
        }
        let block = self.blocks[i / PER_BLOCK];
        if block.is_null() { core::ptr::null_mut() } else { unsafe { (*block)[i % PER_BLOCK] } }
    }

    /// The record in slot `i`, if there is one.
    pub fn get(&mut self, i: usize) -> Option<&'static mut T> {
        let cell = self.cell(i);
        if cell.is_null() { None } else { unsafe { (*cell).as_mut() } }
    }

    /// The record in slot `i`, to read, by whoever may while other slots are
    /// filled and emptied: a slot's room, once made, stays where it is until
    /// the slot is emptied, and the words that find it only grow.
    pub fn peek(&self, i: usize) -> Option<&'static T> {
        let cell = self.cell(i);
        if cell.is_null() { None } else { unsafe { (*cell).as_ref() } }
    }

    /// Slot `i` as an `Option`: the record's own, or for a slot never made the
    /// table's `None`. Filling and emptying a slot are [`fill_at`] and
    /// [`empty`]; nothing else writes a whole slot.
    ///
    /// [`fill_at`]: Table::fill_at
    /// [`empty`]: Table::empty
    pub fn slot(&mut self, i: usize) -> &'static mut Option<T> {
        let cell = self.cell(i);
        if !cell.is_null() {
            return unsafe { &mut *cell };
        }
        if self.missing.is_some() {
            panic!("table: a record was written into a slot that was never made");
        }
        unsafe { &mut *(&raw mut self.missing) }
    }

    /// Whether slot `i` holds a record.
    pub fn used(&self, i: usize) -> bool {
        i < self.high && self.filled[i / 64] & (1 << (i % 64)) != 0
    }

    /// The first slot at or past `from` that holds a record.
    pub fn next_used(&self, from: usize) -> Option<usize> {
        let mut i = from;
        while i < self.high {
            let word = self.filled[i / 64] >> (i % 64);
            if word != 0 {
                let at = i + word.trailing_zeros() as usize;
                return (at < self.high).then_some(at);
            }
            i = (i / 64 + 1) * 64;
        }
        None
    }

    /// One past the highest slot ever filled.
    pub fn high(&self) -> usize {
        self.high
    }

    /// The lowest slot at or above `from` that holds no record.
    pub fn lowest_free(&self, from: usize) -> Option<usize> {
        let mut i = from;
        while i < self.limit {
            let word = !self.filled[i / 64] >> (i % 64);
            if word != 0 {
                let at = i + word.trailing_zeros() as usize;
                return (at < self.limit).then_some(at);
            }
            i = (i / 64 + 1) * 64;
        }
        None
    }

    /// Slot `i`'s room, made if it is not there: `None` past the limit, or
    /// with no memory for it.
    fn room(&mut self, i: usize) -> Option<*mut Option<T>> {
        if i >= self.limit {
            return None;
        }
        let b = i / PER_BLOCK;
        if self.blocks[b].is_null() {
            let block = unsafe { alloc::alloc::alloc(Layout::new::<Block<T>>()) } as *mut Block<T>;
            if block.is_null() {
                return None;
            }
            unsafe { block.write([core::ptr::null_mut(); PER_BLOCK]) };
            self.blocks[b] = block;
        }
        let at = unsafe { &mut (*self.blocks[b])[i % PER_BLOCK] };
        if at.is_null() {
            let cell = unsafe { alloc::alloc::alloc(Layout::new::<Option<T>>()) } as *mut Option<T>;
            if cell.is_null() {
                return None;
            }
            unsafe { cell.write(None) };
            *at = cell;
        }
        Some(*at)
    }

    fn mark(&mut self, i: usize) {
        self.filled[i / 64] |= 1 << (i % 64);
        if i >= self.high {
            self.high = i + 1;
        }
    }

    /// Put `record` in slot `i`. The record comes back if the slot is taken,
    /// past the limit, or there is no memory for it.
    pub fn fill_at(&mut self, i: usize, record: T) -> Result<(), T> {
        let Some(cell) = self.room(i) else { return Err(record) };
        let cell = unsafe { &mut *cell };
        if cell.is_some() {
            return Err(record);
        }
        *cell = Some(record);
        self.mark(i);
        Ok(())
    }

    /// Put a copy of `template` in slot `i`, and have `init` make it this
    /// one: a record made in its room. Built on the stack and moved, a record
    /// costs a kernel stack its size, and more than once — a task's is six
    /// kilobytes. Fails where [`fill_at`](Table::fill_at) would.
    pub fn fill_from(&mut self, i: usize, template: &'static Template<T>, init: impl FnOnce(&mut T)) -> Result<(), ()> {
        let cell = self.room(i).ok_or(())?;
        unsafe {
            if (*cell).is_some() {
                return Err(());
            }
            core::ptr::copy_nonoverlapping(&template.0 as *const Option<T>, cell, 1);
            if let Some(record) = (*cell).as_mut() {
                init(record);
            }
        }
        self.mark(i);
        Ok(())
    }

    /// Give slot `i`'s record back, and the memory it was in.
    pub fn empty(&mut self, i: usize) {
        if i >= self.high {
            return;
        }
        let block = self.blocks[i / PER_BLOCK];
        if block.is_null() {
            return;
        }
        let at = unsafe { &mut (*block)[i % PER_BLOCK] };
        let cell = core::mem::replace(at, core::ptr::null_mut());
        if cell.is_null() {
            return;
        }
        self.filled[i / 64] &= !(1 << (i % 64));
        unsafe {
            core::ptr::drop_in_place(cell);
            alloc::alloc::dealloc(cell as *mut u8, Layout::new::<Option<T>>());
        }
    }
}
