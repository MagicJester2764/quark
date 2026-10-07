//! A program's timers that raise signals: POSIX's `timer_create`.
//!
//! A program has [`PER_PROGRAM`] of them, each a deadline and an interval on
//! one of three clocks, and what to do when the deadline passes: raise a
//! signal for the program, or for one task of it, carrying a value — or
//! nothing, for a timer a program only asks how long is left of. They are
//! kept in the program's record beside its descriptor table, as its alarm
//! is, because they are the program's: a forked child has none, and `exec`
//! ends them ([`Timers::clear`]). The room for them is made with the first.
//!
//! The clock sees to them — [`after`] says when it must next look, and
//! `signal::timers` raises what is [`due`] — one at a time, each re-armed
//! before its signal is raised: raising one may be the end of the program,
//! and of whatever the clock interrupted. A timer whose last signal is still
//! waiting when it fires again raises no other: what is waiting counts one
//! more overrun, as Linux's does, and a timer that fires faster than the
//! clock looks counts the intervals that went by the same way.

use core::alloc::Layout;

/// Timers a program may have: POSIX's least, `_POSIX_TIMER_MAX`.
pub const PER_PROGRAM: usize = 32;

/// The clocks a timer's absolute time is measured on, by Linux's numbers:
/// the date, and two that are the time since boot here.
const REALTIME: u64 = 0;
const MONOTONIC: u64 = 1;
const BOOTTIME: u64 = 7;

#[derive(Clone, Copy)]
struct Timer {
    used: bool,
    clock: u8,
    /// The signal it raises, 0 for none.
    signo: u8,
    /// The task it raises it for and no other, and that task's endpoint
    /// number, which no other task is ever given: 0 for the program.
    task: usize,
    endpoint: u64,
    value: u64,
    /// When it next fires, in the clock's nanoseconds since boot; 0 while it
    /// is disarmed.
    at: u64,
    every: u64,
}

const UNUSED: Timer = Timer { used: false, clock: 0, signo: 0, task: 0, endpoint: 0, value: 0, at: 0, every: 0 };

/// A program's timers, in its record (`fdtable::Program`): no room until it
/// makes its first, and the room given back with the record, or when the
/// program becomes another ([`Timers::clear`]).
pub struct Timers(*mut [Timer; PER_PROGRAM]);

impl Timers {
    pub const NONE: Timers = Timers(core::ptr::null_mut());

    /// The timers, if the program has made one.
    fn get(&mut self) -> Option<&'static mut [Timer; PER_PROGRAM]> {
        unsafe { self.0.as_mut() }
    }

    /// The timers, room made for them if there is none; `None` if there is
    /// no memory for it.
    fn make(&mut self) -> Option<&'static mut [Timer; PER_PROGRAM]> {
        if self.0.is_null() {
            let room = unsafe { alloc::alloc::alloc(Layout::new::<[Timer; PER_PROGRAM]>()) } as *mut [Timer; PER_PROGRAM];
            if room.is_null() {
                return None;
            }
            unsafe { room.write([UNUSED; PER_PROGRAM]) };
            self.0 = room;
        }
        self.get()
    }

    /// None at all: the program has become another.
    pub fn clear(&mut self) {
        let room = core::mem::replace(&mut self.0, core::ptr::null_mut());
        if !room.is_null() {
            unsafe { alloc::alloc::dealloc(room as *mut u8, Layout::new::<[Timer; PER_PROGRAM]>()) };
        }
    }
}

impl Drop for Timers {
    fn drop(&mut self) {
        self.clear();
    }
}

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

/// The timer `id` of `tid`'s program, if it has made one.
///
/// # Safety
/// Interrupts are off.
unsafe fn timer(tid: usize, id: usize) -> Option<&'static mut Timer> {
    if id >= PER_PROGRAM {
        return None;
    }
    let t = unsafe { &mut crate::fdtable::table_of(tid)?.timers.get()?[id] };
    t.used.then_some(t)
}

/// When a timer that was due at `at` and fires every `every` is next due,
/// at `now`, and how many times it would have fired besides in between.
fn next_after(at: u64, every: u64, now: u64) -> (u64, u64) {
    if every == 0 {
        return (0, 0);
    }
    let missed = (now - at) / every;
    (at.saturating_add((missed + 1).saturating_mul(every)), missed)
}

/// How long until timer `t` fires, as of `now`: 0 for one that is disarmed,
/// and never 0 for one that is not.
fn left(t: &Timer, now: u64) -> u64 {
    match t.at {
        0 => 0,
        at if at > now => at - now,
        at if t.every != 0 => next_after(at, t.every, now).0 - now,
        _ => 1,
    }
}

/// `SYS_PTIMER` 0: a timer for `tid`'s program on `clock`, raising `signo`
/// (0 for none) carrying `value` — or its own number, with `own` — for the
/// program, or for its task `task` alone. Disarmed. Its number, or `None`
/// for a clock there is no timer on, a task of another program, or a
/// program that has as many as it may.
pub fn create(tid: usize, clock: u64, signo: u64, own: bool, value: u64, task: usize) -> Option<usize> {
    if !matches!(clock, REALTIME | MONOTONIC | BOOTTIME) || signo > crate::signal::NSIG as u64 {
        return None;
    }
    let table = crate::fdtable::table_index(tid);
    if table == usize::MAX {
        return None;
    }
    let endpoint = if task == 0 {
        0
    } else if crate::fdtable::table_index(task) == table && crate::scheduler::task_is_live(task) {
        crate::cap::endpoint_of(task)
    } else {
        return None;
    };
    let flags = irq_save();
    let made = unsafe {
        // The room for its timers is made with its first.
        let Some(mine) = crate::fdtable::table_of(tid).and_then(|p| p.timers.make()) else {
            irq_restore(flags);
            return None;
        };
        mine.iter().position(|t| !t.used).map(|id| {
            mine[id] = Timer {
                used: true,
                clock: clock as u8,
                signo: signo as u8,
                task,
                endpoint,
                value: if own { id as u64 } else { value },
                at: 0,
                every: 0,
            };
            id
        })
    };
    irq_restore(flags);
    made
}

/// `SYS_PTIMER` 1: timer `id` of `tid`'s program next fires at `first` —
/// nanoseconds from `now`, or with `absolute` a time on its clock — and
/// then every `every`; a `first` of 0 disarms it. Answers with how it stood
/// before, nanoseconds left and between, and when it is now due (0 for
/// never), for the caller to tell the clock.
pub fn set(tid: usize, id: usize, absolute: bool, first: u64, every: u64, now: u64) -> Option<((u64, u64), u64)> {
    let flags = irq_save();
    let out = unsafe {
        timer(tid, id).map(|t| {
            let was = (left(t, now), t.every);
            t.at = match (first, absolute) {
                (0, _) => 0,
                (_, false) => now.saturating_add(first),
                // A date: the time since boot it will be then. A time
                // already gone is due at once.
                (_, true) if t.clock as u64 == REALTIME => {
                    first.saturating_sub(crate::clock::wall().saturating_sub(now)).max(1)
                }
                (_, true) => first,
            };
            t.every = if t.at == 0 { 0 } else { every };
            (was, t.at)
        })
    };
    irq_restore(flags);
    out
}

/// `SYS_PTIMER` 2: how timer `id` of `tid`'s program stands at `now`:
/// nanoseconds left, and between firings.
pub fn get(tid: usize, id: usize, now: u64) -> Option<(u64, u64)> {
    let flags = irq_save();
    let out = unsafe { timer(tid, id).map(|t| (left(t, now), t.every)) };
    irq_restore(flags);
    out
}

/// `SYS_PTIMER` 3: timer `id` of `tid`'s program is no more. A signal of
/// its that is waiting stays waiting.
pub fn delete(tid: usize, id: usize) -> bool {
    let flags = irq_save();
    let done = unsafe { timer(tid, id).map(|t| *t = UNUSED).is_some() };
    irq_restore(flags);
    done
}

/// A timer that has fired: whose, which, and what to raise — `missed` is
/// how many more times it would have fired besides, had the clock looked.
pub struct Due {
    /// A task of the program.
    pub tid: usize,
    /// The task to raise it for alone, or 0 for the program.
    pub task: usize,
    pub signo: u8,
    pub id: usize,
    pub value: u64,
    pub missed: u64,
}

/// One timer due at `now`, re-armed or disarmed already: what it raises.
/// `None` when no more is due. A timer that raises nothing is re-armed and
/// passed over, and so is one for a task that has gone.
pub fn due(now: u64) -> Option<Due> {
    let flags = irq_save();
    let mut found = None;
    unsafe {
        'tables: for (table, p) in crate::fdtable::programs() {
            let Some(mine) = p.timers.get() else { continue };
            for (id, t) in mine.iter_mut().enumerate() {
                if !t.used || t.at == 0 || t.at > now {
                    continue;
                }
                let (next, missed) = next_after(t.at, t.every, now);
                t.at = next;
                if t.signo == 0 || (t.task != 0 && crate::cap::endpoint_of(t.task) != t.endpoint) {
                    continue;
                }
                let Some(tid) = crate::fdtable::a_task_of(table) else { continue };
                found = Some(Due { tid, task: t.task, signo: t.signo, id, value: t.value, missed });
                break 'tables;
            }
        }
    }
    irq_restore(flags);
    found
}

/// When the earliest timer will be due once those due at `now` have been
/// seen to ([`due`]), or `u64::MAX` if none will be.
pub fn after(now: u64) -> u64 {
    let flags = irq_save();
    let mut next = u64::MAX;
    unsafe {
        for t in crate::fdtable::programs().filter_map(|(_, p)| p.timers.get()).flatten() {
            if !t.used || t.at == 0 {
                continue;
            }
            let at = if t.at > now { t.at } else { next_after(t.at, t.every, now).0 };
            if at != 0 {
                next = next.min(at);
            }
        }
    }
    irq_restore(flags);
    next
}
