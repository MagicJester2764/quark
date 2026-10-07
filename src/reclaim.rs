//! Giving memory back when there is none to give.
//!
//! A frame is asked for and there is none free. For a long time that was
//! the end of whichever program asked. But most of what a machine's memory
//! holds at any moment is not being used at that moment, and some of it is
//! held twice: once in memory and once on a disk. This is what finds that,
//! and what makes a program wait for it rather than end.
//!
//! Three things can be given up, cheapest first:
//!
//! - **A page of a file that nothing maps** and that holds nothing its
//!   pager has still to write (`memobj::evict`). It is on the disk, and the
//!   first touch reads it again.
//! - **A page of a file that a program maps to read and has not used** for
//!   a while: its entry goes back to being the reservation it was before it
//!   was touched, and then it is the first kind.
//! - **A page of a program's own that it has not used for a while**: it is
//!   moved to the object memory is written out to (`memobj::swap_out`), if
//!   the machine has one — a pager that has said it will keep such pages,
//!   in a file or on a partition. Its pager is asked to write it; once it
//!   has, it is the first kind too.
//!
//! "Not used for a while" is the processor's own mark on a page, looked at
//! and cleared on the way round ([`paging::take_unused`]): a page is taken
//! if it has not been touched between one look and the next.
//!
//! **What is never taken.** Memory of a driver or a server: those are what
//! memory is written out *with*, and one waiting for its own page to come
//! back from the disk it drives waits for ever. What the system call a
//! task is in has checked (`scheduler::pinned`): the kernel may be about
//! to copy into it with a lock held. Shared memory, devices, pages a fork
//! left in two programs, and anything the kernel itself holds — page
//! tables, the heap, pipes.
//!
//! **Who does the work** is whoever wanted the frame: a program that
//! faults on a page it was promised, with nothing free, looks for memory
//! itself ([`wait`]), asks the pagers to write, and sleeps a few
//! milliseconds at a time while they do. It is ended, as it always was,
//! only when there is nothing left to give up and nothing being written.
//!
//! **The last frames are kept back** ([`reserve`]) for the kernel's own
//! needs and for drivers and servers: a pager that cannot have a frame to
//! do its writing with cannot free any.

use crate::memobj;
use crate::paging;
use crate::pmm;
use crate::scheduler;

/// How many pages one look for memory tries to give up beyond what is
/// short, and how many entries it looks at to find them: a few hundred
/// microseconds with interrupts off, at the most. Not many beyond: a page
/// taken that was about to be used is written out and read straight back,
/// and both are slow.
const SPARE: usize = 32;
const LOOK: usize = 16384;

/// How long a task sleeps between looks, and how long it goes on looking
/// while there is still something being written.
const NAP_NS: u64 = 5_000_000;
const PATIENCE_NS: u64 = 20_000_000_000;

/// Where the walk of the address spaces has got to: which one, and how far
/// through it.
static mut HAND: (usize, usize) = (0, 0);
/// How many times the walk has been all the way round.
static mut LAPS: u64 = 0;

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

/// How many frames are kept back from ordinary programs: a hundred and
/// twenty-eighth of memory, between half a megabyte and eight.
pub fn reserve() -> usize {
    (pmm::total_count() / 128).clamp(128, 2048)
}

/// A frame for a page of a program's: `None` if there is none to be had —
/// none at all, or for a program that is not a driver or a server none
/// beyond what is kept back. Whoever can wait asks [`wait`] and tries again.
pub fn frame() -> Option<usize> {
    if !may_make() {
        return None;
    }
    pmm::alloc().map(|f| f.address())
}

/// Whether the current task may be given what the kernel makes for it — a
/// frame for its page, or a pipe, a counter, a timer: not a program that is
/// not a driver or a server, while what is free is no more than what is kept
/// back. A program making them without end is refused while the kernel can
/// still work.
pub fn may_make() -> bool {
    scheduler::current_is_privileged() || pmm::free_count() > reserve()
}

/// Whether the current task could be given a frame now.
fn room() -> bool {
    let free = pmm::free_count();
    if scheduler::current_is_privileged() { free > 0 } else { free > reserve() }
}

/// What one look for memory came to.
struct Found {
    /// Frames that went back to the allocator.
    freed: usize,
    /// Pages taken from programs, which are frames once they are written.
    taken: usize,
    /// Pages waiting on a pager to write them before they can be given up.
    writing: usize,
}

/// Look for memory to give up: `want` frames, of which at most `take` by
/// taking pages from programs — fewer than `want` when pages taken already
/// are still waiting to be written, and will be frames when they have been.
fn make_room(want: usize, take: usize) -> Found {
    let mut freed = memobj::evict(want);
    let mut taken = 0;
    let short = want.saturating_sub(freed);
    let want = short.min(take);
    if want > 0 {
        let flags = irq_save();
        let (mut index, mut va) = unsafe { *core::ptr::addr_of!(HAND) };
        let mut budget = LOOK;
        // Once round every address space at most, a part of each at a time.
        let mut spaces = 0;
        // On to the next address space, and round again after the last.
        let on = |index: &mut usize, va: &mut usize, spaces: &mut usize| {
            *index += 1;
            if *index >= crate::userspace::spaces_end() {
                *index = 0;
                unsafe { *core::ptr::addr_of_mut!(LAPS) += 1 };
            }
            *va = 0;
            *spaces += 1;
        };
        while budget > 0 && taken < want && spaces <= crate::userspace::spaces_end() {
            let Some((cr3, space)) = crate::userspace::space_at(index) else {
                on(&mut index, &mut va, &mut spaces);
                continue;
            };
            if !scheduler::space_gives_memory(space) {
                on(&mut index, &mut va, &mut spaces);
                continue;
            }
            let word = crate::fdtable::sig_word_of_space(space) & !0xFFF;
            let (next, got) = unsafe { paging::take_unused(cr3, space, word, va, budget, want - taken) };
            // Entries were taken away, or marks cleared that a processor
            // sets again only once it has forgotten the page.
            crate::tlb::stale(cr3);
            if cr3 == paging::read_cr3() {
                unsafe { paging::write_cr3(cr3) };
            }
            taken += got;
            match next {
                Some(at) => {
                    // The budget ran out part of the way through it.
                    va = at;
                    budget = 0;
                }
                None => {
                    on(&mut index, &mut va, &mut spaces);
                    budget = budget.saturating_sub(LOOK / 64);
                }
            }
        }
        unsafe { *core::ptr::addr_of_mut!(HAND) = (index, va) };
        irq_restore(flags);
        // A page of a file that was only mapped is a frame at once.
        freed += memobj::evict(short);
    }
    Found { freed, taken, writing: memobj::ask_to_clean() }
}

/// How long [`page_out`] waits for what it took to be written, and how
/// many pages it looks at before it does.
const OUT_NS: u64 = 5_000_000_000;
const PART: usize = 256;

/// `SYS_PAGE_OUT`: the caller gives up `pages` pages of its own from
/// `addr` — its own memory is written out now, pages of files it maps to
/// read go back to being reservations — and the frames are given back once
/// their pagers have written what they had to. Answers with how many pages
/// were taken: not ones a fork left in two programs, or ones a system call
/// some other task of the program is in has checked, or the page the
/// program is told of signals through; and none of its own on a machine
/// with nowhere to write memory out to.
pub fn page_out(addr: usize, pages: usize) -> u64 {
    let cr3 = paging::read_cr3();
    let space = crate::userspace::space_of(cr3);
    if space == 0 {
        return 0;
    }
    let end = addr + pages * 4096;
    let mut taken = 0;
    let mut at = Some(addr);
    // A part at a time: interrupts are off while pages are taken, and what
    // is taken passes through a cache that holds only so many — each part
    // is written, and its frames given up, before the next is taken.
    while let Some(from) = at {
        let flags = irq_save();
        let word = crate::fdtable::sig_word_of_space(space) & !0xFFF;
        let (next, got) = unsafe { paging::take_range(cr3, space, word, from, end, PART) };
        if got != 0 {
            crate::tlb::stale(cr3);
            unsafe { paging::write_cr3(cr3) };
        }
        irq_restore(flags);
        at = next;
        taken += got;
        if got == 0 {
            continue;
        }
        // Written, and then given up: not before, or what the caller has
        // back is a promise that a full disk can break.
        let began = crate::clock::now();
        loop {
            let waiting = memobj::ask_to_clean();
            memobj::evict(usize::MAX);
            if waiting == 0 {
                break;
            }
            if crate::clock::now().saturating_sub(began) > OUT_NS {
                // Its pager is not writing. What is left stays where it is.
                return taken as u64;
            }
            crate::ipc::pause(NAP_NS / 5);
        }
    }
    taken as u64
}

/// The current task wants a frame and there is none for it: look for
/// memory, and wait while it is being written out. True if it is worth
/// asking again; false if there is nothing to give up and nothing on its
/// way out, which is a machine that is out of memory.
///
/// It may sleep, so it is for whoever may: a fault taken in ring 3, a
/// system call checking a pointer, a fault in the kernel where interrupts
/// were on.
pub fn wait() -> bool {
    let began = crate::clock::now();
    let mut since = unsafe { *core::ptr::addr_of!(LAPS) };
    // Pages taken and not yet written: frames that are on their way.
    let mut coming = memobj::ask_to_clean();
    loop {
        // What is short of what the current task could be given a frame
        // from, and a little more — less what is on its way already. Asked
        // for afresh each time round, a task waiting on a slow disk took
        // thirty pages more every few milliseconds for as long as it
        // waited, and every one of them was written out and read back.
        let line = if scheduler::current_is_privileged() { 0 } else { reserve() };
        let short = (line + 1).saturating_sub(pmm::free_count()) + SPARE;
        let found = make_room(short, short.saturating_sub(coming));
        coming = found.writing;
        if room() {
            return true;
        }
        let laps = unsafe { *core::ptr::addr_of!(LAPS) };
        if found.freed != 0 || found.taken != 0 || found.writing != 0 {
            // Something is on its way out: give its pager the time to
            // write it.
            since = laps;
            if crate::clock::now().saturating_sub(began) > PATIENCE_NS {
                return false;
            }
            crate::ipc::pause(NAP_NS);
        } else if laps.saturating_sub(since) > 2 {
            // Three times round everything with nothing found and nothing
            // coming. The first time round only clears the marks; what was
            // not touched again is what the second would have taken.
            return false;
        }
        // Otherwise on, without waiting: nothing was found in this part of
        // the walk, and there is more to walk.
    }
}
