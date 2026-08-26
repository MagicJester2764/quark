/// Interrupt-safe spin lock for kernel use.
///
/// Saves RFLAGS and disables interrupts before acquiring the lock,
/// restores IF on drop. Prevents deadlock from IRQ handlers that
/// might also need the lock.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

pub struct IrqSpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for IrqSpinLock<T> {}
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    pub const fn new(data: T) -> Self {
        IrqSpinLock {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }

    /// Acquire the lock with interrupts disabled.
    ///
    /// Quark is uniprocessor and this lock holds `cli` for its whole lifetime,
    /// so no other execution context can be running while it is held. Finding
    /// it already taken therefore means *this* code path is re-entering a lock
    /// it already owns — which as a spin would be an unbreakable hang with
    /// interrupts off and no output. Report it instead.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        let saved_flags: u64;
        unsafe {
            core::arch::asm!("pushfq; pop {}; cli", out(reg) saved_flags, options(nostack));
        }
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            panic!("IrqSpinLock: re-entrant acquire (deadlock)");
        }
        IrqSpinLockGuard {
            lock: self,
            saved_flags,
        }
    }
}

pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    saved_flags: u64,
}

impl<T> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
        if self.saved_flags & (1 << 9) != 0 {
            unsafe {
                core::arch::asm!("sti", options(nostack, nomem));
            }
        }
    }
}
