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
    /// Physical address of the futex word.
    ///
    /// Keying on (cr3, vaddr) made a futex inside a shared-memory region a
    /// *different* object in every task that mapped it, so cross-process
    /// synchronisation through shmem silently never woke anyone.
    paddr: usize,
    active: bool,
    /// PIT tick at which to give up. 0 = wait indefinitely.
    deadline: u64,
    /// Set by [`check_timeouts`] when the deadline passed, so the waiter can
    /// tell why it woke. The slot stays `active` until the waiter reads this,
    /// or it could be handed to a new waiter first and the answer lost.
    expired: bool,
}

struct FutexState {
    waiters: [FutexWaiter; MAX_FUTEX_WAITERS],
}

static FUTEX: IrqSpinLock<FutexState> = IrqSpinLock::new(FutexState {
    waiters: [FutexWaiter {
        tid: 0,
        paddr: 0,
        active: false,
        deadline: 0,
        expired: false,
    }; MAX_FUTEX_WAITERS],
});

const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Wait on a futex word. If `*addr == expected`, block the calling task.
/// Returns 0 on wake, 1 if value mismatch, u64::MAX on error.
pub fn futex_wait(addr: u64, expected: u32) -> u64 {
    wait(addr, expected, None)
}

/// As [`futex_wait`], giving up after `timeout_ticks`.
///
/// Returns 2 if the deadline passed before anything woke the task. A timeout
/// of 0 makes this a plain check of the word: it returns 1 immediately if the
/// value differs and 2 if it does not, without ever blocking.
///
/// Without this, a timed wait has to be built out of polling and yielding,
/// which burns a core for the length of the wait and cannot see a wake any
/// sooner than the next poll. That is what `Condvar::wait_timeout` and
/// `thread::park_timeout` are.
pub fn futex_wait_timeout(addr: u64, expected: u32, timeout_ticks: u64) -> u64 {
    wait(addr, expected, Some(timeout_ticks))
}

fn wait(addr: u64, expected: u32, timeout_ticks: Option<u64>) -> u64 {
    // Validate user pointer
    if addr == 0 || addr.checked_add(4).map_or(true, |end| end > USER_ADDR_LIMIT) {
        return u64::MAX;
    }
    if addr % 4 != 0 {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let cr3 = scheduler::current_task_cr3();

    // Confirm the word is actually mapped *before* taking the lock. Faulting on
    // it while holding FUTEX with interrupts disabled would deadlock: the fault
    // path wants to reschedule to the pager.
    if !unsafe { crate::paging::user_range_accessible(cr3, addr, 4, false) } {
        return u64::MAX;
    }
    let paddr = match unsafe { crate::paging::translate(cr3, addr as usize) } {
        Some(p) => p,
        None => return u64::MAX,
    };

    let mut state = FUTEX.lock();

    // Read the user word — we're in the same address space (syscall context)
    let current_val = {
        let _ua = crate::cpu::UserAccess::begin();
        unsafe { *(addr as *const u32) }
    };
    if current_val != expected {
        return 1;
    }

    // A zero timeout means "check, do not wait". The value still matches, so
    // there is nothing to report but the expiry — and blocking here, with no
    // time to wait, would mean never waking.
    if timeout_ticks == Some(0) {
        return TIMED_OUT;
    }

    // Find a free slot
    let slot = match state.waiters.iter().position(|w| !w.active) {
        Some(i) => i,
        None => return u64::MAX, // no free slots
    };

    // 0 in the slot means "no deadline", which is why the arithmetic saturates
    // rather than wrapping: a far-future deadline must stay far-future, not
    // land back on the sentinel.
    let deadline = match timeout_ticks {
        Some(t) => crate::pit::ticks().saturating_add(t),
        None => 0,
    };

    state.waiters[slot] = FutexWaiter {
        tid,
        paddr,
        active: true,
        deadline,
        expired: false,
    };

    // Block the task while holding the lock to prevent wake races
    scheduler::block_task(tid);
    drop(state);

    // Yield to let the scheduler pick another task
    scheduler::yield_now();

    // Woken. Release our slot unconditionally: futex_wake clears it, but a
    // signal-driven unblock does not, and a stale active slot both leaks the
    // entry and lets a later futex_wake unblock a task that is not waiting.
    //
    // Finding the slot still active means nobody took it, which is how a
    // deadline is distinguished from a wake: check_timeouts leaves it in place
    // precisely so this can read `expired` out of it.
    let mut state = FUTEX.lock();
    let mut timed_out = false;
    if let Some(w) = state
        .waiters
        .iter_mut()
        .find(|w| w.active && w.tid == tid && w.paddr == paddr)
    {
        timed_out = w.expired;
        w.active = false;
        w.expired = false;
    }
    drop(state);

    if timed_out { TIMED_OUT } else { 0 }
}

/// Returned by a timed wait whose deadline passed.
pub const TIMED_OUT: u64 = 2;

/// Wake up to `max_wake` tasks waiting on the futex at `addr`.
/// Returns the number of tasks woken.
pub fn futex_wake(addr: u64, max_wake: u64) -> u64 {
    if addr == 0 {
        return 0;
    }

    let cr3 = scheduler::current_task_cr3();
    let paddr = match unsafe { crate::paging::translate(cr3, addr as usize) } {
        Some(p) => p,
        None => return 0,
    };
    let mut woken = 0u64;

    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if woken >= max_wake {
            break;
        }
        // Skip one whose deadline already fired: it is awake and on its way
        // to reading `expired`, and counting it here would spend a wake that
        // another waiter is still blocked for.
        if waiter.active && !waiter.expired && waiter.paddr == paddr {
            waiter.active = false;
            scheduler::unblock_task(waiter.tid);
            woken += 1;
        }
    }

    woken
}

/// Expire waits whose deadline has passed. Called from `pit::tick`.
///
/// The slot is left active on purpose: the waiter needs to find it to learn
/// that it timed out rather than being woken, and freeing it here could hand
/// it to a new waiter before the old one has run.
pub fn check_timeouts() {
    // Interrupt context, so interrupts are already off.
    let now = crate::pit::ticks();
    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if waiter.active && !waiter.expired && waiter.deadline != 0 && now >= waiter.deadline {
            waiter.deadline = 0;
            waiter.expired = true;
            scheduler::unblock_task(waiter.tid);
        }
    }
}

/// Clean up futex waiters for a dead task.
pub fn cleanup_task(tid: usize) {
    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if waiter.active && waiter.tid == tid {
            waiter.active = false;
            waiter.expired = false;
            waiter.deadline = 0;
        }
    }
}
