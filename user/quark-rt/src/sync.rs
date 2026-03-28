/// User-space mutex using futex syscalls.
///
/// 3-state design (like Linux): 0=unlocked, 1=locked-no-waiters, 2=locked-with-waiters.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{sys_futex_wait, sys_futex_wake};

pub struct Mutex<T> {
    state: AtomicU32,
    data: UnsafeCell<T>,
}

unsafe impl<T: Send> Send for Mutex<T> {}
unsafe impl<T: Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(data: T) -> Self {
        Mutex {
            state: AtomicU32::new(0),
            data: UnsafeCell::new(data),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        // Fast path: uncontended
        if self.state.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            return MutexGuard { mutex: self };
        }

        // Slow path: mark as contended and wait
        loop {
            // If state was already non-zero, swap to 2 (contended)
            // If it was 0, we just acquired it (swap returns old value 0)
            let old = self.state.swap(2, Ordering::Acquire);
            if old == 0 {
                return MutexGuard { mutex: self };
            }

            // Wait until the state might change
            sys_futex_wait(&self.state as *const AtomicU32 as *const u32, 2);
        }
    }
}

pub struct MutexGuard<'a, T> {
    mutex: &'a Mutex<T>,
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // Swap to 0 (unlocked), check if there were waiters
        let old = self.mutex.state.swap(0, Ordering::Release);
        if old == 2 {
            // There were waiters — wake one
            sys_futex_wake(&self.mutex.state as *const AtomicU32 as *const u32, 1);
        }
    }
}
