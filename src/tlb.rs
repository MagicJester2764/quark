//! A mapping taken away is taken away on every processor.
//!
//! A processor remembers the translations it has used, and goes on using
//! what it remembers after the page table says otherwise. On one processor
//! the cure is `invlpg` where the table changes, and the paging code has
//! always done that. With more than one, a thread of the same program may
//! be running on another processor, with the same address space loaded and
//! the old translation in its cache: it would go on reading and writing a
//! frame that has been given back, and then given to somebody else.
//!
//! So a change that takes something away — a page unmapped, or mapped to
//! somewhere else — is told to every other processor that has that address
//! space loaded, and they forget everything (reloading CR3 does it). Two
//! rules make it safe without doing it once a page:
//!
//! - **Before a frame is given out again.** The allocator settles this
//!   first ([`sync`] in `pmm::alloc`), so a frame freed by an unmapping
//!   cannot be anybody else's while a stale translation to it exists.
//!   That holds for the frames of page tables too, which a processor also
//!   remembers its way through.
//! - **Before the kernel lock is given up** (`klock::release`), which is
//!   before the call that did the unmapping returns, and before any other
//!   processor can do anything in the kernel at all.
//!
//! In between, the change is only noted ([`stale`]). A program that unmaps
//! a thousand pages in one call interrupts its other threads once.
//!
//! Whoever asks holds the kernel lock and waits for the answers, so the
//! answer cannot need the lock: a processor asked this reloads CR3 from an
//! interrupt that takes nothing (`idt::VEC_FLUSH`), or, if it is itself
//! waiting for the lock with interrupts off, from the loop it waits in
//! (`smp::while_waiting`). A processor that later loads the address space
//! was told nothing and needs nothing: loading it forgot everything.
//!
//! Adding a mapping where there was none needs none of this. A processor
//! does not remember that an address had nothing behind it.

use crate::percpu;
use core::sync::atomic::{AtomicBool, Ordering};

/// How many address spaces can be noted between one settling and the next.
/// A call changes one, or two when it moves pages between them; more than
/// this and every processor is asked.
const SPACES: usize = 4;

/// The address spaces that have lost mappings since the last [`sync`].
/// Changed only with the kernel lock held and interrupts off.
static mut STALE: [usize; SPACES] = [0; SPACES];
static mut NSTALE: usize = 0;
static mut EVERY_SPACE: bool = false;
/// Whether there is anything in them: what the allocator and the lock look
/// at, every time, before looking any further.
static ANY: AtomicBool = AtomicBool::new(false);

/// A mapping has been taken out of address space `cr3`, or changed: its
/// processors are to be told before it matters.
///
/// The caller holds the kernel lock. With one processor there is nobody to
/// tell.
pub fn stale(cr3: usize) {
    if percpu::count() == 1 {
        return;
    }
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
        let known = &mut *(&raw mut STALE);
        let n = *(&raw const NSTALE);
        if !known[..n].contains(&cr3) {
            if n < SPACES {
                known[n] = cr3;
                *(&raw mut NSTALE) = n + 1;
            } else {
                *(&raw mut EVERY_SPACE) = true;
            }
        }
        ANY.store(true, Ordering::Release);
        if flags & (1 << 9) != 0 {
            core::arch::asm!("sti", options(nostack, nomem));
        }
    }
}

/// A mapping every address space has has been taken out — a kernel stack's
/// (`kstack.rs`) — and every processor is to be told, whatever it has
/// loaded. The caller holds the kernel lock.
pub fn stale_everywhere() {
    if percpu::count() == 1 {
        return;
    }
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
        *(&raw mut EVERY_SPACE) = true;
        ANY.store(true, Ordering::Release);
        if flags & (1 << 9) != 0 {
            core::arch::asm!("sti", options(nostack, nomem));
        }
    }
}

/// Tell every other processor that has a changed address space loaded to
/// forget its translations, and wait until each has.
///
/// The caller holds the kernel lock. Nothing if nothing has changed, which
/// is nearly always.
#[inline]
pub fn sync() {
    if ANY.load(Ordering::Acquire) {
        settle();
    }
}

fn settle() {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
        ANY.store(false, Ordering::Release);
        let known = &*(&raw const STALE);
        let n = core::mem::replace(&mut *(&raw mut NSTALE), 0);
        let every = core::mem::replace(&mut *(&raw mut EVERY_SPACE), false);
        let me = percpu::index();
        let count = percpu::count();
        let mut asked = [0u64; percpu::MAX_CPUS / 64];
        for cpu in (0..count).filter(|&cpu| cpu != me) {
            if every || known[..n].contains(&percpu::cr3_of(cpu)) {
                percpu::ask_flush(cpu);
                crate::lapic::send(percpu::apic_id(cpu), crate::idt::VEC_FLUSH);
                asked[cpu / 64] |= 1 << (cpu % 64);
            }
        }
        // Each answers from wherever it is: ring 3, its idle loop, or the
        // wait for the lock this processor holds. None of those can be
        // waiting for this one, except for the lock.
        let mut turns: u64 = 0;
        while (0..count).any(|cpu| asked[cpu / 64] >> (cpu % 64) & 1 != 0 && percpu::flush_pending(cpu)) {
            crate::smp::while_waiting();
            core::hint::spin_loop();
            turns += 1;
            if turns == 1 << 34 {
                panic!("tlb: a processor asked to forget its translations has not answered");
            }
        }
        if flags & (1 << 9) != 0 {
            core::arch::asm!("sti", options(nostack, nomem));
        }
    }
}

/// On a processor that may have been asked: forget every translation, and
/// say so. Needs no lock, and is safe anywhere interrupts are off.
#[inline]
pub fn answer() {
    unsafe {
        if percpu::flush_asked() {
            crate::paging::write_cr3(crate::paging::read_cr3());
            percpu::flush_done();
        }
    }
}
