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

pub const MAX_SIGFDS: usize = 32;

#[derive(Clone, Copy)]
struct SigFd {
    refs: usize,
    mask: u64,
}

const FREE: SigFd = SigFd { refs: 0, mask: 0 };

static mut SIGFDS: [SigFd; MAX_SIGFDS] = [FREE; MAX_SIGFDS];

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
unsafe fn sigfds() -> &'static mut [SigFd; MAX_SIGFDS] {
    unsafe { &mut *core::ptr::addr_of_mut!(SIGFDS) }
}

/// A new one, read for `mask`, with one reference: the descriptor about to
/// name it.
pub fn create(mask: u64) -> Option<usize> {
    let flags = irq_save();
    let made = unsafe {
        sigfds().iter().position(|s| s.refs == 0).inspect(|&s| {
            sigfds()[s] = SigFd { refs: 1, mask };
        })
    };
    irq_restore(flags);
    made
}

/// What `s` is read for.
pub fn mask(s: usize) -> u64 {
    if s >= MAX_SIGFDS {
        return 0;
    }
    let flags = irq_save();
    let mask = unsafe { sigfds()[s].mask };
    irq_restore(flags);
    mask
}

/// `s` is read for `mask` from now on.
pub fn set_mask(s: usize, mask: u64) {
    if s < MAX_SIGFDS {
        let flags = irq_save();
        unsafe {
            if sigfds()[s].refs != 0 {
                sigfds()[s].mask = mask;
            }
        }
        irq_restore(flags);
    }
}

/// Another descriptor names `s`.
pub fn retain(s: usize) {
    if s < MAX_SIGFDS {
        let flags = irq_save();
        unsafe {
            if sigfds()[s].refs != 0 {
                sigfds()[s].refs += 1;
            }
        }
        irq_restore(flags);
    }
}

/// A descriptor for `s` is gone; the last takes it with it.
pub fn release(s: usize) {
    if s < MAX_SIGFDS {
        let flags = irq_save();
        unsafe {
            let it = &mut sigfds()[s];
            it.refs = it.refs.saturating_sub(1);
        }
        irq_restore(flags);
    }
}
