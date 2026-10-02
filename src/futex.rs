/// Futex (fast userspace mutex) support.
///
/// Provides wait/wake operations on user-space atomic words,
/// enabling efficient blocking synchronization primitives.

use crate::scheduler;
use crate::sync::IrqSpinLock;

const MAX_FUTEX_WAITERS: usize = 64;

/// Which word a task is waiting on.
///
/// A word in memory the program does not own — shared memory, a file mapped
/// shared — is the same word in every program that maps it, wherever each
/// has it: it is named by its frame. Keying on (cr3, vaddr) made a futex
/// inside a shared-memory region a *different* object in every task that
/// mapped it, so cross-process synchronisation through shmem silently never
/// woke anyone.
///
/// A word in memory the program owns is that program's and nobody else's:
/// it is named by the address space and the address. Its frame says less
/// than that and more. After a `fork` the parent and the child have the
/// page in one frame until one of them writes it, and are not waiting on
/// one word; and the one that writes has it in another frame from then on,
/// so a thread that waited before the write was never found by the wake
/// that came after it — which is every wake, since what is waited for is a
/// write to that word.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    /// The address space, for a word in memory it owns; 0 for a frame.
    space: usize,
    /// The address there, or the frame's.
    at: usize,
}

const NO_KEY: Key = Key { space: 0, at: 0 };

/// The key of the word at `addr` in `cr3`, if there is a page there.
fn key_of(cr3: usize, addr: u64) -> Option<Key> {
    unsafe {
        let own = crate::paging::leaf_flags(cr3, addr as usize)
            .is_some_and(|f| f & crate::paging::OWNED != 0);
        if own {
            return Some(Key { space: cr3, at: addr as usize });
        }
        crate::paging::translate(cr3, addr as usize).map(|at| Key { space: 0, at })
    }
}

#[derive(Clone, Copy)]
struct FutexWaiter {
    tid: usize,
    /// The word it waits on.
    key: Key,
    active: bool,
    /// When to give up, in the clock's nanoseconds. 0 = wait indefinitely.
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
        key: NO_KEY,
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

/// As [`futex_wait`], giving up after `timeout_ns` nanoseconds.
///
/// Returns 2 if the deadline passed before anything woke the task. A timeout
/// of 0 makes this a plain check of the word: it returns 1 immediately if the
/// value differs and 2 if it does not, without ever blocking.
///
/// Without this, a timed wait has to be built out of polling and yielding,
/// which burns a core for the length of the wait and cannot see a wake any
/// sooner than the next poll. That is what `Condvar::wait_timeout` and
/// `thread::park_timeout` are.
pub fn futex_wait_timeout(addr: u64, expected: u32, timeout_ns: u64) -> u64 {
    wait(addr, expected, Some(timeout_ns))
}

fn wait(addr: u64, expected: u32, timeout_ns: Option<u64>) -> u64 {
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
    let usable = unsafe {
        crate::paging::back_range(cr3, addr, 4, false).is_ok()
            && crate::paging::user_range_accessible(cr3, addr, 4, false)
    };
    if !usable {
        return u64::MAX;
    }
    let key = match key_of(cr3, addr) {
        Some(k) => k,
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
    if timeout_ns == Some(0) {
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
    let deadline = match timeout_ns {
        Some(t) => crate::clock::after(t),
        None => 0,
    };

    state.waiters[slot] = FutexWaiter {
        tid,
        key,
        active: true,
        deadline,
        expired: false,
    };

    // Block the task while holding the lock to prevent wake races
    scheduler::block_task(tid);
    if deadline != 0 {
        crate::clock::due(deadline);
    }
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
        .find(|w| w.active && w.tid == tid && w.key == key)
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
    // No page there is no waiter there: a wait gives the page its memory
    // before it waits.
    let key = match key_of(cr3, addr) {
        Some(k) => k,
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
        if waiter.active && !waiter.expired && waiter.key == key {
            waiter.active = false;
            scheduler::unblock_task(waiter.tid);
            woken += 1;
        }
    }

    woken
}

/// Expire waits whose deadline has passed at `now`, and say when the next
/// one does: `u64::MAX` if none is waiting on a time. Called from the clock
/// (`clock::expire`).
///
/// The slot is left active on purpose: the waiter needs to find it to learn
/// that it timed out rather than being woken, and freeing it here could hand
/// it to a new waiter before the old one has run.
pub fn check_timeouts(now: u64) -> u64 {
    // Interrupt context, so interrupts are already off.
    let mut next = u64::MAX;
    let mut state = FUTEX.lock();
    for waiter in state.waiters.iter_mut() {
        if !waiter.active || waiter.expired || waiter.deadline == 0 {
            continue;
        }
        if now >= waiter.deadline {
            waiter.deadline = 0;
            waiter.expired = true;
            scheduler::unblock_task(waiter.tid);
        } else {
            next = next.min(waiter.deadline);
        }
    }
    next
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
