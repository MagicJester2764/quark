/// Futex (fast userspace mutex) support.
///
/// Provides wait/wake operations on user-space atomic words,
/// enabling efficient blocking synchronization primitives.

use crate::scheduler;
use crate::sync::IrqSpinLock;

const MAX_FUTEX_WAITERS: usize = 64;

#[derive(Clone, Copy)]
struct FutexWaiter {
    tid: usize,
    cr3: usize,
    vaddr: usize,
    active: bool,
}

struct FutexState {
    waiters: [FutexWaiter; MAX_FUTEX_WAITERS],
}

static FUTEX: IrqSpinLock<FutexState> = IrqSpinLock::new(FutexState {
    waiters: [FutexWaiter {
        tid: 0,
        cr3: 0,
        vaddr: 0,
        active: false,
    }; MAX_FUTEX_WAITERS],
});

const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Wait on a futex word. If `*addr == expected`, block the calling task.
/// Returns 0 on wake, 1 if value mismatch, u64::MAX on error.
pub fn futex_wait(addr: u64, expected: u32) -> u64 {
    // Validate user pointer
    if addr == 0 || addr.checked_add(4).map_or(true, |end| end > USER_ADDR_LIMIT) {
        return u64::MAX;
    }
    if addr % 4 != 0 {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let cr3 = scheduler::current_task_cr3();

    let mut state = FUTEX.lock();

    // Read the user word — we're in the same address space (syscall context)
    let current_val = unsafe { *(addr as *const u32) };
    if current_val != expected {
        return 1;
    }

    // Find a free slot
    let slot = match state.waiters.iter().position(|w| !w.active) {
        Some(i) => i,
        None => return u64::MAX, // no free slots
    };

    state.waiters[slot] = FutexWaiter {
        tid,
        cr3,
        vaddr: addr as usize,
        active: true,
    };

    // Block the task while holding the lock to prevent wake races
    scheduler::block_task(tid);
    drop(state);

    // Yield to let the scheduler pick another task
    scheduler::yield_now();

    0
}

/// Wake up to `max_wake` tasks waiting on the futex at `addr`.
/// Returns the number of tasks woken.
pub fn futex_wake(addr: u64, max_wake: u64) -> u64 {
    if addr == 0 {
        return 0;
    }

    let cr3 = scheduler::current_task_cr3();
    let vaddr = addr as usize;
    let mut woken = 0u64;

    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if woken >= max_wake {
            break;
        }
        if waiter.active && waiter.cr3 == cr3 && waiter.vaddr == vaddr {
            waiter.active = false;
            scheduler::unblock_task(waiter.tid);
            woken += 1;
        }
    }

    woken
}

/// Clean up futex waiters for a dead task.
pub fn cleanup_task(tid: usize) {
    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if waiter.active && waiter.tid == tid {
            waiter.active = false;
        }
    }
}
