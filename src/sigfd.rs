//! A descriptor that is read for signals: Linux's `signalfd`.
//!
//! What it names is a set of signals, and nothing else: whose signals a read
//! takes is the reader's — its own task's first, then its program's — so a
//! forked child that reads its copy reads its own, as on Linux. A program
//! holds the signals back and waits on this with everything else its event
//! loop waits on; a signal it does not hold back is run or does what it
//! does, and is never here to read.
//!
//! The set is the object's, so a second descriptor for it (`dup`, a fork,
//! one sent down a stream) sees a change made through either. A read is
//! `signal::read_for`.

#[derive(Clone, Copy)]
struct SigFd {
    refs: usize,
    mask: u64,
}

/// Every one, by its number: made when a program makes one, given back with
/// its last descriptor (`table.rs`). Thirty-two for the whole machine, they
/// were.
static mut SIGFDS: crate::table::Table<SigFd> = crate::table::Table::new(crate::table::MOST);

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

/// # Safety
/// Interrupts are off.
unsafe fn sigfds() -> &'static mut crate::table::Table<SigFd> {
    unsafe { &mut *core::ptr::addr_of_mut!(SIGFDS) }
}

/// A new one, read for `mask`, with one reference: the descriptor about to
/// name it.
pub fn create(mask: u64) -> Option<usize> {
    if !crate::reclaim::may_make() {
        return None;
    }
    let flags = irq_save();
    let made = unsafe { sigfds().lowest_free(0).filter(|&s| sigfds().fill_at(s, SigFd { refs: 1, mask }).is_ok()) };
    irq_restore(flags);
    made
}

/// What `s` is read for.
pub fn mask(s: usize) -> u64 {
    let flags = irq_save();
    let mask = unsafe { sigfds().get(s).map_or(0, |it| it.mask) };
    irq_restore(flags);
    mask
}

/// `s` is read for `mask` from now on.
pub fn set_mask(s: usize, mask: u64) {
    let flags = irq_save();
    unsafe {
        if let Some(it) = sigfds().get(s) {
            it.mask = mask;
        }
    }
    irq_restore(flags);
}

/// Another descriptor names `s`.
pub fn retain(s: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(it) = sigfds().get(s) {
            it.refs += 1;
        }
    }
    irq_restore(flags);
}

/// A descriptor for `s` is gone; the last takes it with it.
pub fn release(s: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(it) = sigfds().get(s) {
            it.refs = it.refs.saturating_sub(1);
            if it.refs == 0 {
                sigfds().empty(s);
            }
        }
    }
    irq_restore(flags);
}
