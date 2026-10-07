//! An array that grows: its entries on the heap, the room doubled when one
//! past the end is wanted, never past a ceiling its owner gives.
//!
//! What a program's descriptors are kept in (`fdtable.rs`). A table of a fixed
//! size spent its memory whether or not it was used, and was too small for a
//! desktop the day it was not: sixty-four descriptors a program, where a
//! program holding a display connection, a session bus, its fonts and its
//! event loop's wake-ups wants hundreds.
//!
//! Growing asks the heap itself and takes no for an answer. The kernel's
//! allocator stops the machine on a failure handed to it through `alloc`'s
//! own collections; a program asking for one descriptor too many is not a
//! reason to.

use core::alloc::Layout;

/// There was no room: past the ceiling, or no memory for more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoRoom;

pub struct Grow<T: Copy> {
    ptr: *mut T,
    len: usize,
    /// What a new entry starts as.
    fill: T,
}

impl<T: Copy> Grow<T> {
    /// No room yet; every entry made later starts as `fill`.
    pub const fn new(fill: T) -> Self {
        Grow { ptr: core::ptr::null_mut(), len: 0, fill }
    }

    /// How many entries there is room for now.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn get(&self, i: usize) -> Option<&T> {
        if i < self.len { Some(unsafe { &*self.ptr.add(i) }) } else { None }
    }

    pub fn get_mut(&mut self, i: usize) -> Option<&mut T> {
        if i < self.len { Some(unsafe { &mut *self.ptr.add(i) }) } else { None }
    }

    /// Entry `i`, room made for it if there is none: the room doubles, from
    /// `first`, until it holds `i`, and never past `most`.
    pub fn ensure(&mut self, i: usize, first: usize, most: usize) -> Result<&mut T, NoRoom> {
        if i < self.len {
            return Ok(unsafe { &mut *self.ptr.add(i) });
        }
        if i >= most {
            return Err(NoRoom);
        }
        let mut want = self.len.max(first).max(1);
        while want <= i {
            want = want.saturating_mul(2);
        }
        let want = want.min(most);
        let layout = Layout::array::<T>(want).map_err(|_| NoRoom)?;
        let new = unsafe { alloc::alloc::alloc(layout) } as *mut T;
        if new.is_null() {
            return Err(NoRoom);
        }
        unsafe {
            if self.len > 0 {
                core::ptr::copy_nonoverlapping(self.ptr, new, self.len);
                alloc::alloc::dealloc(self.ptr as *mut u8, Layout::array::<T>(self.len).unwrap());
            }
            for j in self.len..want {
                new.add(j).write(self.fill);
            }
        }
        self.ptr = new;
        self.len = want;
        Ok(unsafe { &mut *self.ptr.add(i) })
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        (0..self.len).map(move |i| unsafe { &*self.ptr.add(i) })
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> + '_ {
        let ptr = self.ptr;
        (0..self.len).map(move |i| unsafe { &mut *ptr.add(i) })
    }

    /// Everything, taken out: this is left with no room.
    pub fn take(&mut self) -> Grow<T> {
        core::mem::replace(self, Grow::new(self.fill))
    }
}

impl<T: Copy> Drop for Grow<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { alloc::alloc::dealloc(self.ptr as *mut u8, Layout::array::<T>(self.len).unwrap()) };
        }
    }
}
