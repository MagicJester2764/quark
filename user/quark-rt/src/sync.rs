//! Locks and the things built from them.
//!
//! The kernel gives one primitive — a futex — and that is the right one: Linux
//! exposes `futex`, Windows `WaitOnAddress`, macOS `__ulock_wait`, and on all
//! three a mutex is a word in user space with a system call only on the slow
//! path. What was missing here is not kernel surface but this: the library
//! everyone else already has.
//!
//! Every one of these is uncontended-fast. Taking a free lock is one
//! compare-exchange and no system call; only a task that actually has to wait
//! pays for one.
//!
//! They also work between processes, which is not an extra feature but a
//! consequence: the kernel keys its wait queue on the *physical* address of the
//! word, so a lock in shared memory is a lock between address spaces.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::syscall;

/// Three states, which is what makes unlocking cheap in the common case:
/// 0 free, 1 held with nobody waiting, 2 held with somebody waiting. An
/// unlock only makes a system call when the word says 2.
const FREE: u32 = 0;
const HELD: u32 = 1;
const CONTENDED: u32 = 2;

fn wait(word: &AtomicU32, expect: u32) {
    syscall::sys_futex_wait(word.as_ptr() as *const u32, expect);
}

fn wake(word: &AtomicU32, n: usize) {
    syscall::sys_futex_wake(word.as_ptr() as *const u32, n);
}

/// Mutual exclusion.
pub struct Mutex<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// The whole point is to be shared; the lock is what makes it sound.
unsafe impl<T: Send> Sync for Mutex<T> {}
unsafe impl<T: Send> Send for Mutex<T> {}

pub struct MutexGuard<'a, T> {
    lock: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Mutex { state: AtomicU32::new(FREE), value: UnsafeCell::new(value) }
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        // The fast path, and the only path most of the time.
        if self
            .state
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            return MutexGuard { lock: self };
        }
        self.lock_slow();
        MutexGuard { lock: self }
    }

    fn lock_slow(&self) {
        // Claim CONTENDED before sleeping, and keep claiming it on every
        // retry: the word has to say "somebody is waiting" for the unlock to
        // know it must wake us, and an unlock that races us here is why this
        // reloads rather than assuming.
        let mut c = self.state.swap(CONTENDED, Ordering::Acquire);
        while c != FREE {
            wait(&self.state, CONTENDED);
            c = self.state.swap(CONTENDED, Ordering::Acquire);
        }
    }

    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        if self
            .state
            .compare_exchange(FREE, HELD, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(MutexGuard { lock: self })
        } else {
            None
        }
    }

    fn unlock(&self) {
        if self.state.swap(FREE, Ordering::Release) == CONTENDED {
            wake(&self.state, 1);
        }
    }
}

impl<T> core::ops::Deref for MutexGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.unlock();
    }
}

/// Waiting for something to become true.
///
/// The sequence number is what closes the gap between releasing the lock and
/// going to sleep: a notification that lands in that window bumps it, and the
/// futex wait then returns at once instead of sleeping through it.
pub struct Condvar {
    seq: AtomicU32,
}

impl Default for Condvar {
    fn default() -> Self {
        Self::new()
    }
}

impl Condvar {
    pub const fn new() -> Self {
        Condvar { seq: AtomicU32::new(0) }
    }

    /// Release `guard`, wait, and take the lock again before returning.
    ///
    /// Spurious wake-ups are permitted, as everywhere else: call this in a
    /// loop around the condition you actually care about.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
        let lock = guard.lock;
        let seen = self.seq.load(Ordering::Relaxed);
        drop(guard);
        wait(&self.seq, seen);
        lock.lock()
    }

    pub fn notify_one(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        wake(&self.seq, 1);
    }

    pub fn notify_all(&self) {
        self.seq.fetch_add(1, Ordering::Release);
        wake(&self.seq, usize::MAX);
    }
}

/// Run something once, however many tasks arrive.
pub struct Once {
    state: AtomicU32,
}

const ONCE_NEW: u32 = 0;
const ONCE_RUNNING: u32 = 1;
const ONCE_DONE: u32 = 2;

impl Default for Once {
    fn default() -> Self {
        Self::new()
    }
}

impl Once {
    pub const fn new() -> Self {
        Once { state: AtomicU32::new(ONCE_NEW) }
    }

    pub fn call_once<F: FnOnce()>(&self, f: F) {
        if self.state.load(Ordering::Acquire) == ONCE_DONE {
            return;
        }
        match self.state.compare_exchange(
            ONCE_NEW,
            ONCE_RUNNING,
            Ordering::Acquire,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                f();
                self.state.store(ONCE_DONE, Ordering::Release);
                wake(&self.state, usize::MAX);
            }
            Err(_) => {
                // Somebody else is running it. Wait for them to finish rather
                // than returning before the thing has happened.
                while self.state.load(Ordering::Acquire) != ONCE_DONE {
                    wait(&self.state, ONCE_RUNNING);
                }
            }
        }
    }

    pub fn is_completed(&self) -> bool {
        self.state.load(Ordering::Acquire) == ONCE_DONE
    }
}

/// A count of permits.
pub struct Semaphore {
    count: AtomicU32,
}

impl Semaphore {
    pub const fn new(permits: u32) -> Self {
        Semaphore { count: AtomicU32::new(permits) }
    }

    pub fn acquire(&self) {
        loop {
            let c = self.count.load(Ordering::Relaxed);
            if c > 0 {
                if self
                    .count
                    .compare_exchange_weak(c, c - 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
                continue;
            }
            wait(&self.count, 0);
        }
    }

    pub fn try_acquire(&self) -> bool {
        loop {
            let c = self.count.load(Ordering::Relaxed);
            if c == 0 {
                return false;
            }
            if self
                .count
                .compare_exchange_weak(c, c - 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    pub fn release(&self) {
        self.count.fetch_add(1, Ordering::Release);
        wake(&self.count, 1);
    }
}

/// Many readers or one writer.
///
/// The top bit is the writer and the rest is a count of readers, so both can be
/// decided with one word and one compare-exchange.
pub struct RwLock<T> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

const WRITER: u32 = 1 << 31;

unsafe impl<T: Send + Sync> Sync for RwLock<T> {}
unsafe impl<T: Send> Send for RwLock<T> {}

pub struct ReadGuard<'a, T> {
    lock: &'a RwLock<T>,
}

pub struct WriteGuard<'a, T> {
    lock: &'a RwLock<T>,
}

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        RwLock { state: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    pub fn read(&self) -> ReadGuard<'_, T> {
        loop {
            let s = self.state.load(Ordering::Relaxed);
            if s & WRITER != 0 {
                wait(&self.state, s);
                continue;
            }
            if self
                .state
                .compare_exchange_weak(s, s + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return ReadGuard { lock: self };
            }
        }
    }

    pub fn write(&self) -> WriteGuard<'_, T> {
        loop {
            let s = self.state.load(Ordering::Relaxed);
            if s != 0 {
                wait(&self.state, s);
                continue;
            }
            if self
                .state
                .compare_exchange_weak(0, WRITER, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return WriteGuard { lock: self };
            }
        }
    }
}

impl<T> core::ops::Deref for ReadGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> Drop for ReadGuard<'_, T> {
    fn drop(&mut self) {
        // The last reader out wakes a writer; earlier ones would wake it to
        // find the lock still held.
        if self.lock.state.fetch_sub(1, Ordering::Release) == 1 {
            wake(&self.lock.state, usize::MAX);
        }
    }
}

impl<T> core::ops::Deref for WriteGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> core::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for WriteGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.state.store(0, Ordering::Release);
        wake(&self.lock.state, usize::MAX);
    }
}

/// A meeting point for a fixed number of tasks.
pub struct Barrier {
    count: AtomicU32,
    generation: AtomicU32,
    total: u32,
}

impl Barrier {
    pub const fn new(total: u32) -> Self {
        Barrier { count: AtomicU32::new(0), generation: AtomicU32::new(0), total }
    }

    /// Returns true for exactly one of the tasks, the one that completed it.
    pub fn wait(&self) -> bool {
        let gen = self.generation.load(Ordering::Relaxed);
        if self.count.fetch_add(1, Ordering::AcqRel) + 1 == self.total {
            self.count.store(0, Ordering::Relaxed);
            self.generation.fetch_add(1, Ordering::Release);
            wake(&self.generation, usize::MAX);
            true
        } else {
            while self.generation.load(Ordering::Acquire) == gen {
                wait(&self.generation, gen);
            }
            false
        }
    }
}
