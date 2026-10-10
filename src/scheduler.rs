/// Preemptive round-robin scheduler for the Quark microkernel.
///
/// Uses a ready queue of task IDs. The PIT timer IRQ calls `schedule()`
/// to preempt the running task and switch to the next ready one.
///
/// Which task is running is each processor's own (`percpu::current`), and
/// task 0 is what a processor runs when it has nothing to: the idle loop,
/// one task with a saved context per processor. It is in no ready queue.
/// It is what is left when the queues are empty.
///
/// **More than one processor.** Each processor has run queues of its own,
/// under the kernel lock (`klock.rs`): a task waits on the processor it last
/// ran on, unless another would run it sooner (`runq::place_for`); a
/// processor with nothing of its own takes from the busiest, and one with
/// fewer waiting than the busiest takes one every fourth tick. A task that
/// is preempted waits where it was, and is run next by another only when
/// that one would otherwise have nothing to do.
///
/// What a second processor changes is that a task which is not the caller
/// may be *running* — in ring 3, on another processor — at the moment the
/// kernel does something to it. Two things are done to tasks from outside,
/// ending them and stopping them, and for both the rule is the same: the
/// task is marked, its processor is interrupted, and the task is looked at
/// again every time it comes into the kernel from ring 3 ([`arrived`]). So
/// a task that has been ended may run a little longer in ring 3, and never
/// again in the kernel; and two things wait for it to be off its
/// processor ([`ON_CPU`]): being collected by its parent, and being taken
/// apart.

use crate::context;
use crate::task::{Task, TaskRec, TaskState, KERNEL_STACK_SIZE, MAX_TASKS};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

mod runq;

/// Each processor's ticks, for what it does every so many of them.
static mut TICKS: [u32; crate::percpu::MAX_CPUS] = [0; crate::percpu::MAX_CPUS];

/// Whether each processor's tick is stopped: while it has nothing to run
/// (`stop_tick`, `start_tick`).
static mut TICK_STOPPED: [bool; crate::percpu::MAX_CPUS] = [false; crate::percpu::MAX_CPUS];

/// The task table: a record for each task, by its id (`table.rs`). A slot
/// with no task has no record.
static mut TASKS: crate::table::Table<TaskRec> = crate::table::Table::new(MAX_TASKS);

/// Slot `tid` of the task table: `Some` with the task's record while there is
/// a task there. Interrupts must be off, as for any look at the table.
#[inline]
pub(crate) unsafe fn slot(tid: usize) -> &'static mut Option<TaskRec> {
    unsafe { (*core::ptr::addr_of_mut!(TASKS)).slot(tid) }
}

/// Task `tid`'s record. Interrupts must be off.
#[inline]
pub(crate) unsafe fn rec(tid: usize) -> Option<&'static mut TaskRec> {
    unsafe { (*core::ptr::addr_of_mut!(TASKS)).get(tid) }
}

/// The table itself: what fills a slot and empties one.
#[inline]
unsafe fn table() -> &'static mut crate::table::Table<TaskRec> {
    unsafe { &mut *core::ptr::addr_of_mut!(TASKS) }
}

/// What the scheduler keeps about a task, in its record (`TaskRec::sched`):
/// each was an array of `MAX_TASKS`, and a task with no record reads as
/// `PerTask::new()`, which is what an empty slot of those arrays held.
pub struct PerTask {
    /// Per-task wait state. If true, the task is blocked in sys_wait.
    wait_blocked: bool,
    /// TID of the dead child collected for a waiting parent. 0 = none yet.
    wait_result: usize,
    /// Which child a task blocked in a wait is waiting for: 0 for whichever goes
    /// first. Another child going is not what it asked to be woken for.
    wait_target: usize,
    /// Or which process group of children, when that is what it named; 0 when
    /// it named a child or none.
    wait_group: u64,
    /// What else it asked to hear of besides a child ending: `job::HAS_STOPPED`,
    /// `job::HAS_CONTINUED`.
    wait_reports: u8,
    /// Set when a waiter is woken to look again rather than with a dead child:
    /// one of its children has stopped, or been continued.
    wait_again: bool,
    /// Tasks that may not run: every task of a program a signal has stopped
    /// (`job.rs`).
    ///
    /// It is not a state of its own, because a task goes on being whatever it
    /// was — blocked in a call, asleep, or ready to run — and has to be that
    /// again when the program is continued. A held task is simply never put on
    /// a ready queue, and never switched to. What would have made it runnable
    /// makes it `Ready` and in no queue, and continuing the program queues
    /// every task of it that is.
    held: bool,
    /// The process id of the program each task belongs to: the number, never
    /// given out twice, of the task the program began as.
    ///
    /// A task id is a slot in a table, and the next task made is given the lowest
    /// one free — usually the one that has just been let go. Every Unix program
    /// that remembers a child assumes the opposite: that a number it was told a
    /// moment ago does not come back as somebody else. A shell would not wait
    /// for a command because it had been given the number of the last thing it
    /// had run in the background.
    ///
    /// A program started by a spawner or by `fork` has its first task's number,
    /// which `exec` leaves alone; a thread has its program's. Kept by task and
    /// not with the program's table, because the table goes when the program
    /// dies and a parent asks about a child after that.
    process_id: u64,
    /// Exit code of the child that woke a waiter, captured at wake time.
    ///
    /// It cannot be read from the task afterwards: waking the parent also sets
    /// REAPED, which makes `reap_dead` free the slot, and `child_exit_code` then
    /// falls back to 0. Every non-zero status was being lost that way.
    wait_code: i32,
    /// Per-task "reaped" flag. If true, parent has collected the exit via sys_wait (or has no parent).
    reaped: bool,
    /// Ticks left in each task's slice: how long it may run before it is
    /// preempted, which is `usage::slice_for` its program's niceness when the
    /// scheduler chooses it — three ticks for one that has said nothing.
    ///
    /// The scheduler used to reschedule on every timer interrupt, which is a
    /// quantum of one tick and a context switch a hundred times a second whether
    /// or not anything else wanted the CPU. It also meant a task could never
    /// finish a short burst of work without being interrupted partway through it.
    ///
    /// Three ticks is thirty milliseconds. Anything interactive blocks long before
    /// that — a server blocks on its next receive, a client on its next call — so
    /// this only ever bounds work that is genuinely CPU-bound; and so does how
    /// nice a program is, which is the share of its band it has when it and
    /// another are both computing.
    slice_left: u32,
    /// How far each task has run, as its band sees it: the nanoseconds it has
    /// had, each counted for more the nicer its program is (`usage::weighted`).
    /// Within a band the task that has run least goes next, so that two
    /// programs computing side by side have the band in proportion to their
    /// weights — on one processor or sixteen. In the order they were queued,
    /// with one queue for every processor, a task with a short turn comes round
    /// sooner, and the share a short turn was to cut evened out: a program at
    /// nice 10 had about half of what one at nought did, on four processors.
    vrun: u64,
    /// How nice it is, -20 to 19: the task's own, given it by whoever made
    /// it (`SYS_SCHED`; `SYS_NICE` says it of every task of a program). It
    /// was the program's, and no thread could be nicer than its siblings.
    nice: i8,
    /// Its class — [`SCHED_OTHER`], [`SCHED_FIFO`] or [`SCHED_RR`], Linux's
    /// numbers — and in a real-time one its own priority, 1 to 99; 0 in
    /// none. Its threads and children are given both.
    policy: u8,
    rt_base: u8,
    /// The real-time priority it runs at: its own, or the better one of a
    /// task waiting on it, as `priority` is its band (`refresh_priority`).
    rt: u8,
    /// Has just yielded: passed over once if anything else in its queue is
    /// ready — while the queue's turn is the one it was queued in
    /// (`yield_turn`). Once, and not sent to the back for good, or a task
    /// yielding while it waits for another would wait behind everybody for
    /// ever.
    yielded: bool,
    yield_turn: u64,
    /// Woken by the answer to its call, it runs on what was left of the
    /// turn the call was made in (`unblock_task_next`) — kept when it is
    /// chosen, and not made a whole one.
    handed: bool,
    /// The processor each task is running on, or [`NO_CPU`].
    ///
    /// A task is on a processor from the switch to it until the switch away
    /// from it — in ring 3, in the kernel, or waiting at the kernel's door for
    /// the lock. Both switches are made with the kernel lock held, and whoever
    /// holds the lock afterwards finds the second one complete: the lock is
    /// not given up half way through a switch.
    ///
    /// It is what "this task is not running" has to mean with more than one
    /// processor. A dead task's state says nothing about it: ended from
    /// another processor, a task goes on in ring 3 until the interrupt that
    /// tells its processor arrives, on its own kernel stack and in its own
    /// address space. Neither may be freed under it.
    on_cpu: u16,
    /// The processor it last ran on — or runs on — as `SYS_CPU_INFO` says
    /// it: Linux's thirty-ninth field of `/proc/PID/stat`. Kept when it
    /// leaves, where `on_cpu` is not.
    last_cpu: u16,
    /// Dead, ended from another processor while it ran, and still on its own:
    /// what is said when a task dies — its parent woken to collect it, SIGCHLD
    /// — has not been said yet, and is said by its processor when it leaves
    /// the task ([`schedule_inner`]). A parent told sooner would collect a
    /// child that is still running.
    unannounced: bool,
    /// Whether its processor held the one lock (`klock`) when it was switched
    /// out: what it expects again when it is switched back to, and gives
    /// back on its way out — a system call, a fault or an interrupt that
    /// took it is still in it. A task made, or forked, starts in a
    /// trampoline that gives it up, and expects it too.
    kl_held: bool,
    /// The reaping epoch it was stamped with once it was ready to be taken
    /// apart and in no queue, or 0 (`reap_one`, `CHOOSING`).
    reap_stamp: u64,
    /// Dead, and nobody is going to wait for it: a thread that is joined
    /// through the word it asked to have cleared ([`joined_by_word`]). Decided
    /// as it dies, when the word is forgotten.
    unwaited: bool,
    /// What the system call each task is in has checked of its program's
    /// memory, and may touch with interrupts off until it returns: these pages
    /// are not taken away to be written out (`reclaim.rs`). A call checks a
    /// buffer, waits — for a pipe to have something in it — and then copies
    /// with a lock held, where a page that had gone in the meantime could not
    /// be waited for.
    pinned: [(u64, u64); PINS],
    npinned: u8,
    /// The band whose run queue the task is in, or [`NOT_QUEUED`]; the
    /// processor whose it is; and which part of it (`runq::IN_RT`,
    /// `IN_FRONT`, `IN_FAIR`).
    queued: u8,
    queued_on: u16,
    heap_in: u8,
    /// The tasks either side of it in a queue's front list.
    run_next: u16,
    run_prev: u16,
    /// Its links in a queue's heap: its first child, its next sibling, and
    /// its elder sibling — or its parent, if it is the first child.
    heap_child: u16,
    heap_next: u16,
    heap_prev: u16,
    /// When it was put in its queue, in the order of all of them: of two
    /// otherwise equal, the first queued goes first.
    seq: u64,
    /// What it weighed when it was put in its queue (`usage::weight`), and
    /// takes out of the queue's load with it.
    weight: u32,
    /// The processor, and the band there, whose measure its run time is in:
    /// where it last ran or was last put to wait, which are not the same
    /// for a task put to wait somewhere and taken out again unrun — stopped,
    /// say — nor for one handed a call, or lent a caller's band
    /// (`runq::moved`).
    measured_on: u16,
    measured_in: u8,
    /// When it last left a processor, by the clock.
    ran_at: u64,
    /// The processors it may run on, a bit each of 256: all of them unless
    /// it, or its program, or whoever manages it said otherwise
    /// (`SYS_AFFINITY`). Given to the threads and children it makes, and
    /// kept by `exec`.
    allowed: [u64; 4],
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            wait_blocked: false,
            wait_result: 0,
            wait_target: 0,
            wait_group: 0,
            wait_reports: 0,
            wait_again: false,
            held: false,
            process_id: 0,
            wait_code: 0,
            reaped: false,
            slice_left: 0,
            vrun: 0,
            nice: 0,
            policy: SCHED_OTHER,
            rt_base: 0,
            rt: 0,
            yielded: false,
            yield_turn: 0,
            handed: false,
            on_cpu: NO_CPU,
            last_cpu: 0,
            unannounced: false,
            kl_held: true,
            reap_stamp: 0,
            unwaited: false,
            pinned: [(0, 0); PINS],
            npinned: 0,
            queued: NOT_QUEUED,
            queued_on: 0,
            heap_in: 0,
            run_next: END,
            run_prev: END,
            heap_child: END,
            heap_next: END,
            heap_prev: END,
            seq: 0,
            weight: 0,
            measured_on: 0,
            measured_in: PRIO_NORMAL,
            ran_at: 0,
            allowed: [u64::MAX; 4],
        }
    }
}

/// What the scheduler keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for —
/// what an empty slot of the arrays these were answered, and where a write
/// for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match rec(tid) {
            Some(r) => &mut r.sched,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();

/// Every task's id, in order. Each step looks at the table afresh, so a
/// walk's body may change it. Interrupts must be off.
/// How many locks the tasks' records share: a task's is the one its number
/// picks. A lock in each record would go with the record at its reap, under
/// whoever was waiting for it.
const RECORD_LOCKS: usize = 256;

static RECORDS: [crate::sync::Padded<crate::sync::IrqSpinLock<()>>; RECORD_LOCKS] =
    [const { crate::sync::Padded(crate::sync::IrqSpinLock::new(crate::sync::RANK_TASK, "a task's record", ())) }; RECORD_LOCKS];

/// The lock of `tid`'s record: its IPC state (`ipc.rs`), its state as the
/// scheduler has it — blocked, ready, dead — and the place it is lent.
pub fn record_lock(tid: usize) -> &'static crate::sync::IrqSpinLock<()> {
    &RECORDS[tid % RECORD_LOCKS].0
}

/// `tid`'s record's lock, unless this processor holds it already: what a
/// step that changes another task's state takes.
pub fn lock_record(tid: usize) -> Option<crate::sync::IrqSpinLockGuard<'static, ()>> {
    record_lock(tid).lock_unless_held()
}

/// The records of `a` and `b`, in the order of where their locks are — or
/// the one, if they share it: for what changes both, a call or a send, or a
/// receive taking a sender's message.
pub fn lock_records(
    a: usize,
    b: usize,
) -> (crate::sync::IrqSpinLockGuard<'static, ()>, Option<crate::sync::IrqSpinLockGuard<'static, ()>>) {
    let (la, lb) = (record_lock(a), record_lock(b));
    if core::ptr::eq(la, lb) {
        return (la.lock(), None);
    }
    let (first, second) = if (la as *const crate::sync::IrqSpinLock<()> as usize) < (lb as *const crate::sync::IrqSpinLock<()> as usize) {
        (la, lb)
    } else {
        (lb, la)
    };
    let held = first.lock();
    (held, Some(second.lock_second()))
}

pub(crate) fn tids() -> impl Iterator<Item = usize> {
    tids_from(0)
}

/// The first task at or past `from`: `SYS_TASK_NEXT`.
pub fn next_task(from: usize) -> Option<usize> {
    let flags = irq_save();
    let next = unsafe { table().next_used(from) };
    irq_restore(flags);
    next
}

/// Every task there is, from task `start` up and then round from 0 to
/// before it: for a walk that takes turns. As [`tids`], each step looks
/// afresh.
pub(crate) fn tids_from(start: usize) -> impl Iterator<Item = usize> {
    let (mut at, mut wrapped) = (start, false);
    core::iter::from_fn(move || {
        loop {
            match unsafe { table().next_used(at) } {
                Some(i) if !wrapped || i < start => {
                    at = i + 1;
                    return Some(i);
                }
                _ if !wrapped => (at, wrapped) = (0, true),
                _ => return None,
            }
        }
    })
}

// Simple circular ready queue (array of TIDs)
/// Scheduling bands, best first. A task runs only when nothing better is
/// waiting; within a band they take turns.
///
/// The bands exist because round-robin gives a program burning CPU exactly the
/// same share as the keyboard driver, which has nothing to do until a key
/// arrives and everything to do the moment one does. What separates them is
/// not how much CPU they want but how soon they need it, and that is a
/// property of the job rather than of its recent behaviour — so it is declared
/// rather than inferred.
pub const NUM_PRIORITIES: usize = 4;
/// Interrupt-driven hardware: it must answer the device before the buffer
/// behind it overflows.
pub const PRIO_DRIVER: u8 = 0;
/// The system's servers, which programs are usually blocked waiting on.
pub const PRIO_SERVER: u8 = 1;
/// Ordinary programs, and the default for anything that asks for nothing.
pub const PRIO_NORMAL: u8 = 2;
/// The idle task, and nothing else.
pub const PRIO_IDLE: u8 = 3;

/// The classes a task can be in, by Linux's numbers: ordinary, and the two
/// real-time ones. Within a band a real-time task runs before every other,
/// the best priority first: one in FIFO until it blocks, yields or something
/// better is ready; one in RR for a turn of [`RR_TICKS`] among its equals.
pub const SCHED_OTHER: u8 = 0;
pub const SCHED_FIFO: u8 = 1;
pub const SCHED_RR: u8 = 2;

/// A round-robin task's turn among those of its priority: a hundred
/// milliseconds, as Linux's.
const RR_TICKS: u32 = 10;

/// What a processor's real-time tasks may have of each window of time, so
/// that one that never stops leaves the rest a twentieth: Linux's
/// `sched_rt_runtime_us` and `sched_rt_period_us`.
const RT_WINDOW_NS: u64 = 1_000_000_000;
const RT_RUNTIME_NS: u64 = 950_000_000;

/// Per processor: when its window began, how much of it its real-time tasks
/// have had, and when one of them last stopped running. Counted as a turn is
/// counted (`count_turn`).
static mut RT_USE: [(u64, u64, u64); crate::percpu::MAX_CPUS] = [(0, 0, 0); crate::percpu::MAX_CPUS];

/// Per processor: a real-time task better placed than the one it is running
/// has been made ready on it, and the call it is in is to give way before it
/// returns (`preempt_if_asked`). A task woken in an ordinary band still
/// waits for the tick, which is a known gap; a real-time one is woken to run.
static RESCHED: [crate::sync::Padded<AtomicBool>; crate::percpu::MAX_CPUS] =
    [const { crate::sync::Padded(AtomicBool::new(false)) }; crate::percpu::MAX_CPUS];

/// Each processor that is choosing what to run next — from taking a task
/// out of a queue, or deciding to hand over to one, to claiming it under
/// its record's lock (`switch_to`) — by the reaping epoch it began in
/// (`REAP_EPOCH`), or `u64::MAX` while it is not. Until the claim the task
/// is on no processor and in no queue, and its record is read and written
/// here. So a task is taken apart only once it is in no queue and every
/// processor choosing began after that (`reap_one`): none can have it in
/// hand. A processor's choosing is short, and a task waits a moment at
/// most; one not taken apart now is tried again.
static CHOOSING: [crate::sync::Padded<AtomicU64>; crate::percpu::MAX_CPUS] =
    [const { crate::sync::Padded(AtomicU64::new(u64::MAX)) }; crate::percpu::MAX_CPUS];

/// The reaping epoch: moved on by each task made ready to be taken apart.
static REAP_EPOCH: AtomicU64 = AtomicU64::new(1);

/// This processor begins choosing what to run next.
fn begin_choosing(me: usize) {
    CHOOSING[me].store(REAP_EPOCH.load(Ordering::SeqCst), Ordering::SeqCst);
}

/// This processor has chosen, and claimed what it chose or nothing.
fn end_choosing(me: usize) {
    CHOOSING[me].store(u64::MAX, Ordering::Release);
}

/// Whether every processor choosing now began after `stamp`: none can have
/// in hand a task that was in no queue by then.
fn chosen_since(stamp: u64) -> bool {
    (0..crate::percpu::count()).all(|cpu| CHOOSING[cpu].load(Ordering::SeqCst) > stamp)
}

/// Where `tid` stands to be chosen: the band it runs in and its real-time
/// priority there, 0 for none. The lower band is better, and in one band the
/// higher priority.
pub fn place_of(tid: usize) -> (u8, u8) {
    unsafe { ((*slot(tid)).as_ref().map_or(PRIO_NORMAL, |t| t.priority), st(tid).rt) }
}

/// Whether place `a` is better than place `b`.
pub fn better(a: (u8, u8), b: (u8, u8)) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 > b.1)
}

/// Whether `tid` runs at a place other than its own: lent one by somebody
/// waiting on it.
pub fn runs_lent(tid: usize) -> bool {
    unsafe {
        match *slot(tid) {
            Some(ref t) => (t.priority, st(tid).rt) != (t.base_priority, st(tid).rt_base),
            None => false,
        }
    }
}

/// Whether `waiter` coming to wait on `on` is a change to the place `on`
/// runs at, for [`refresh_priority`] to work out: its own is better, or what
/// `on` runs at is lent already and is worked out again whoever waits. A
/// call between two tasks of one band changes nothing, and is not made to
/// look at every task to find that out.
pub fn lends(waiter: usize, on: usize) -> bool {
    better(place_of(waiter), place_of(on)) || runs_lent(on)
}

/// Whether this processor's real-time tasks have had their share of the
/// window: past it, anything else in their band goes first.
///
/// # Safety
/// Interrupts off.
unsafe fn throttled() -> bool { unsafe {
    let (start, used, _) = (*core::ptr::addr_of!(RT_USE))[crate::percpu::index()];
    used >= RT_RUNTIME_NS && crate::clock::now_here().saturating_sub(start) < RT_WINDOW_NS
}}

/// `ran` nanoseconds of a real-time task's, to now, against this processor's
/// window.
///
/// A window begins when the last has run its length, and when the real-time
/// tasks come back after being away for as long as the rest are owed of one:
/// those have had their share of the window, and what comes next has a whole
/// window to itself. Begun only by time, a window was wherever earlier
/// real-time work had left it, and three seconds of FIFO met two throttles
/// or three by where that was — 3.4% for the rest in one run, 5.3% in another.
///
/// # Safety
/// Interrupts off.
unsafe fn charge_rt(ran: u64) { unsafe {
    let now = crate::clock::now_here();
    let began = now.saturating_sub(ran);
    let u = &mut (*core::ptr::addr_of_mut!(RT_USE))[crate::percpu::index()];
    if now.saturating_sub(u.0) >= RT_WINDOW_NS || began.saturating_sub(u.2) >= RT_WINDOW_NS - RT_RUNTIME_NS {
        (u.0, u.1) = (began, 0);
    }
    u.1 = u.1.saturating_add(ran);
    u.2 = now;
}}

/// Whether something waiting on this processor should run before `tid`
/// does: a task in a better band, or in its own a real-time one of a higher
/// priority while this processor's real-time tasks have time left. What a
/// reschedule now would choose over it, and so what a hand-over to it must
/// not skip. Each answered without looking at what is waiting (`runq`).
///
/// # Safety
/// Interrupts off.
unsafe fn outranked(tid: usize) -> bool { unsafe {
    let me = crate::percpu::index();
    let band = priority_of(tid);
    if runq::best_band(me).is_some_and(|b| b < band) {
        return true;
    }
    !throttled() && runq::best_rt(me, band) > st(tid).rt
}}

/// A real-time task made ready on this processor that is better placed than
/// what it is running: the call in progress gives way to it before it
/// returns.
///
/// # Safety
/// Interrupts off.
unsafe fn note_ready(tid: usize) { unsafe {
    let me = crate::percpu::current();
    if me != 0 && st(tid).rt > 0 && better(place_of(tid), place_of(me)) {
        RESCHED[crate::percpu::index()].store(true, Ordering::Relaxed);
    }
}}

/// At the end of a system call: if a real-time task better placed than the
/// caller was made ready during it, the caller gives way now — the woken
/// task runs within the call's return, not at the next tick.
///
/// The flag only says to look: it may be stale, set by an interrupt while
/// the task was in ring 3, or the task woken may have run elsewhere since.
pub fn preempt_if_asked() {
    let flags = irq_save();
    let asked = RESCHED[crate::percpu::index()].swap(false, Ordering::Relaxed);
    let me = crate::percpu::current();
    let give_way = asked && me != 0 && unsafe { outranked(me) };
    irq_restore(flags);
    if give_way {
        unsafe { schedule_inner(true) };
    }
}

/// How nice task `tid` is.
pub fn nice_of(tid: usize) -> i8 {
    let flags = irq_save();
    let n = unsafe { st(tid).nice };
    irq_restore(flags);
    n
}

/// Make task `tid` as nice as `nice`.
pub fn set_nice(tid: usize, nice: i8) {
    let flags = irq_save();
    unsafe {
        if (*slot(tid)).is_some() {
            st(tid).nice = nice.clamp(-20, 19);
        }
    }
    irq_restore(flags);
}

/// Task `tid`'s class and its real-time priority.
pub fn sched_of(tid: usize) -> (u8, u8) {
    let flags = irq_save();
    let out = unsafe { (st(tid).policy, st(tid).rt_base) };
    irq_restore(flags);
    out
}

/// Put task `tid` in class `policy`, at real-time priority `priority` (0 for
/// an ordinary one). The caller has said whether it may.
pub fn set_sched(tid: usize, policy: u8, priority: u8) {
    let flags = irq_save();
    unsafe {
        if (*slot(tid)).is_some() {
            st(tid).policy = policy;
            st(tid).rt_base = if policy == SCHED_OTHER { 0 } else { priority };
            // The priority it runs at: its own now, or a waiter's better one.
            refresh_priority(tid);
        }
    }
    irq_restore(flags);
}

/// The end of a run queue's list, and no task in a heap's links.
const END: u16 = u16::MAX;
/// In no ready queue.
const NOT_QUEUED: u8 = u8::MAX;

static INITIALIZED: AtomicBool = AtomicBool::new(false);






/// How far behind where its band has got to a task that was waiting may
/// join a queue: one turn of a program at nought, so that it is chosen next
/// rather than last (`runq::join`).
const SLEEPER_LEAD: u64 = 30_000_000;

const NO_CPU: u16 = u16::MAX;

/// There is a dead task for the kernel to take apart: one nobody will
/// collect — a thread joined through its word, a task whose creator has
/// gone — or one that could not be taken apart when somebody tried, because
/// it was still on a processor. Whoever next comes into the kernel from
/// ring 3, or has nothing to do, does it.
static REAP_WANTED: AtomicBool = AtomicBool::new(false);

/// A task ended from another processor was still on its own when it was
/// ended (`end_other`), and its parent is told — woken to collect it — by
/// whoever next finds it on none: the processor leaving it, as it chooses
/// what to run next, or failing that the next door or idle loop
/// (`announce_due`). A switch made without the one lock can be leaving it
/// already when it is ended, and has chosen by then.
static ANNOUNCE_WANTED: AtomicBool = AtomicBool::new(false);

/// Whether the door has something to say to `tid`, coming into the kernel
/// or leaving it without the one lock (`syscall_dispatch`): it was ended,
/// stopped, or barred from this processor while it ran. Read as it is: what
/// is said after the look is said at the next door — a task ended or
/// stopped from elsewhere has its processor interrupted, and comes to one.
/// A dead task to take apart or to tell of is not this task's news: the
/// next door under the one lock, or the next idle loop, sees to it — a
/// reap waiting out a processor's choosing would otherwise send every call
/// on every processor to the one lock until it was done.
pub fn door_has_news(tid: usize) -> bool {
    if tid == 0 || tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let news = unsafe {
        matches!(*slot(tid), Some(ref t) if t.state == TaskState::Dead)
            || st(tid).held
            || !may_run_on(tid, crate::percpu::index())
    };
    irq_restore(flags);
    news
}

/// Run `f` under the one lock: taken for it if this processor has it not —
/// which is right only with no other lock held, as for any taking of it —
/// and given back after. `f` runs with interrupts as the caller had them,
/// as a call under the one lock runs: it may wait, and be moved, and a
/// processor that runs it again holds the lock for it (`kl_held`).
pub fn with_kernel<R>(f: impl FnOnce() -> R) -> R {
    let flags = irq_save();
    let took = !crate::klock::held();
    if took {
        crate::klock::acquire();
    }
    irq_restore(flags);
    let out = f();
    let _ = irq_save();
    if took {
        crate::klock::release();
    }
    irq_restore(flags);
    out
}

/// Tell of every task ended from another processor that is on none now.
///
/// # Safety
/// Interrupts off, the one lock held.
unsafe fn announce_due() { unsafe {
    let mut more = false;
    for t in tids() {
        if st(t).unannounced {
            if core::ptr::read_volatile(&raw const st(t).on_cpu) == NO_CPU {
                st(t).unannounced = false;
                announce(t);
            } else {
                more = true;
            }
        }
    }
    if more {
        ANNOUNCE_WANTED.store(true, Ordering::Relaxed);
    }
}}

/// Initialize the scheduler. Creates the idle task (TID 0) which represents
/// the current execution context (kernel_main's continuation).
pub fn init() {
    unsafe {
        runq::init();
        // TID 0 = idle task (current context, its stack/context will be saved on switch)
        let idle = TaskRec::new(Task {
            tid: 0,
            state: TaskState::Running,
            context: context::CpuContext::empty(),
            kernel_stack_base: core::ptr::null_mut(), // uses boot stack
            kernel_stack_size: 0,
            priority: PRIO_IDLE,
            base_priority: PRIO_IDLE,
            cr3: crate::paging::read_cr3(),
            space: 0,
            pager_tid: 0,
            parent_tid: 0,
            mem_pages: 0,
            mem_limit: 0,
            exit_code: 0,
            fs_base: 0,
            gs_base: 0,
            clear_child_tid: 0,
            uid: 0,
            gid: 0,
            groups: [0; crate::task::MAX_GROUPS],
            ngroups: 0,
            fpu: crate::fpu::clean(),
        });
        if table().fill_at(0, idle).is_err() {
            panic!("scheduler: no memory for the idle task");
        }
        crate::fdtable::attach_new(0);
        crate::usage::task_made(0);
        // With the UID 0 bypass gone, TID 0's authority has to come from its
        // CSpace like anyone else's: every bit, and the capabilities for them.
        crate::cap::task_made(0);
        crate::cap::add_bits(0, crate::task::CAP_ALL);
    }
    INITIALIZED.store(true, Ordering::SeqCst);
}

/// Find a free slot in the task table, skipping TID 0 (the idle task).
///
/// The table slot *is* the TID, and slots are reused once `reap_dead` clears
/// them. TIDs used to come from a monotonic counter that reaping never gave
/// back, so the system could only ever create `MAX_TASKS - 1` tasks across its
/// entire uptime — after ~63 shell commands nothing could spawn again.
///
/// # Safety
/// Caller must hold interrupts off across the search and the subsequent
/// install, or another task can claim the same slot.
unsafe fn find_free_tid() -> Option<usize> { unsafe {
    table().lowest_free(1)
}}

/// Spawn a new kernel task that begins at `entry_fn`.
/// Returns the new task's TID.
pub fn spawn(entry_fn: fn()) -> usize {
    // Reserve a slot, then build the task. `Task::new` allocates a kernel
    // stack, which takes the heap lock and re-enables interrupts on release —
    // so the slot is re-checked before installing.
    loop {
        let flags = irq_save();
        let tid = match unsafe { find_free_tid() } {
            Some(t) => t,
            None => {
                irq_restore(flags);
                panic!("scheduler: too many tasks");
            }
        };
        irq_restore(flags);

        let task = Task::new(tid, entry_fn);

        let flags = irq_save();
        unsafe {
            let held = record_lock(tid).lock();
            let filled = table().fill_at(tid, TaskRec::new(task));
            drop(held);
            if let Err(rec) = filled {
                irq_restore(flags);
                let mut task = rec.task;
                task.free_stack();
                // Raced with another spawn for the slot: another one, then.
                if table().used(tid) {
                    continue;
                }
                panic!("scheduler: no memory for a task");
            }
            crate::cap::open_endpoint(tid);
            crate::cap::task_made(tid);
            crate::usage::task_made(tid);
            st(tid).process_id = crate::cap::endpoint_of(tid);
            crate::fdtable::attach_new(tid);
            enqueue(tid);
        }
        irq_restore(flags);
        return tid;
    }
}

/// Save RFLAGS and disable interrupts. Returns saved flags.
#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

/// Restore RFLAGS (re-enabling interrupts if they were enabled before).
#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Voluntary yield: whatever else in the band is ready goes first, and then
/// the task competes as before. A task that has blocked and calls this is
/// not yielding but waiting, and is in no queue.
pub fn yield_now() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    let flags = irq_save();
    unsafe {
        let me = crate::percpu::current();
        if me != 0 && matches!(*slot(me), Some(ref t) if t.state == TaskState::Running) {
            st(me).yielded = true;
        }
    }
    irq_restore(flags);
    unsafe { schedule_inner(false) };
}

/// Mark current task as Dead and reschedule. Never returns.
pub fn exit() -> ! {
    exit_with(0)
}

/// Mark current task as Dead with an exit status and reschedule. Never returns.
pub fn exit_with(code: i32) -> ! {
    unsafe {
        let current = crate::percpu::current();
        crate::serial::puts(b"[exit tid=");
        crate::serial::put_usize(current);
        crate::serial::puts(b"]\n");
        // Before anything else: clear the word this task registered and wake
        // whoever is waiting on it. musl's thread-list lock *is* that word, and
        // a dying thread holds it — so this is what publishes the thread's
        // removal and lets the next `pthread_join` proceed. It has to happen
        // while the address space is still ours to write.
        let clear_at = (*slot(current)).as_ref().map_or(0, |t| t.clear_child_tid);
        if clear_at != 0 {
            let cr3 = read_cr3_of(current);
            // The page is given back its memory first, if it has been
            // written out since the word was named; and on a page shared
            // since a fork it is this program's own word that is cleared,
            // a write that would not be allowed until the page is its own.
            let _ = crate::paging::back_range(cr3, clear_at, 4, true);
            if crate::paging::user_range_accessible(cr3, clear_at, 4, true) {
                let _ua = crate::cpu::UserAccess::begin();
                core::ptr::write_volatile(clear_at as *mut u32, 0);
                drop(_ua);
                crate::futex::futex_wake(clear_at, u64::MAX);
            }
        }

        // The descriptors go now, not at reaping. A dead task keeps its memory
        // until its parent collects it — that is deliberate, and its exit
        // status needs somewhere to live — but a descriptor is something
        // *another* task can be waiting on: the oldest idiom there is has a
        // child write down a pipe and exit while its parent reads to the end
        // and only then waits for it. Held to reaping, the pipe never reaches
        // its end and the two wait for each other for ever.
        close_descriptors(current);

        // Nothing comes between marking the task dead and switching away
        // from it. A dead task is never run again, so a tick that preempted
        // it in here left the rest undone for good: nobody was told it had
        // gone, and a parent waiting for it went on waiting. Interrupts stay
        // off until the switch, which is the last thing this task does.
        let _ = irq_save();
        st(current).unwaited = joined_by_word(current);
        let held = lock_record(current);
        if let Some(ref mut task) = *slot(current) {
            task.clear_child_tid = 0;
            task.state = TaskState::Dead;
            task.exit_code = code;
        }
        drop(held);
        if (*slot(current)).is_some() {
            crate::ipc::clear_signal_deadline(current);
            // Whoever is in a call to it is answered now, with a failure. Its
            // parent may be one of them, and a parent waiting for an answer
            // is not waiting to collect anybody.
            crate::ipc::fail_waiters(current);
            // Told now rather than at reaping: reaping waits on a parent that
            // may never call sys_wait, and whatever this task was holding
            // needs reclaiming when it stops, not when it is tidied away.
            note_death(current);
        }
        announce(current);
        schedule_inner(false);
    }
    // Should never reach here
    loop {
        core::hint::spin_loop();
    }
}

/// Called from the PIT IRQ handler to charge the running task for the tick,
/// and to preempt it once its slice is spent.
pub fn timer_tick() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let current = crate::percpu::current();
        let me = crate::percpu::index();

        // Every fourth tick, a processor with two fewer waiting than the
        // busiest takes one of them to wait here instead.
        TICKS[me] = TICKS[me].wrapping_add(1);
        if TICKS[me] % 4 == 0 {
            runq::balance(me);
        }

        // Something still waits here that a processor asleep may run: it
        // goes to wait there, and that one is woken to run it. Asleep, a
        // processor takes no tick of its own to find it by; and it is done
        // here, a tick on, rather than as the task is left waiting, because
        // a task left waiting is nearly always one a driver or a server
        // preempted for a moment — woken then, the other processor found it
        // gone, every time.
        if current != 0 {
            if let Some(cpu) = runq::hand_to_sleeper(me) {
                crate::smp::wake(cpu);
            }
        }

        // A processor with nothing to do: whatever is ready is its to run,
        // its own or another's.
        if current == 0 {
            if anything_to_run() {
                schedule_inner(true);
            }
            return;
        }

        // A task in a better band is waiting, so the running one has had its
        // turn whether or not its slice is spent. Without this a driver woken
        // by its device waits out whatever was running, and the slice that
        // makes CPU-bound work cheaper would make interrupt-driven work worse.
        let band = priority_of(current);
        if let Some(best) = best_ready_band() {
            if best < band {
                st(current).slice_left = 0;
                schedule_inner(true);
                return;
            }
        }

        // And within its band: a real-time task runs before every other, the
        // best priority first — a FIFO one until it blocks, yields or
        // something better comes, a round-robin one for its turn among its
        // equals — and one that has had its share of the window waits for
        // whatever ordinary task is ready.
        let rt = st(current).rt;
        if rt > 0 {
            // What it has run so far counts against this processor's window
            // now, and not only when it next gives the processor up: a FIFO
            // task that never does would never be counted.
            count_turn(current);
        }
        let throttled = throttled();
        if rt > 0 && !throttled {
            if runq::best_rt(me, band) > rt {
                st(current).slice_left = 0;
                schedule_inner(true);
                return;
            }
            if st(current).policy == SCHED_RR {
                if st(current).slice_left > 1 {
                    st(current).slice_left -= 1;
                    return;
                }
                if runq::best_rt(me, band) >= rt {
                    st(current).slice_left = 0;
                    schedule_inner(true);
                    return;
                }
                st(current).slice_left = RR_TICKS;
            }
            return;
        }
        let first = if rt > 0 { runq::has_ordinary(me, band) } else { !throttled && runq::best_rt(me, band) > 0 };
        if first {
            st(current).slice_left = 0;
            schedule_inner(true);
            return;
        }

        if st(current).slice_left > 1 {
            st(current).slice_left -= 1;
            return; // still has time to run
        }
        st(current).slice_left = 0;
        schedule_inner(true);
    }
}

/// Core scheduling logic.
///
/// # Safety
/// Must be called with interrupts disabled (from IRQ handler) or willing
/// to be preempted (yield_now).
unsafe fn schedule_inner(from_irq: bool) { unsafe {
    let _ = from_irq;

    // Disable interrupts during scheduling
    let flags: u64;
    core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));

    let current_tid = crate::percpu::current();
    begin_choosing(crate::percpu::index());
    // Its turn so far is counted before anything is chosen, so that what it
    // has just run counts against it.
    count_turn(current_tid);

    // Put current task back in ready queue if it's still runnable. Not the
    // idle loop: that is what runs when the queues are empty, and is in none.
    // Whether it is, is asked under its record's lock, where it is changed:
    // another processor may be ending it (`end_other`). Asked before the
    // lock and answered after it, a task ended in between was put back in a
    // queue as ready — its descriptors closed and its death told — and ran
    // on.
    if current_tid != 0 {
        let held = lock_record(current_tid);
        let state = (*slot(current_tid)).as_ref().map(|t| t.state);
        if state == Some(TaskState::Running) {
            if let Some(ref mut task) = *slot(current_tid) {
                task.state = TaskState::Ready;
            }
        }
        drop(held);
        if state == Some(TaskState::Running) {
            requeue_here(current_tid);
        } else if state == Some(TaskState::Dead) && st(current_tid).unannounced {
            // Ended from another processor, and this is the one it was
            // running on, leaving it: now its parent may be told — under the
            // one lock, which a task in a call made without it has not got,
            // and has no other lock held here.
            if !crate::klock::held() {
                crate::klock::acquire();
            }
            st(current_tid).unannounced = false;
            announce(current_tid);
        }
    }

    // Find next ready task, or have nothing to do
    let next_tid = dequeue_ready().unwrap_or(0);
    if next_tid == 0 && current_tid == 0 {
        // Already idle, restore flags and return
        end_choosing(crate::percpu::index());
        restore_flags(flags);
        return;
    }
    // A slice of its own, since this is the scheduler choosing it rather than
    // a task handing over what it had left: as long as its niceness says,
    // or a round-robin task's turn. A FIFO task's is not counted down. But
    // not for a caller its answer woke: it has what was left of the turn it
    // called in, which its answerer was running on (`unblock_task_next`).
    if st(next_tid).handed {
        st(next_tid).handed = false;
        st(next_tid).slice_left = st(next_tid).slice_left.max(1);
    } else {
        st(next_tid).slice_left = if st(next_tid).policy == SCHED_RR && st(next_tid).rt > 0 {
            RR_TICKS
        } else {
            crate::usage::slice_for(st(next_tid).nice)
        };
    }
    switch_to(current_tid, next_tid, flags);
}}

/// Switch from `current_tid` to `next_tid`.
///
/// Interrupts are already off; `flags` is what they were. Whoever calls this
/// has already decided who runs next and how long they get.
///
/// # Safety
/// Interrupts must be disabled and both tasks must exist.
unsafe fn switch_to(current_tid: usize, next_tid: usize, flags: u64) { unsafe {
    // No lock but the one lock is carried across a switch: it would be held
    // by a processor its task had left, and said to be held by whatever ran
    // there next (`sync::holding_any`).
    if crate::sync::holding_any() {
        crate::serial::puts(b"\n[KLOCK a lock held across a switch]\n");
        panic!("a lock held across a switch");
    }
    let me = crate::percpu::index();
    if next_tid == current_tid {
        // Same task, just mark running again
        let held = lock_record(current_tid);
        if let Some(ref mut task) = *slot(current_tid) {
            if task.state != TaskState::Dead {
                task.state = TaskState::Running;
            }
        }
        drop(held);
        end_choosing(me);
        restore_flags(flags);
        return;
    }

    // Its turn is over: counted, and as given up if it is waiting rather
    // than being made to make way.
    let gave_up = !matches!((*slot(current_tid)).as_ref().map(|t| t.state), Some(TaskState::Ready | TaskState::Running));
    count_turn(current_tid);
    crate::usage::switched(current_tid, gave_up);
    crate::usage::resumed(next_tid);

    // What the task being left expects of the one lock when it is switched
    // back to: what this processor holds now.
    if current_tid != 0 {
        st(current_tid).kl_held = crate::klock::held();
    } else {
        // Leaving the idle loop — from an interrupt it was woken by, as
        // often as not, and so before the loop itself can say it is awake.
        // Left saying it slept, this processor would be "woken" for the
        // next task made ready, while it ran this one, and that task would
        // wait for a tick.
        crate::percpu::woke();
        // And it has something to run, which a tick shares out. Here, while
        // the idle loop's hold on the one lock is this processor's: the
        // first processor's tick is a line of the interrupt controller,
        // which is the one lock's, and the lock may be given up below for a
        // task that does not hold it.
        start_tick();
    }
    // The task to run may still be on another processor, switching away
    // from it: its registers are not saved until that one's switch says so
    // (`context_switch`). Only the idle loop waits for that — nothing waits
    // for the idle loop, so no two processors can each be waiting for the
    // other's — and without the one lock, which the processor leaving it may
    // need to finish, answering what other processors ask meanwhile. A task
    // being left puts it back where it can be found, and the idle loop runs
    // here instead.
    let mut next_tid = next_tid;
    if next_tid != 0 && core::ptr::read_volatile(&raw const st(next_tid).on_cpu) != NO_CPU {
        if current_tid == 0 {
            if crate::klock::held() {
                crate::klock::release();
            }
            while core::ptr::read_volatile(&raw const st(next_tid).on_cpu) != NO_CPU {
                crate::smp::while_waiting();
                core::hint::spin_loop();
            }
        } else {
            let held = lock_record(next_tid);
            enqueue(next_tid);
            drop(held);
            next_tid = 0;
        }
    }
    // Claimed under its record's lock: running, and here — unless it was
    // ended meanwhile, or taken apart, when this processor's idle loop runs
    // instead. A task ended goes no further than its next door, and one
    // ended before it is claimed has none; one claimed first is ended by
    // whoever ended it as a task running elsewhere (`end_other`). The one
    // being left is said to be on no processor by the switch, once its
    // registers are saved: said any sooner, another processor could run it
    // from what was saved before.
    if next_tid != 0 {
        let held = lock_record(next_tid);
        let alive = matches!(*slot(next_tid), Some(ref t) if t.state != TaskState::Dead);
        if alive {
            if let Some(ref mut task) = *slot(next_tid) {
                task.state = TaskState::Running;
            }
            st(next_tid).on_cpu = me as u16;
            st(next_tid).last_cpu = me as u16;
        }
        drop(held);
        if !alive {
            next_tid = 0;
        }
    }
    end_choosing(me);
    if next_tid == current_tid {
        // The idle loop, which chose a task that was ended: it goes on.
        restore_flags(flags);
        return;
    }

    // And the one lock as the task switched to expects it: held for one
    // switched out holding it, and for the idle loop; not for one that was
    // not, which would never give it back.
    let wants = next_tid == 0 || st(next_tid).kl_held;
    if wants && !crate::klock::held() {
        crate::klock::acquire();
    } else if !wants && crate::klock::held() {
        crate::klock::release();
    }

    if next_tid != 0 {
        // What it runs here is counted as this processor counts it: one
        // handed a call, or taken from another, comes from another's.
        runq::moved(next_tid, me, priority_of(next_tid));
    }
    if current_tid != 0 {
        // Said to be on no processor by the switch itself, once it is saved.
        // When it left: a processor that takes it soon after takes it warm
        // from this one's cache (`runq::pull`).
        st(current_tid).ran_at = crate::clock::now_here();
    }
    crate::percpu::set_current(next_tid);

    // Switch CR3 if address spaces differ. From what the register holds,
    // not from the task being left: a task that has just become another
    // program, or one that has not been to ring 3 yet, is not in the
    // address space its record names.
    let new_cr3 = (*slot(next_tid)).as_ref().unwrap().cr3;
    if new_cr3 != 0 && new_cr3 != crate::paging::read_cr3() {
        crate::paging::write_cr3(new_cr3);
    }

    // Update kernel RSP for syscall re-entry and TSS RSP0 for exceptions
    let new_task = (*slot(next_tid)).as_ref().unwrap();
    if !new_task.kernel_stack_base.is_null() {
        let kernel_stack_top =
            new_task.kernel_stack_base as u64 + new_task.kernel_stack_size as u64;
        crate::percpu::set_kernel_stack(kernel_stack_top);
    }

    // Threads share an address space, so FS is what tells one thread's
    // thread-locals from another's. Written unconditionally: comparing against
    // the outgoing value first would need per-CPU state for no measurable gain
    // at this scheduler's switch rate. Where FSGSBASE is on a program changes
    // its FS and GS bases without a call, so the outgoing task's are read back
    // from the processor first — its GS base is where `swapgs` left it.
    if crate::cpu::fsgsbase() && current_tid != 0 {
        if let Some(ref mut t) = *slot(current_tid) {
            t.fs_base = crate::cpu::fs_base();
            t.gs_base = crate::cpu::user_gs_base();
        }
    }
    let new_task = (*slot(next_tid)).as_ref().unwrap();
    crate::cpu::set_fs_base(new_task.fs_base);
    crate::cpu::set_user_gs_base(new_task.gs_base);

    // The floating-point and SSE registers, which are nobody's until this says
    // whose. Saved from the outgoing task and loaded for the incoming one
    // *before* the switch rather than after it: the kernel is soft-float and
    // never touches them, so loading early is safe, and it means a task that
    // has never run starts from its own clean state without its entry
    // trampoline having to know anything about this.
    //
    // The idle loop has none to save or load: it is the kernel, which never
    // touches them. What the last task left stays in the registers while a
    // processor idles — saved already — and is replaced before anything in
    // ring 3 runs again.
    if current_tid != 0 {
        crate::fpu::save(&raw mut (*slot(current_tid)).as_mut().unwrap().fpu);
    }
    if next_tid != 0 {
        crate::fpu::restore(&raw const (*slot(next_tid)).as_ref().unwrap().fpu);
    }

    // Get raw pointers to contexts. The idle loop's is this processor's.
    let old_ctx = context_of(current_tid);
    let new_ctx = context_of(next_tid) as *const context::CpuContext;

    // Do NOT restore interrupts here — context_switch restores RFLAGS from
    // the new context, which atomically re-enables interrupts with the switch.
    // Enabling interrupts before context_switch creates a race where a nested
    // timer interrupt can re-enter schedule_inner with stale old_ctx/new_ctx.

    // Perform the context switch (restores RFLAGS from new context), which
    // says the task left is on no processor once it has saved it.
    let left = if current_tid != 0 { &raw mut st(current_tid).on_cpu } else { core::ptr::null_mut() };
    context::context_switch(old_ctx, new_ctx, left);
}}

/// Where a task's registers are kept while it is not running: in the task,
/// or for the idle loop in the processor whose idle loop it is.
///
/// # Safety
/// Interrupts off, and the task exists.
unsafe fn context_of(tid: usize) -> *mut context::CpuContext { unsafe {
    if tid == 0 {
        crate::percpu::idle_context()
    } else {
        &raw mut (*slot(tid)).as_mut().unwrap().context
    }
}}

/// Move a task into a scheduling band.
///
/// Takes effect from its next turn: a task already running keeps the slice it
/// is on, which is at most a few ticks and saves reasoning about a queue entry
/// filed under the band it used to be in.
pub fn set_priority(tid: usize, band: u8) -> Result<(), ()> {
    if tid >= MAX_TASKS || band as usize >= NUM_PRIORITIES {
        return Err(());
    }
    unsafe {
        match *slot(tid) {
            Some(ref mut task) => {
                task.base_priority = band;
                Ok(())
            }
            None => Err(()),
        }
    }
    .map(|()| refresh_priority(tid))
}

/// Work out what band `tid` should actually run in, and follow the chain if it
/// changes.
///
/// A task runs in the better of its own band and the band of anything blocked
/// waiting on it. Without that, bands introduce the problem they are famous
/// for: a server in an ordinary band, called by something in a better one, is
/// preempted by any middling task that comes along — and the caller, which
/// outranks that task, waits behind it. The work is being done on the caller's
/// behalf, so it should be done at the caller's urgency. Waiting on it is
/// calling it, or waiting to lock a priority-inheriting word it holds
/// (`futex::lock_pi`): the same problem, between threads.
///
/// Called whenever the set of tasks waiting on `tid` changes, with
/// interrupts off and no task's record held: it takes each it changes.
pub fn refresh_priority(tid: usize) {
    let mut cur = tid;
    // Chains of one server calling another are short. One of holders of
    // priority-inheriting words, each waiting for the next, is lent along as
    // far as `futex::lock_pi` searches one for a cycle (`futex::PI_DEPTH`).
    // The bound is so that a cycle — which should not exist, and would mean
    // a deadlock if it did — cannot turn into a hang here.
    for _ in 0..crate::futex::PI_DEPTH {
        if cur >= MAX_TASKS {
            return;
        }
        unsafe {
            // Its place is changed with its record's lock held; its
            // waiters' are read as they are.
            let _held = lock_record(cur);
            // Its own place, or the better of a waiter's: the band, and the
            // real-time priority in it.
            let base = match *slot(cur) {
                Some(ref t) => (t.base_priority, st(cur).rt_base),
                None => return,
            };
            let mut best = base;
            for t in tids() {
                if waits_on(t) == Some(cur) && (*slot(t)).is_some() {
                    let waiter = place_of(t);
                    if better(waiter, best) {
                        best = waiter;
                    }
                }
            }
            if place_of(cur) == best {
                return; // unchanged, so nothing downstream changes either
            }
            // A task waiting in a run queue is where its place put it: taken
            // out first, and put back by its new one, on the same processor,
            // in one step.
            runq::replace(cur, || {
                if let Some(ref mut t) = *slot(cur) {
                    t.priority = best.0;
                }
                st(cur).rt = best.1;
            });
        }
        // Whatever `cur` is itself waiting on inherits this too.
        match waits_on(cur) {
            Some(next) => cur = next,
            None => return,
        }
    }
}

/// The task `t` is waiting on, for as long as it waits: the server it calls,
/// or the holder of the priority-inheriting word it waits to lock. That one
/// runs at `t`'s place where it is better than its own.
fn waits_on(t: usize) -> Option<usize> {
    crate::ipc::blocked_on(t).or_else(|| crate::futex::pi_waits_on(t))
}

/// Make a blocked task runnable without putting it in the ready queue.
///
/// For the task that is about to be switched to directly. Queueing it as well
/// would leave an entry behind for a task that is already running. Until that
/// switch it is runnable and in no queue, so interrupts must stay off from
/// here to [`donate_to`].
pub fn make_ready(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let _held = lock_record(tid);
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
            }
        }
    }
}

/// Give the rest of this task's slice to `tid` and switch to it now.
///
/// The caller has already blocked itself on `tid` — it is waiting for a reply
/// and has nothing to contribute until it arrives, so a synchronous call is
/// one line of control moving between address spaces rather than two tasks
/// taking turns. Running the callee on the caller's own slice is what makes
/// that true of the accounting as well: a server does not earn a fresh
/// quantum every time somebody calls it, which for a chain of servers polled
/// a hundred times a second is most of a CPU handed out for free.
///
/// Falls back to ordinary scheduling if the target cannot take over.
///
/// Interrupts must already be off, and `flags` is what they were before. The
/// caller turned them off to block itself and to [`make_ready`] the target,
/// and they stay off until the switch: in between, the target is runnable but
/// in no queue. A tick there used to preempt the caller -- already blocked --
/// and run something else, and nothing ever ran the target again. When the
/// caller's deadline woke it, it resumed straight into this switch and was
/// left marked running, in no queue either.
pub fn donate_to(tid: usize, flags: u64) {
    unsafe {
        if !INITIALIZED.load(Ordering::SeqCst) {
            restore_flags(flags);
            return;
        }
        let current_tid = crate::percpu::current();
        // Choosing from here to the claim (`switch_to`): its record is not
        // taken apart meanwhile, and is looked at under its lock.
        let me = crate::percpu::index();
        begin_choosing(me);
        let held = if tid < MAX_TASKS { lock_record(tid) } else { None };

        // A task of a stopped program is not handed the processor: it is
        // ready, in no queue, and stays so until the program is continued.
        let mut takeable = tid < MAX_TASKS
            && tid != current_tid
            && !st(tid).held
            && (*slot(tid)).as_ref().map(|t| t.state == TaskState::Ready).unwrap_or(false);
        // Handing the CPU straight to the callee skips the scheduler, so it
        // must not be used to run a worse band ahead of a better one, or an
        // ordinary task ahead of a real-time one. When something better is
        // waiting, go through the queue instead — the callee is ready and
        // will be picked in its turn.
        if takeable && (outranked(tid) || !may_run_on(tid, me)) {
            enqueue(tid);
            takeable = false;
        }
        if takeable {
            // At least one tick, so a caller whose slice was already spent
            // still makes progress rather than handing over a turn that
            // ends at once.
            st(tid).slice_left = st(current_tid).slice_left.max(1);
            st(current_tid).slice_left = 0;
        }
        drop(held);
        if !takeable {
            end_choosing(me);
            restore_flags(flags);
            yield_now();
            return;
        }
        switch_to(current_tid, tid, flags);
    }
}

/// Which band a task is in. Anything out of range is treated as ordinary.
/// The band `tid` was put in, whatever it is running in for now.
pub fn base_priority_of(tid: usize) -> Option<u8> {
    if tid >= MAX_TASKS {
        return None;
    }
    unsafe { (*slot(tid)).as_ref().filter(|t| t.state != TaskState::Dead).map(|t| t.base_priority) }
}

pub fn priority_of(tid: usize) -> usize {
    if tid >= MAX_TASKS {
        return PRIO_NORMAL as usize;
    }
    unsafe {
        (*slot(tid))
            .as_ref()
            .map(|t| (t.priority as usize).min(NUM_PRIORITIES - 1))
            .unwrap_or(PRIO_NORMAL as usize)
    }
}

/// The best band with a task waiting in this processor's queues, if any.
///
/// # Safety
/// Interrupts must be off.
unsafe fn best_ready_band() -> Option<usize> { unsafe {
    runq::best_band(crate::percpu::index())
}}

/// Whether this processor has anything to run: of its own, or waiting on
/// another for it to take (`runq::pull`).
///
/// # Safety
/// Interrupts must be off.
unsafe fn anything_to_run() -> bool { unsafe {
    let me = crate::percpu::index();
    runq::best_band(me).is_some() || runq::takeable(me)
}}

/// Take `tid` out of the run queue it is in, if it is in one.
///
/// # Safety
/// Interrupts off.
unsafe fn unlink_ready(tid: usize) { unsafe {
    runq::unlink(tid);
}}

/// What this processor runs next: the best of its own queues, best band
/// first; or, with nothing of its own, the best of the busiest processor's.
unsafe fn dequeue_ready() -> Option<usize> { unsafe {
    let me = crate::percpu::index();
    let throttled = throttled();
    for p in 0..NUM_PRIORITIES {
        if let Some(tid) = runq::take(me, p, throttled) {
            return Some(tid);
        }
    }
    runq::pull(me, throttled)
}}

/// Count the turn `tid` is having on this processor, to now: in what it has
/// used, and in how far it has run as its band sees it.
///
/// # Safety
/// Interrupts off, and `tid` is what this processor is running.
unsafe fn count_turn(tid: usize) { unsafe {
    let ran = crate::usage::charge(tid);
    if tid != 0 && ran != 0 {
        // A real-time turn is counted against this processor's window, and
        // not in how far the task has run as its band sees it: counted
        // there, three seconds of FIFO put the band's floor three seconds
        // ahead, and every ordinary task that woke joined that far behind
        // those that had not slept. Leaving the class, it joins where the
        // band has got to, as any task that has been away does.
        if st(tid).rt > 0 {
            charge_rt(ran);
        } else {
            // What it has run is what a queue orders it by, and it can be
            // in one while it runs: put back by the scheduler before the
            // switch away counts the last of its turn, or made ready by an
            // interrupt after it blocked and before it gave the processor
            // up. Changed in a heap, it would be out of its order there;
            // so out while it changes, and back where it was, in one step.
            runq::recount(tid, || st(tid).vrun = st(tid).vrun.saturating_add(crate::usage::weighted(ran, st(tid).nice)));
            // Where its band has got to here, now that it has run further.
            runq::settle(crate::percpu::index(), priority_of(tid), tid);
        }
    }
}}

/// Put a task made ready in a run queue: on the processor `runq::place_for`
/// says, waking it if it is asleep. Not a held one: that is queued when its
/// program is continued. And never the idle loop.
///
/// One already waiting in its band stays where it is; one waiting in
/// another, which it has since left, goes to this one.
unsafe fn enqueue(tid: usize) { unsafe {
    if tid == 0 || st(tid).held {
        return;
    }
    let p = priority_of(tid);
    if st(tid).queued == p as u8 {
        return;
    }
    unlink_ready(tid);
    let cpu = runq::place_for(tid);
    runq::moved(tid, cpu, p);
    runq::join(tid, cpu, p);
    runq::link(tid, cpu, p, false);
    note_ready(tid);
    // Another processor: woken if it sleeps, and interrupted if what it
    // runs is worse — it runs this now, not at its tick, which was half a
    // tick on average for a real-time thread woken while every processor
    // computed. This one gives way at the end of the call (`note_ready`).
    if cpu != crate::percpu::index() {
        if crate::percpu::napping(cpu) {
            crate::smp::wake(cpu);
        } else if better(place_of(tid), place_of(crate::percpu::current_of(cpu))) {
            crate::smp::interrupt(cpu);
        }
    }
}}

/// Put the task this processor was running back in its queue here: its
/// turn is over, or something better has come. As far as it has run: it
/// has not been away. Passed through the floor a task that slept joins
/// behind, a task at nought whose turn ended alongside one at nice 10 lost
/// its lead each time the nicer ran — the floor is where the nicer had got
/// to — and had three times its share where it is owed nine.
unsafe fn requeue_here(tid: usize) { unsafe {
    if tid == 0 || st(tid).held {
        return;
    }
    // Not here, if it may not run here: where it may.
    if !may_run_on(tid, crate::percpu::index()) {
        unlink_ready(tid);
        enqueue(tid);
        return;
    }
    let p = priority_of(tid);
    unlink_ready(tid);
    // In the band it ran in, unless its place changed meanwhile.
    runq::moved(tid, crate::percpu::index(), p);
    runq::link(tid, crate::percpu::index(), p, false);
}}

/// Put a task at the *front* of this processor's queue, so it runs next:
/// the caller of whoever is replying here.
unsafe fn enqueue_front(tid: usize) { unsafe {
    if tid == 0 || st(tid).held {
        return;
    }
    if !may_run_on(tid, crate::percpu::index()) {
        enqueue(tid);
        return;
    }
    let p = priority_of(tid);
    unlink_ready(tid);
    let me = crate::percpu::index();
    runq::moved(tid, me, p);
    runq::join(tid, me, p);
    runq::link(tid, me, p, true);
    note_ready(tid);
}}

/// Take a task out of the run queue it is in.
///
/// # Safety
/// Interrupts must be off.
unsafe fn unqueue(tid: usize) { unsafe {
    unlink_ready(tid);
}}

/// Hold a task: it does not run again until [`release_task`]. For a program
/// being stopped, by `job::stop`, which holds every task of it.
///
/// The running task can be held too. It goes on until it next gives up the
/// processor — [`stop_here`] — and is not given it back.
pub fn hold_task(tid: usize) {
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        st(tid).held = true;
        unqueue(tid);
        // Running on another processor, it goes on until that processor is
        // brought into the kernel to look.
        interrupt_if_elsewhere(tid);
    }
    irq_restore(flags);
}

/// If `tid` is running on a processor other than this one, interrupt that
/// processor: the task has been ended or stopped, and in ring 3 it does
/// not know.
///
/// # Safety
/// Interrupts off.
unsafe fn interrupt_if_elsewhere(tid: usize) { unsafe {
    let cpu = st(tid).on_cpu;
    if cpu != NO_CPU && cpu as usize != crate::percpu::index() {
        crate::smp::interrupt(cpu as usize);
    }
}}

/// Whether `tid` is running on a processor other than this one.
///
/// # Safety
/// Interrupts off.
unsafe fn runs_elsewhere(tid: usize) -> bool { unsafe {
    st(tid).on_cpu != NO_CPU && st(tid).on_cpu as usize != crate::percpu::index()
}}

/// A task has come into the kernel from ring 3, or is about to go back
/// there, and the kernel lock is held: look at what another processor may
/// have done to it while it ran.
///
/// - **Ended**: it does not go on. This processor leaves it, and whoever
///   was waiting to collect it is told then.
/// - **Stopped**: it waits here until its program is continued.
///
/// Called at each of the kernel's three doors — a system call, a fault and
/// an interrupt taken in ring 3. With one processor neither can have
/// happened: a task that is running is the one doing things.
///
/// And if there is a dead task the kernel is to take apart, this is as
/// good a moment as the idle loop — and one that comes whether or not the
/// machine ever has nothing to do.
pub fn arrived() {
    // Looked at before it is taken: this is on the way into every system
    // call, and taking it is a write every processor would wait its turn at.
    if REAP_WANTED.load(Ordering::Relaxed) && REAP_WANTED.swap(false, Ordering::Relaxed) {
        reap_dead();
    }
    if ANNOUNCE_WANTED.load(Ordering::Relaxed) && ANNOUNCE_WANTED.swap(false, Ordering::Relaxed) {
        let flags = irq_save();
        unsafe { announce_due() };
        irq_restore(flags);
    }
    let me = crate::percpu::current();
    if me == 0 {
        return;
    }
    let flags = irq_save();
    let (dead, held) = unsafe {
        (matches!(*slot(me), Some(ref t) if t.state == TaskState::Dead), st(me).held)
    };
    if dead {
        unsafe { schedule_inner(false) };
        // A dead task is never switched back to.
        loop {
            core::hint::spin_loop();
        }
    }
    let barred = unsafe { !may_run_on(me, crate::percpu::index()) };
    irq_restore(flags);
    if held {
        stop_here();
    }
    // Told it may not run here while it ran: off, to one it may.
    if barred {
        yield_now();
    }
}

/// Another processor has interrupted this one to have it look at what it
/// is doing. For a task in ring 3 the looking is [`arrived`], on the way
/// back there. For a processor with nothing to do, it is this: something
/// has been made ready. And for one running something worse than a task
/// just put to wait here (`enqueue`), it is the switch to that task.
pub fn kicked() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let current = crate::percpu::current();
        if current == 0 {
            if anything_to_run() {
                schedule_inner(true);
            }
        } else if outranked(current) {
            // Something better has been put to wait here: it runs now.
            st(current).slice_left = 0;
            schedule_inner(true);
        }
    }
}

/// The clock has woken what was due, on this processor and between two
/// ticks: if what is now ready is better than what is running, it runs now,
/// as it would have at the next tick — which is the wait it was woken on
/// time to be spared. Better is a better band, or in the same band a
/// real-time task of a higher priority ([`outranked`]); an ordinary task of
/// the running task's own band waits for the turn to end, as at a tick.
pub fn woken() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe {
        let current = crate::percpu::current();
        if best_ready_band().is_none() {
            return;
        }
        if current == 0 {
            schedule_inner(true);
        } else if outranked(current) {
            st(current).slice_left = 0;
            schedule_inner(true);
        }
    }
}

/// Let a held task run again: at once if it is ready to, and otherwise when
/// whatever it is blocked on lets it.
pub fn release_task(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        if st(tid).held {
            st(tid).held = false;
            // Ready, and not running: one that was stopped from another
            // processor and has not been reached yet is still running, and
            // simply goes on.
            if matches!(*slot(tid), Some(ref t) if t.state == TaskState::Ready) && st(tid).on_cpu == NO_CPU {
                enqueue(tid);
            }
        }
    }
    irq_restore(flags);
}

/// Whether a task is held.
pub fn is_held(tid: usize) -> bool {
    tid < MAX_TASKS && unsafe { st(tid).held }
}

/// If the running task's program has just been stopped, stop: give up the
/// processor, and come back when the program is continued.
///
/// For whoever raised the signal that did it, once it has nothing left to
/// finish. A task is held where it is, and for the one that is running,
/// here is where that is.
pub fn stop_here() {
    while is_held(current_tid()) {
        yield_now();
    }
}

/// Whether `tid` is a task nobody waits for: a thread — made by a task of
/// its own program — with a word to be cleared when it ends
/// (`SYS_SET_CLEAR_TID`), which its creator gives it before starting it.
///
/// That word is how such a thread is joined. A C library registers one for
/// every thread it makes and waits on the word, never on the task; what it
/// waits for with a *wait* are its child processes, and a thread among them
/// is a child it did not make: `waitpid(-1)` answered with a thread that
/// had ended, and with "none has ended yet" for a program whose only
/// children were its own threads, where the answer is that it has none.
/// A thread with no such word is waited for like any child — that is how
/// this system's own runtime joins one.
///
/// # Safety
/// Interrupts must be off.
unsafe fn joined_by_word(tid: usize) -> bool { unsafe {
    if st(tid).unwaited {
        return true;
    }
    let Some(ref t) = *slot(tid) else { return false };
    let parent = t.parent_tid;
    t.clear_child_tid != 0
        && t.space != 0
        && parent != 0
        && (*slot(parent)).as_ref().is_some_and(|p| p.space == t.space)
}}

/// `tid` has been given a word to clear. If that makes it a thread nobody
/// waits for, and its creator is waiting — for it, or for any child — the
/// creator looks again, and may find it has no children at all.
///
/// A thread's creator gives the word before the thread is started, and
/// then there is nobody waiting. But the call can be made by a thread for
/// itself, later, and a wait that had counted it as a child would go on
/// waiting for a task no wait is ever given.
pub fn word_given(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        if joined_by_word(tid) {
            let parent = (*slot(tid)).as_ref().map_or(0, |t| t.parent_tid);
            if parent != 0 && waits_for(parent, tid) {
                st(parent).wait_blocked = false;
                st(parent).wait_result = 0;
                st(parent).wait_again = true;
                unblock_task(parent);
            }
        }
    }
    irq_restore(flags);
}

/// Is `parent` blocked in a wait that `child` is one of the children of?
///
/// # Safety
/// Interrupts must be off.
unsafe fn waits_for(parent: usize, child: usize) -> bool { unsafe {
    st(parent).wait_blocked
        && match st(parent).wait_group {
            0 => st(parent).wait_target == 0 || st(parent).wait_target == child,
            group => crate::job::pgid_of(child) == group,
        }
}}

/// Task `child`'s program has stopped or been continued (`kind` is one of
/// `job::HAS_STOPPED`, `job::HAS_CONTINUED`): its parent hears SIGCHLD, as
/// it does when a child ends, and a parent waiting to hear of exactly this
/// is woken to look.
///
/// Nothing for a task whose parent is in its own program — a thread.
pub fn child_changed(child: usize, kind: u8) {
    if child >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let (parent, space) = match *slot(child) {
            Some(ref t) => (t.parent_tid, t.space),
            None => (0, 0),
        };
        let theirs = if parent == 0 { 0 } else { (*slot(parent)).as_ref().map_or(0, |p| p.space) };
        if parent != 0 && !(theirs != 0 && theirs == space) {
            if waits_for(parent, child) && st(parent).wait_reports & kind != 0 {
                st(parent).wait_blocked = false;
                st(parent).wait_result = 0;
                st(parent).wait_again = true;
                unblock_task(parent);
            }
            let info = if kind == crate::job::HAS_STOPPED {
                crate::signal::child_info(child, crate::signal::CLD_STOPPED, crate::job::stopped_by(child) as u64)
            } else {
                crate::signal::child_info(child, crate::signal::CLD_CONTINUED, crate::signal::SIGCONT as u64)
            };
            crate::signal::child_ended(parent, info);
        }
    }
    irq_restore(flags);
}

/// Restore interrupt flag from saved RFLAGS.
unsafe fn restore_flags(flags: u64) { unsafe {
    if flags & (1 << 9) != 0 {
        core::arch::asm!("sti", options(nostack, nomem));
    }
}}

/// Get the current task's TID.
pub fn current_tid() -> usize {
    crate::percpu::current()
}

/// Mark a task as blocked. Used by IPC.
pub fn block_task(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    // Not over a death: a task ended while it was on its way to waiting
    // stays ended.
    let _held = lock_record(tid);
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            if task.state != TaskState::Dead {
                task.state = TaskState::Blocked;
            }
        }
    }
}

/// Unblock a task and put it back in the ready queue. Used by IPC.
///
/// And wake a processor that is asleep, if one is, to run it: whoever woke
/// it is going on with what it was doing.
pub fn unblock_task(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let _held = lock_record(tid);
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
                enqueue(tid);
            }
        }
    }
}

/// Unblock a task and run it *next*, ahead of everything already waiting.
///
/// For the two halves of a synchronous IPC: the caller has just blocked on the
/// callee, and the reply is the only thing that will wake it again. Queueing
/// the callee behind everything else makes a round trip cost a lap of the
/// whole table — measured at 84 milliseconds for one poll of the input server
/// through the keyboard driver, which is what "typing is slow" turned out to
/// mean. The caller has stopped to wait, so this is its remaining time being
/// handed to the task it is waiting for, not a queue-jump for free.
///
/// Fairness is unaffected: the woken task runs on what is left of the turn
/// it called in — which its answerer ran on (`donate_to`) — and yields at
/// its end, and a task that never blocks is never overtaken by this. Given
/// a whole turn each time, as it was, a pair of tasks calling each other
/// never came to the end of one, and nothing else of their band ran on
/// their processor while they called.
///
/// No processor is woken for it, where [`unblock_task`] wakes one. Whoever
/// answers a call is about to wait for the next, and the caller runs here
/// in its place; waking another processor for it would send the two of
/// them back and forth between processors, an interrupt each way for every
/// call. If the answerer does not wait after all, the caller is found by
/// the next processor to look — this one when it next reschedules, or any
/// that is idle, on its tick.
pub fn unblock_task_next(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let _held = lock_record(tid);
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
                let answerer = crate::percpu::current();
                if answerer != 0 {
                    st(tid).slice_left = st(answerer).slice_left;
                    st(tid).handed = true;
                }
                enqueue_front(tid);
            }
        }
    }
}

/// Read a dead child's exit status. Interrupts must be off.
unsafe fn child_exit_code(tid: usize) -> i32 { unsafe {
    (*slot(tid)).as_ref().map(|t| t.exit_code).unwrap_or(0)
}}

/// Block the current task until a child exits.
///
/// Returns the dead child's TID in bits [31:0] and its exit status in bits
/// [63:32], or u64::MAX if the caller has no children. The status used to be
/// dropped entirely, so `process::exit(1)` was indistinguishable from success.
pub fn sys_wait() -> u64 {
    sys_wait_for(0, Wait { no_wait: false, by_pid: false, group: false, reports: 0 })
}

/// A signal has arrived for `tid`, which may be waiting for a child: if it
/// is, it goes round and looks again — at its children first, and then at
/// what arrived. True if it was waiting.
pub fn interrupt_wait(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let waiting = unsafe {
        let blocked = matches!(*slot(tid), Some(ref t) if t.state == TaskState::Blocked);
        if st(tid).wait_blocked && blocked && st(tid).wait_result == 0 {
            st(tid).wait_again = true;
            unblock_task(tid);
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    waiting
}

/// The process id of the program `tid` belongs to, or 0 if there is no such
/// task.
pub fn pid_of(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    let flags = irq_save();
    let pid = unsafe { st(tid).process_id };
    irq_restore(flags);
    pid
}

/// A task that has just been started in its creator's address space is a
/// thread of the creator's program, and is in that process.
pub fn join_process(tid: usize, of: usize) {
    if tid >= MAX_TASKS || of >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe { st(tid).process_id = st(of).process_id };
    irq_restore(flags);
    // And in its group and its session, and stopped if it is.
    crate::job::joined(tid, of);
}

/// A task of the process `pid`: one that is running if there is one, and
/// else one that has ended and not been collected. `None` if the id names
/// nothing — which, since an id is never used twice, means it has gone.
pub fn task_of_pid(pid: u64) -> Option<usize> {
    if pid == 0 {
        return None;
    }
    let flags = irq_save();
    let found = unsafe {
        let is = |want_dead: bool| {
            tids().filter(|&i| i >= 2).find(|&i| {
                st(i).process_id == pid
                    && matches!(*slot(i), Some(ref t) if (t.state == TaskState::Dead) == want_dead)
            })
        };
        is(false).or_else(|| is(true))
    };
    irq_restore(flags);
    found
}

/// How a wait for a child is to be made: `SYS_WAIT_FOR`'s flags.
#[derive(Clone, Copy)]
pub struct Wait {
    /// Answer 0 rather than wait.
    pub no_wait: bool,
    /// The child is named, there and back, by its process id — the number
    /// that is never used twice — rather than by its task id, which is.
    pub by_pid: bool,
    /// What is named is a process group of children: 0 for the caller's own.
    pub group: bool,
    /// Besides a child ending, what to hear of: `job::HAS_STOPPED` and
    /// `job::HAS_CONTINUED`.
    pub reports: u8,
}

/// In the answer to a wait, beside the child's name: this is a child that
/// has stopped or been continued, not one that has ended, and the status
/// above it is the signal that stopped it, or 0 for a continue.
pub const WAIT_REPORT: u64 = 1 << 31;

/// Collect a child that has ended: `target`, or whichever is first if that
/// is 0. Returns its id with its status above it.
///
/// With `no_wait`, a child that has not ended yet is answered with 0 rather
/// than waited for. `u64::MAX` means there is nothing to wait for: no
/// children, or `target` is not one.
///
/// Waiting for one child in particular is not a loop around waiting for any:
/// that would collect, and so lose, every child that finished first. A shell
/// with a pipeline has several, and wants each one's status.
///
/// A shell with jobs wants to hear of more than endings. With `reports`, a
/// child that has stopped, or been continued, is answered too — marked
/// [`WAIT_REPORT`], and not collected: it is still there.
pub fn sys_wait_for(target: u64, how: Wait) -> u64 {
    let parent = current_tid();
    loop {
        let flags = irq_save();
        unsafe {
            // The children meant: one, as a task; a group of them; or all.
            let (target, group) = if how.group {
                (0, if target == 0 { crate::job::pgid_of(parent) } else { target })
            } else if target == 0 {
                (0, 0)
            } else if how.by_pid {
                // Not a task nobody waits for: a program's first task, ended
                // by an exec from another thread, still has the program's id
                // until it is taken apart, and the task that took its place
                // is the one to wait for.
                let child = tids().filter(|&i| i >= 1).find(|&i| {
                    st(i).process_id == target
                        && (*slot(i)).as_ref().is_some_and(|t| t.parent_tid == parent)
                        && !joined_by_word(i)
                });
                match child {
                    Some(i) => (i, 0),
                    None => {
                        irq_restore(flags);
                        return u64::MAX;
                    }
                }
            } else if target < MAX_TASKS as u64 {
                (target as usize, 0)
            } else {
                irq_restore(flags);
                return u64::MAX;
            };
            // What the child is called in the answer. Asked before it is reaped:
            // afterwards it has no name.
            let name = |i: usize| if how.by_pid { st(i).process_id } else { i as u64 };
            let wanted = |i: usize| {
                if group != 0 { crate::job::pgid_of(i) == group } else { target == 0 || target == i }
            };
            // Not a thread that is joined through its word: that is no
            // child of anybody's.
            let mine = |i: usize| {
                wanted(i) && (*slot(i)).as_ref().is_some_and(|t| t.parent_tid == parent) && !joined_by_word(i)
            };
            // Check if a child is already dead (zombie) and not yet reaped.
            // Not one that was ended from another processor and is still
            // running there: it is collected when that processor has left
            // it, and the wait below is woken then.
            for i in tids().filter(|&i| i >= 1) {
                let collectable = mine(i)
                    && !st(i).reaped
                    && !st(i).unannounced
                    && matches!(*slot(i), Some(ref t) if t.state == TaskState::Dead);
                if collectable {
                    st(i).reaped = true;
                    let code = child_exit_code(i);
                    let child = name(i);
                    // What it used is what its parent's children used.
                    crate::usage::collected(parent, i);
                    reap(i);
                    irq_restore(flags);
                    return (child & 0x7FFF_FFFF) | ((code as u32 as u64) << 32);
                }
            }
            // Or one that has stopped or started, if that was asked for.
            if how.reports != 0 {
                for i in tids().filter(|&i| i >= 1) {
                    let alive = mine(i) && matches!(*slot(i), Some(ref t) if t.state != TaskState::Dead);
                    if !alive {
                        continue;
                    }
                    if let Some(signo) = crate::job::take_report(i, how.reports) {
                        let child = name(i);
                        irq_restore(flags);
                        return (child & 0x7FFF_FFFF) | WAIT_REPORT | (signo as u64) << 32;
                    }
                }
            }

            // Check if we have any living children of the kind asked for at all
            if !tids().filter(|&i| i >= 1).any(mine) {
                irq_restore(flags);
                return u64::MAX;
            }
            if how.no_wait {
                irq_restore(flags);
                return 0;
            }
            // A signal with a handler for the kernel to run ends the wait,
            // or stops it beginning: asked here, in the same step as the
            // marking below, so that one raised a moment later finds this
            // parked and ends it (`interrupt_wait`).
            if crate::signal::ends_wait(parent) {
                irq_restore(flags);
                return crate::signal::INTERRUPTED;
            }

            // Block until a child exits. Marking and blocking must both happen
            // before interrupts come back on, or exit() can slip in between them.
            st(parent).wait_blocked = true;
            st(parent).wait_target = target;
            st(parent).wait_group = group;
            st(parent).wait_reports = how.reports;
            st(parent).wait_again = false;
            st(parent).wait_result = 0;
            st(parent).wait_code = 0;
            block_task(parent);
            irq_restore(flags);
            yield_now();

            // Woken up — WAIT_RESULT has the dead child's TID
            let flags = irq_save();
            let child_tid = st(parent).wait_result;
            let again = core::mem::replace(&mut st(parent).wait_again, false);
            st(parent).wait_blocked = false;
            st(parent).wait_result = 0;
            st(parent).wait_target = 0;
            st(parent).wait_group = 0;
            st(parent).wait_reports = 0;
            if child_tid != 0 {
                let code = st(parent).wait_code;
                let child = if how.by_pid { st(child_tid).process_id } else { child_tid as u64 };
                // Reaped now rather than whenever the machine next goes idle.
                // A dead task keeps all its memory until it is reaped, and a
                // parent running programs one after another never lets the
                // machine idle: a test suite held every program it had run.
                crate::usage::collected(parent, child_tid);
                reap(child_tid);
                irq_restore(flags);
                return (child & 0x7FFF_FFFF) | ((code as u32 as u64) << 32);
            }
            irq_restore(flags);
            if !again {
                return u64::MAX;
            }
            // A child stopped, or started: go round and say which.
        }
    }
}

/// Get a mutable reference to a task by TID.
///
/// # Safety
/// Caller must ensure no aliasing.
/// Put `kind` in the lowest free descriptor at or above 3, and say which.
///
/// At or above 3 because 0, 1 and 2 are whatever a spawner wired them to, and a
/// program allocating a descriptor never means to take stdin's place.
pub fn install_fd(tid: usize, kind: crate::task::FdKind) -> Option<usize> {
    crate::fdtable::install(tid, kind, 3)
}

/// The lowest free descriptor at or above 3.
pub fn lowest_free_fd(tid: usize) -> Option<usize> {
    free_fd_at_or_above(tid, 3)
}

/// The lowest free descriptor at or above `floor`, which is what `dup` with a
/// minimum asks for.
pub fn free_fd_at_or_above(tid: usize, floor: usize) -> Option<usize> {
    crate::fdtable::free_at_or_above(tid, floor)
}

/// Empty one descriptor without releasing what it named.
///
/// For unwinding a partial install, where the caller releases the object.
pub fn clear_fd(tid: usize, fd: usize) -> Result<(), ()> {
    if fd >= crate::task::FD_MOST {
        return Err(());
    }
    crate::fdtable::replace(tid, fd, crate::task::FdKind::Empty).map(|_| ())
}

pub unsafe fn get_task_mut(tid: usize) -> Option<&'static mut Task> { unsafe {
    if tid < MAX_TASKS {
        (*slot(tid)).as_mut().map(|r| &mut r.task)
    } else {
        None
    }
}}

/// Kill a task by TID. Marks it Dead and wakes its parent if waiting.
/// Cannot kill TID 0 (idle) or TID 1 (init).
///
/// One task, whatever else is in its program: what a program ending one of
/// its own threads means. Anything said from outside a program is said to
/// the program, and is [`kill_program`].
pub fn kill_task(tid: usize) -> Result<(), ()> {
    if tid <= 1 || tid >= MAX_TASKS {
        return Err(());
    }
    // Killing yourself must not return: the old code marked the task Dead and
    // then let it sysret back to user space, where it kept running until the
    // next preemption dropped it — on an address space reaping was free to
    // tear down underneath it.
    // -9, SIGKILL's number: every status for a task the kernel ended is the
    // negated Linux signal, so a parent can tell a kill from a crash.
    if tid == current_tid() {
        exit_with(-9);
    }
    let flags = irq_save();
    let ended = end_other(tid, -9);
    irq_restore(flags);
    ended
}

/// Kill the program `tid` belongs to: [`end_program`], as SIGKILL would.
pub fn kill_program(tid: usize) -> Result<(), ()> {
    end_program(tid, -9)
}

/// End the program `tid` belongs to: every task in its address space, each
/// with `code` as its status.
///
/// What a kill means to whoever asks for one — a shell, a compositor ending
/// its session, a test that ran out of time, the deadline a signal carries.
/// Ending only the task named left a threaded program's other threads behind,
/// parked for ever, holding everything the program had open: a compositor
/// that ended a toolkit client by its first task kept three of its threads
/// and its connection.
///
/// A task that has not been started is in no program yet, and is ended alone.
///
/// `code` is the negated number of the signal that did it — -9 for a kill —
/// which is the status a fault leaves too.
pub fn end_program(tid: usize, code: i32) -> Result<(), ()> {
    if tid <= 1 || tid >= MAX_TASKS {
        return Err(());
    }
    let me = current_tid();
    let flags = irq_save();
    let (mine, theirs) = unsafe {
        (
            (*slot(me)).as_ref().map_or(0, |t| t.space),
            (*slot(tid)).as_ref().map_or(0, |t| t.space),
        )
    };
    if theirs == 0 {
        if tid == me {
            exit_with(code);
        }
        let ended = end_other(tid, code);
        irq_restore(flags);
        return ended;
    }
    if theirs == mine {
        // The caller's own program, or the one that was running when its
        // deadline passed: the caller goes with it, and last.
        irq_restore(flags);
        exit_program(code);
    }
    let mut ended = Err(());
    each_task_of(theirs, |other| {
        let alive = unsafe { matches!(*slot(other), Some(ref t) if t.state != TaskState::Dead) };
        if other >= 2 && alive && end_other(other, code).is_ok() {
            ended = Ok(());
        }
    });
    irq_restore(flags);
    ended
}

/// Task `tid` has just died: tell its parent's program, as SIGCHLD.
///
/// Only if the parent is another program. A thread's parent is the task that
/// made it, in the program they share, and a thread ending is that program
/// carrying on, not a child of it ending. A program that has not asked to
/// hear is not troubled: the signal's default is to do nothing.
///
/// After the parent has been woken from `sys_wait`, if it was in one, so
/// that the child is there to collect by the time anything hears of it.
/// Interrupts are off.
unsafe fn tell_parent(tid: usize) { unsafe {
    let Some(ref task) = *slot(tid) else { return };
    let parent = task.parent_tid;
    if parent == 0 {
        return;
    }
    let theirs = (*slot(parent)).as_ref().map_or(0, |p| p.space);
    if theirs != 0 && theirs == task.space {
        return;
    }
    // What it exited with, or the signal that ended it.
    let info = if task.exit_code < 0 {
        crate::signal::child_info(tid, crate::signal::CLD_KILLED, -(task.exit_code as i64) as u64)
    } else {
        crate::signal::child_info(tid, crate::signal::CLD_EXITED, (task.exit_code & 0xFF) as u64)
    };
    crate::signal::child_ended(parent, info);
}}

/// End a task that is not the one running, with a status.
fn end_other(tid: usize, code: i32) -> Result<(), ()> {
    unsafe {
        let unwaited = joined_by_word(tid);
        // Marked under its record's lock, and the rest done after it.
        let held = lock_record(tid);
        let ended = match (*slot(tid)).as_mut() {
            Some(task) if task.state != TaskState::Dead => {
                st(tid).unwaited = unwaited;
                task.state = TaskState::Dead;
                task.exit_code = code;
                true
            }
            _ => false,
        };
        drop(held);
        if !ended {
            return Err(());
        }
        crate::ipc::clear_signal_deadline(tid);
        // As in `exit_with`: what others wait on is let go now, and whoever
        // is in a call to it is answered.
        close_descriptors(tid);
        crate::ipc::fail_waiters(tid);
        note_death(tid);
        if runs_elsewhere(tid) {
            // It is running, in ring 3, on another processor, and goes on
            // until that processor comes into the kernel: which this makes
            // it do. It has not finished dying until then — it is on its
            // kernel stack and in its address space — so its parent is
            // told by that processor as it leaves ([`schedule_inner`]).
            st(tid).unannounced = true;
            ANNOUNCE_WANTED.store(true, Ordering::Relaxed);
            interrupt_if_elsewhere(tid);
        } else {
            announce(tid);
        }
        Ok(())
    }
}

/// Say that `tid`, which is dead and on no processor, has died: wake its
/// parent if it is waiting to collect it, and raise SIGCHLD for it.
///
/// The last thing a death does, and for a task ended from another
/// processor it is done later than the rest, by the processor the task was
/// on: a parent woken sooner would collect — and take apart — a task that
/// was still running.
///
/// # Safety
/// Interrupts off.
unsafe fn announce(tid: usize) { unsafe {
    let Some(ref task) = *slot(tid) else { return };
    let (parent, code) = (task.parent_tid, task.exit_code);
    if st(tid).unwaited {
        // A thread joined through its word, which was cleared as it died:
        // that was the whole of its being collected, and no wait is woken
        // for it. Nobody is coming for what is left, so the kernel takes it
        // apart — the next time anybody is at the door, or has nothing to
        // do. Left to its creator's wait, it stayed until its program
        // ended, and a program that made threads one after another used up
        // the machine's places for tasks.
        st(tid).reaped = true;
        REAP_WANTED.store(true, Ordering::Relaxed);
        return;
    }
    if parent == 0 || (*slot(parent)).is_none() {
        // Nobody's either: whoever made it has gone. A thread made by a
        // thread that has since ended is one of these, so they are not
        // left for a processor with nothing to do — a busy machine has
        // none.
        REAP_WANTED.store(true, Ordering::Relaxed);
        return;
    }
    // If parent is blocked in sys_wait, wake it with the dead child's TID
    if waits_for(parent, tid) {
        st(parent).wait_blocked = false;
        st(parent).wait_result = tid;
        st(parent).wait_code = code;
        st(tid).reaped = true;
        unblock_task(parent);
    }
    tell_parent(tid);
}}

/// End the running task's whole program: every other task in its address
/// space, and then the caller, all with one status.
///
/// What `exit` means in C, and what a main thread returning means in Rust.
/// Ending only the caller left a program's other threads behind — parked on a
/// lock nobody would ever release — and with them everything the program had
/// open, which is the program's and not the caller's: a pipe it was writing
/// never reached its end, and a compositor never heard its client go.
pub fn exit_program(code: i32) -> ! {
    let me = current_tid();
    let flags = irq_save();
    let space = unsafe { (*slot(me)).as_ref().map_or(0, |t| t.space) };
    if space != 0 {
        each_task_of(space, |tid| {
            let sibling = unsafe { matches!(*slot(tid), Some(ref t) if tid != me && t.state != TaskState::Dead) };
            if tid >= 2 && sibling {
                let _ = end_other(tid, code);
            }
        });
    }
    irq_restore(flags);
    exit_with(code)
}

/// The id of the program `tid` belongs to — its address space's — or 0 for a
/// kernel task, a task not yet started, or no task at all.
pub fn space_of_task(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    let flags = irq_save();
    let space = unsafe {
        match *slot(tid) {
            Some(ref t) if t.state != TaskState::Dead => t.space,
            _ => 0,
        }
    };
    irq_restore(flags);
    space
}

/// Make a task for the program in `cr3`, to be started there later. It
/// belongs to that program from now on, so a spawner can tell servers about
/// it before it runs.
pub fn create_task_in(cr3: usize) -> Option<usize> {
    let space = crate::userspace::space_of(cr3);
    if space == 0 {
        return None;
    }
    let tid = create_empty_task()?;
    let flags = irq_save();
    unsafe {
        if let Some(t) = (*slot(tid)).as_mut() {
            t.space = space;
        }
        st(tid).npinned = 0;
    }
    irq_restore(flags);
    Some(tid)
}

/// A task of program `space` that has not died, if there is one.
/// Interrupts must be off.
pub fn task_of_space(space: u64) -> Option<usize> {
    if space == 0 {
        return None;
    }
    unsafe {
        tids().find(|&i| matches!(*slot(i), Some(ref t) if t.space == space && t.state != TaskState::Dead))
    }
}

/// Whether any task that has not died is running in address space `cr3`.
///
/// # Safety
/// Interrupts must be off.
unsafe fn space_has_live_task(space: u64) -> bool { unsafe {
    tids().any(|i| matches!(*slot(i), Some(ref t) if t.space == space && t.state != TaskState::Dead))
}}

/// Tell whoever watches `tid` that it has died, and whoever watches its
/// program if it was the program's last task.
///
/// `tid` is already marked dead.
///
/// # Safety
/// Interrupts must be off.
unsafe fn note_death(tid: usize) { unsafe {
    crate::ipc::notify_watchers(tid);
    let Some(ref t) = *slot(tid) else { return };
    let space = t.space;
    if space != 0 && !space_has_live_task(space) {
        crate::ipc::notify_space_watchers(space);
        // Its devices reach nothing any more.
        crate::iommu::program_gone(space);
        // The process has gone: a session it led, and a job it left
        // stopped with nobody to continue it.
        crate::job::process_ended(tid);
    }
}}

/// Whether some task in program `space` is still alive.
pub fn space_is_live(space: u64) -> bool {
    if space == 0 {
        return false;
    }
    let flags = irq_save();
    let live = unsafe { space_has_live_task(space) };
    irq_restore(flags);
    live
}

/// True if `tid` names a live (not-yet-reaped, not-dead) task.
///
/// IPC uses this to reject sends to slots that were never filled: previously
/// `sys_send`/`sys_call` only range-checked the TID, so sending to an empty
/// slot took the slow path and blocked forever with nothing able to wake it.
pub fn task_is_live(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    unsafe {
        (*slot(tid))
            .as_ref()
            .is_some_and(|t| t.state != TaskState::Dead)
    }
}

/// Whether task `tid` may run on processor `cpu`.
///
/// # Safety
/// Interrupts off.
unsafe fn may_run_on(tid: usize, cpu: usize) -> bool { unsafe {
    cpu < 256 && st(tid).allowed[cpu / 64] >> (cpu % 64) & 1 == 1 && crate::percpu::online(cpu)
}}

/// The processors task `tid` may run on, a bit each.
pub fn affinity_of(tid: usize) -> [u64; 4] {
    let flags = irq_save();
    let set = unsafe { st(tid).allowed };
    irq_restore(flags);
    set
}

/// Let task `tid` run only on the processors `set` has, of which one is
/// online. Waiting on one it may not run on, it is put to wait on one it
/// may; running on one, it moves off when that processor next looks
/// (`arrived`) — at once, if that is this one.
pub fn set_affinity(tid: usize, set: [u64; 4]) {
    let flags = irq_save();
    let here = unsafe {
        st(tid).allowed = set;
        if st(tid).queued != NOT_QUEUED && !may_run_on(tid, st(tid).queued_on as usize) {
            unlink_ready(tid);
            enqueue(tid);
        }
        let on = st(tid).on_cpu;
        if on != NO_CPU && !may_run_on(tid, on as usize) {
            if on as usize == crate::percpu::index() {
                true
            } else {
                crate::smp::interrupt(on as usize);
                false
            }
        } else {
            false
        }
    };
    irq_restore(flags);
    if here {
        yield_now();
    }
}

/// The processor task `tid` last ran on, or runs on; `None` if there is no
/// such task.
pub fn last_cpu(tid: usize) -> Option<usize> {
    if tid >= MAX_TASKS {
        return None;
    }
    let flags = irq_save();
    let cpu = unsafe { (*slot(tid)).as_ref().map(|_| st(tid).last_cpu as usize) };
    irq_restore(flags);
    cpu
}

/// How many tasks are running or ready to: Linux's `procs_running`.
pub fn runnable() -> usize {
    let flags = irq_save();
    let n = unsafe {
        tids()
            .filter(|&i| matches!(*slot(i), Some(ref t) if matches!(t.state, TaskState::Ready | TaskState::Running)))
            .count()
    };
    irq_restore(flags);
    n
}

/// Get task info for enumeration. Returns (state, uid, gid, parent_tid) or None.
pub fn task_info(tid: usize) -> Option<(TaskState, u32, u32, usize)> {
    if tid >= MAX_TASKS { return None; }
    unsafe {
        (*slot(tid)).as_ref().map(|t| (t.state, t.uid, t.gid, t.parent_tid))
    }
}

/// The current task's kernel stack, as (base, top). For the idle loop, the
/// stack its processor started on.
///
/// For working out whether a fault is a stack overflow, which from the rsp
/// alone is unknowable: the same address is "nearly empty" or "just ran out"
/// depending on where the allocation starts.
pub fn current_kernel_stack() -> (usize, usize) {
    unsafe {
        let tid = crate::percpu::current();
        match *slot(tid) {
            Some(ref t) if !t.kernel_stack_base.is_null() => {
                (t.kernel_stack_base as usize, t.kernel_stack_base as usize + t.kernel_stack_size)
            }
            _ => crate::percpu::idle_stack(),
        }
    }
}

/// What a processor does when there is nothing for it to run. Does not
/// return: this is task 0, on this processor.
///
/// Entered holding the kernel lock. A processor with nothing to do must not
/// keep it — nothing else could get into the kernel — so it is given up
/// around the `hlt` and taken again when something has woken the
/// processor. The interrupt that does the waking arrives while it is not
/// held, takes it for itself and gives it back (`klock::enter`).
///
/// Before waiting, whatever is ready is run. It used not to be: an
/// interrupt that woke a driver while the machine was idle left the driver
/// in the queue until the next tick found it there, up to ten milliseconds
/// later.
pub fn idle() -> ! {
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    loop {
        // Here the lock is held and interrupts are off.
        loop {
            REAP_WANTED.store(false, Ordering::Relaxed);
            reap_dead();
            if ANNOUNCE_WANTED.swap(false, Ordering::Relaxed) {
                unsafe { announce_due() };
            }
            if !run_ready() {
                break;
            }
        }
        // Nothing to run, so nothing for a tick to share out: what is due
        // is the clock's to fire (`clock::due`), and whatever is made ready
        // here wakes this processor (`smp::wake`). Started again when it
        // runs something (`switch_to`).
        unsafe { stop_tick() };
        // Taken offline: what waits here goes to the others, and this one
        // stops until it is brought back (`smp::park`).
        let me = crate::percpu::index();
        if !crate::percpu::online(me) {
            unsafe {
                runq::give_away(me);
                crate::smp::park();
            }
            continue;
        }
        // Said before the lock goes, so that whoever makes a task ready
        // next — which takes the lock — knows there is a processor to wake
        // for it (`smp::wake`). And what it does from here is nothing.
        unsafe {
            crate::usage::idling();
            crate::percpu::nap();
        }
        crate::klock::release();
        // An interrupt is not taken between `sti` and the instruction after
        // it, so nothing can arrive after the look above and before the
        // wait: it arrives during the wait, and ends it.
        unsafe {
            core::arch::asm!("sti; hlt; cli", options(nostack, nomem));
            crate::percpu::woke();
        }
        crate::klock::acquire();
    }
}

/// Stop this processor's tick: it has nothing to run, and a tick would
/// only find that out a hundred times a second. The first processor's is
/// the 8254's line, masked — where the clock is the counter and its own
/// timer fires what is due (`clock::tick_may_stop`); elsewhere the tick is
/// the clock, and goes on. Every other processor's is its own timer.
///
/// # Safety
/// Interrupts off.
unsafe fn stop_tick() { unsafe {
    let me = crate::percpu::index();
    if TICK_STOPPED[me] {
        return;
    }
    if me == 0 {
        if !crate::clock::tick_may_stop() {
            return;
        }
        crate::intc::disable(0);
    } else {
        crate::lapic::tick(false);
    }
    TICK_STOPPED[me] = true;
}}

/// Start this processor's tick again: it is about to run something.
///
/// # Safety
/// Interrupts off.
unsafe fn start_tick() { unsafe {
    let me = crate::percpu::index();
    if !TICK_STOPPED[me] {
        return;
    }
    if me == 0 {
        crate::intc::enable(0);
    } else {
        crate::lapic::tick(true);
    }
    TICK_STOPPED[me] = false;
}}

/// From the idle loop: run whatever is ready, and say whether anything was.
/// Comes back when this processor next has nothing to do.
fn run_ready() -> bool {
    unsafe {
        if !anything_to_run() {
            return false;
        }
        // Whether anything ran: what waits elsewhere may be nothing this
        // processor may run, and an idle loop that went round again for it
        // would go round for ever, with the kernel's lock.
        let switches = crate::usage::switches_here();
        schedule_inner(false);
        crate::usage::switches_here() != switches
    }
}

/// Reap every dead task that nobody still has a claim on.
///
/// Run from the idle loop, which picks up tasks nobody collects: those with no
/// parent, or whose parent is gone. A task its parent waits for is reaped by
/// the wait itself.
pub fn reap_dead() {
    let mut at = 1;
    loop {
        // One task at a time with interrupts off, so a parent reaping the
        // child it just collected can never find it half torn down.
        let flags = irq_save();
        let next = unsafe { table().next_used(at) };
        if let Some(i) = next {
            unsafe { reap(i) };
            at = i + 1;
        }
        irq_restore(flags);
        if next.is_none() {
            break;
        }
    }
}

/// Tear down task `i`, if it is dead and nobody still has a claim on it, and
/// with it any of its children that are dead too — see [`reap_one`]. Does
/// nothing otherwise.
///
/// Must run with interrupts off. Nothing in here yields.
unsafe fn reap(i: usize) { unsafe {
    // Its children are nobody's now, and the dead among them are taken
    // apart too, and theirs: a sweep for dead tasks with no parent, again
    // for as long as one of them had dead children of its own.
    let mut more = reap_one(i);
    while more {
        more = false;
        let mut at = 1;
        while let Some(j) = table().next_used(at) {
            at = j + 1;
            let orphan = matches!(*slot(j), Some(ref t) if t.state == TaskState::Dead && t.parent_tid == 0);
            more |= orphan && reap_one(j);
        }
    }
}}

/// Tear down task `i` — its descriptors, IPC, IRQs, memory and kernel stack —
/// and free its slot, if it is dead and nobody still has a claim on it: its
/// parent collected it with sys_wait, or it has no parent, or the parent is
/// gone.
///
/// Its children are orphaned, and the ones already dead are returned, since
/// nothing will collect them now. A thread is a child of the thread that made
/// it, so a program's exited threads are exactly these, and the address space
/// they share goes only when the last of them does.
/// Let go of everything a task holds that somebody else can be waiting on.
///
/// Called when a task dies rather than when it is reaped. What it does *not*
/// touch is memory: a dead task keeps that until it is collected, which is
/// what lets `sys_wait` report an exit status.
///
/// The descriptors are its program's, so what goes here is the task's use of
/// them — and the descriptors themselves only if it was the last task the
/// program had. A thread that exits closes nothing.
pub fn close_descriptors(tid: usize) {
    // The robust mutexes it held, while its memory is still where it was.
    crate::threads::let_go(tid);
    // What it used is its program's, before it leaves the program's record.
    crate::usage::task_ended(tid);
    crate::fdtable::task_gone(tid);
    // The futex word it waited on, if it died waiting: a wake counted for it
    // would be one a waiter that will run again was not given.
    crate::futex::cleanup_task(tid);
    // And its program's capabilities, which go with the last task to use
    // them: a dead task is the authority for nothing.
    crate::cap::task_gone(tid);
}

unsafe fn reap_one(i: usize) -> bool { unsafe {
    let Some(ref mut task) = *slot(i) else {
        return false;
    };
    if task.state != TaskState::Dead {
        return false;
    }
    let parent = task.parent_tid;
    let can_reap = st(i).reaped || parent == 0 || (*slot(parent)).is_none();
    if !can_reap {
        return false;
    }
    // Its parent was waiting for it, has been woken to collect it, and has
    // not run yet. The parent reads the child's name when it does, and takes
    // it apart itself (`sys_wait_for`); this is somebody else — a processor
    // with nothing to do — and must not get there first. With one processor
    // nothing could: the parent was ready, and the idle loop runs when
    // nothing is. With two, the parent was told its child was process 0.
    if parent != 0 && st(parent).wait_result == i {
        return false;
    }
    // Out of whatever queue it is in first, so that no processor takes it
    // from here on; stamped then with the reaping epoch; and taken apart
    // once every processor choosing began after that (`CHOOSING`) — one
    // that began before may have taken it already and not yet claimed it,
    // its record in hand. Whoever next comes into the kernel under the one
    // lock, or has nothing to do, tries again.
    unlink_ready(i);
    if st(i).reap_stamp == 0 {
        st(i).reap_stamp = REAP_EPOCH.fetch_add(1, Ordering::SeqCst);
    }
    if !chosen_since(st(i).reap_stamp) {
        REAP_WANTED.store(true, Ordering::Relaxed);
        return false;
    }
    // Ended from another processor, and still running there: what is freed
    // below is what it is standing on. That processor has been interrupted
    // and will leave it; whoever next comes into the kernel, or has nothing
    // to do, tries again.
    if core::ptr::read_volatile(&raw const st(i).on_cpu) != NO_CPU {
        REAP_WANTED.store(true, Ordering::Relaxed);
        return false;
    }
    // For a task that died some way other than `exit_with` or `kill_task` —
    // a kernel task, say. Ordinarily it left its table when it died.
    crate::fdtable::task_gone(i);
    crate::cap::task_gone(i);
    // Reclaim pipes it created but never attached to an fd
    crate::pipe::cleanup_orphans(i);
    // Clean up IPC state and unblock tasks waiting on this one
    crate::ipc::cleanup_task_ipc(i);
    // Objects it paged for have no pager now.
    crate::memobj::task_gone(i);
    // And what it served has no server. Before its endpoint is closed: that
    // is how its objects are told from the next task's with this number.
    crate::served::server_gone(i);
    // Unregister any IRQ handlers
    crate::irq_dispatch::unregister_task_irqs(i);
    // Clean up futex waiters
    crate::futex::cleanup_task(i);
    // Shared memory it mapped or made. If its program lives on, so do the
    // mappings: they are handed to a task still running there.
    let space = task.space;
    let survivor = if space == 0 {
        None
    } else {
        tids().find(|&j| matches!(*slot(j), Some(ref t) if t.tid != i && t.space == space && t.state != TaskState::Dead))
    };
    crate::shmem::cleanup_task(i, survivor);
    // Reclaim sys_phys_alloc reservations it never released
    crate::pmm::release_task_frames(i);
    // Destroy the address space only once the last task using
    // it is gone. Threads share one; tearing it down when the
    // first exits would pull it out from under the others.
    let cr3 = task.cr3;
    if cr3 != 0 && cr3 != crate::paging::kernel_cr3()
        && crate::userspace::addrspace_unref(cr3)
    {
        crate::paging::destroy_address_space(cr3);
        crate::userspace::unregister_address_space(cr3);
    }
    task.free_stack();
    st(i).reaped = false;
    // Clean up wait state if this task was a parent
    st(i).wait_blocked = false;
    st(i).wait_result = 0;
    st(i).wait_target = 0;
    st(i).wait_group = 0;
    st(i).wait_reports = 0;
    st(i).wait_again = false;
    // Every capability to it names nothing from here on, whoever holds one.
    crate::cap::close_endpoint(i);
    st(i).process_id = 0;
    st(i).held = false;
    st(i).unannounced = false;
    st(i).unwaited = false;
    crate::job::forget(i);
    // Dead, it may still be in a ready queue, or on a list of waiters, each of
    // which goes through its record.
    unlink_ready(i);
    crate::waitlist::forget(i);
    // Given back under its lock: whoever looks at another task's record does
    // so holding it, and finds a record or none, never one being freed.
    let held = record_lock(i).lock();
    table().empty(i);
    drop(held);

    // Left naming this TID, its children would wait on a parent that is gone,
    // and whatever took the slot next would find them its own — collected by
    // its sys_wait, counted as its threads, and startable by it with whatever
    // user ID they were made with.
    let mut dead = false;
    for j in tids().filter(|&j| j >= 1) {
        if let Some(ref mut child) = *slot(j) {
            if child.parent_tid == i {
                child.parent_tid = 0;
                dead |= child.state == TaskState::Dead;
            }
        }
    }
    dead
}}

/// Get the current task's CR3 (address space).
pub fn current_task_cr3() -> usize {
    let tid = current_tid();
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => task.cr3,
            None => 0,
        }
    }
}

/// Get the current task's capability bits.
pub fn current_task_caps() -> u32 {
    crate::cap::bits_of(current_tid())
}

/// Check if the current task has a given capability.
pub fn current_task_has_cap(cap: u32) -> bool {
    crate::cap::bits_of(current_tid()) & cap != 0
}

/// Get the current task's UID.
pub fn current_task_uid() -> u32 {
    let tid = current_tid();
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => task.uid,
            None => 0,
        }
    }
}

/// Get the current task's GID.
pub fn current_task_gid() -> u32 {
    let tid = current_tid();
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => task.gid,
            None => 0,
        }
    }
}

/// Get a task's UID and GID by TID.
pub fn task_uid_gid(tid: usize) -> Result<(u32, u32), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => Ok((task.uid, task.gid)),
            None => Err(()),
        }
    }
}

/// Set a task's UID.
pub fn set_task_uid(tid: usize, uid: u32) -> Result<(), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match (*slot(tid)).as_mut() {
            Some(task) => { task.uid = uid; Ok(()) }
            None => Err(()),
        }
    }
}

/// The groups a task is in besides its own, into `out`. How many there are.
pub fn task_groups(tid: usize, out: &mut [u32; crate::task::MAX_GROUPS]) -> Result<usize, ()> {
    if tid >= MAX_TASKS {
        return Err(());
    }
    let flags = irq_save();
    let got = unsafe {
        (*slot(tid)).as_ref().map(|t| {
            *out = t.groups;
            t.ngroups as usize
        })
    };
    irq_restore(flags);
    got.ok_or(())
}

/// Say which groups a task is in besides its own.
pub fn set_task_groups(tid: usize, groups: &[u32]) -> Result<(), ()> {
    if tid >= MAX_TASKS || groups.len() > crate::task::MAX_GROUPS {
        return Err(());
    }
    let flags = irq_save();
    let set = unsafe {
        (*slot(tid)).as_mut().map(|t| {
            t.groups = [0; crate::task::MAX_GROUPS];
            t.groups[..groups.len()].copy_from_slice(groups);
            t.ngroups = groups.len() as u8;
        })
    };
    irq_restore(flags);
    set.ok_or(())
}

/// Say who a task is: its user, its group and the groups it is in, in one
/// step.
pub fn identify(tid: usize, uid: u32, gid: u32, groups: &[u32]) -> Result<(), ()> {
    if tid >= MAX_TASKS || groups.len() > crate::task::MAX_GROUPS {
        return Err(());
    }
    let flags = irq_save();
    let set = unsafe {
        (*slot(tid)).as_mut().map(|t| {
            t.uid = uid;
            t.gid = gid;
            t.groups = [0; crate::task::MAX_GROUPS];
            t.groups[..groups.len()].copy_from_slice(groups);
            t.ngroups = groups.len() as u8;
        })
    };
    irq_restore(flags);
    set.ok_or(())
}

/// Set a task's GID.
pub fn set_task_gid(tid: usize, gid: u32) -> Result<(), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match (*slot(tid)).as_mut() {
            Some(task) => { task.gid = gid; Ok(()) }
            None => Err(()),
        }
    }
}

/// Create an empty task slot (Blocked, cr3=0, caps=0). Returns TID.
/// The address space a task is running in.
fn read_cr3_of(tid: usize) -> usize {
    unsafe { (*slot(tid)).as_ref().map_or(0, |t| t.cr3) }
}

/// The address space `tid` runs in, or 0 if there is no such task.
pub fn task_cr3(tid: usize) -> usize {
    if tid >= MAX_TASKS {
        return 0;
    }
    read_cr3_of(tid)
}

/// The task that created `tid`, while both exist.
///
/// Reaping a task orphans its children, so a TID that has been given to
/// somebody else is never mistaken for their creator.
pub fn parent_of(tid: usize) -> Option<usize> {
    if tid >= MAX_TASKS {
        return None;
    }
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(t) if t.parent_tid != 0 => Some(t.parent_tid),
            _ => None,
        }
    }
}

/// How many tasks `tid`'s program has, as its allowance counts them
/// (`syscall::A_PROGRAMS_TASKS`): its own that have not died, and every task
/// one of them made in another program that has not been collected.
///
/// The bound on making tasks without any authority: a program may make
/// itself threads and children, and cannot make so many that it exhausts the
/// table for everybody else.
pub fn program_tasks(tid: usize) -> usize {
    let flags = irq_save();
    let n = unsafe {
        let space = (*slot(tid)).as_ref().map_or(0, |t| t.space);
        let of_program = |t: usize| space != 0 && matches!(*slot(t), Some(ref x) if x.space == space);
        tids()
            .filter(|&t| t >= 1)
            .filter(|&t| match *slot(t) {
                Some(ref x) if x.space == space && space != 0 => x.state != TaskState::Dead,
                Some(ref x) => x.parent_tid != 0 && of_program(x.parent_tid),
                None => false,
            })
            .count()
    };
    irq_restore(flags);
    n
}

/// What a task's record is made from (`Table::fill_from`).
static TASK_TEMPLATE: crate::table::Template<TaskRec> = crate::table::Template(Some(TaskRec::empty()));

pub fn create_empty_task() -> Option<usize> {
    // The stack up front, before the slot is claimed (`kstack.rs`).
    let (stack_base, _) = crate::kstack::alloc()?;
    let stack_base = stack_base as *mut u8;

    let flags = irq_save();
    let tid = match unsafe { find_free_tid() } {
        Some(t) => t,
        None => {
            irq_restore(flags);
            crate::kstack::free(stack_base as usize);
            return None;
        }
    };

    let parent = current_tid();
    let (parent_uid, parent_gid, parent_groups, parent_ngroups) = unsafe {
        match (*slot(parent)).as_ref() {
            Some(t) => (t.uid, t.gid, t.groups, t.ngroups),
            None => (0, 0, [0; crate::task::MAX_GROUPS], 0),
        }
    };
    unsafe {
        // Made in its room, from what every task starts as: built here and
        // moved, the record took six kilobytes of the caller's kernel stack.
        // Under its lock, so that one who looks at it holding that finds it
        // whole or not at all.
        let held = record_lock(tid).lock();
        let made = table().fill_from(tid, &TASK_TEMPLATE, |r| {
            let t = &mut r.task;
            t.tid = tid;
            t.kernel_stack_base = stack_base;
            t.kernel_stack_size = KERNEL_STACK_SIZE;
            t.parent_tid = parent;
            t.uid = parent_uid;
            t.gid = parent_gid;
            t.groups = parent_groups;
            t.ngroups = parent_ngroups;
            // Clean, not the parent's. A new task inheriting whatever the
            // registers held when it was created would be reading its
            // creator's data, and a thread has no more right to that than a
            // stranger: it can already read the memory, but not the moment.
            crate::fpu::clean_into(&raw mut t.fpu);
        });
        drop(held);
        if made.is_err() {
            // No memory for its record: nothing was made.
            irq_restore(flags);
            crate::kstack::free(stack_base as usize);
            return None;
        }
        // A new record is a clean one: nothing pinned, no signals held back
        // or waiting.
        crate::cap::open_endpoint(tid);
        // A process id of its own, a table of its own and a capability space
        // of its own, empty. A task started as a thread gives them up for its
        // program's; a task started as a program keeps them.
        crate::cap::task_made(tid);
        crate::usage::task_made(tid);
        crate::threads::task_made(tid, parent);
        st(tid).process_id = crate::cap::endpoint_of(tid);
        // In its creator's process group and session: a job is whatever a
        // shell started, and what those started.
        st(tid).held = false;
        st(tid).yielded = false;
        // As nice as its creator, and in its class: a thread or a child runs
        // as the task that made it was told to, as on Linux.
        st(tid).nice = st(parent).nice;
        st(tid).allowed = st(parent).allowed;
        st(tid).policy = st(parent).policy;
        st(tid).rt_base = st(parent).rt_base;
        st(tid).rt = st(parent).rt_base;
        // It has run nothing, and joining a band puts it where that band
        // has got to.
        st(tid).vrun = 0;
        st(tid).unannounced = false;
        st(tid).unwaited = false;
        st(tid).on_cpu = NO_CPU;
        // Where its creator is, which has just written what it starts with.
        st(tid).last_cpu = crate::percpu::index() as u16;
        st(tid).measured_on = crate::percpu::index() as u16;
        crate::job::born(tid, parent, st(tid).process_id);
        crate::fdtable::attach_new(tid);
        // Limited as its creator's program is: a child forked or spawned
        // runs as its parent was told to.
        crate::fdtable::runs_like(tid, parent);
    }
    irq_restore(flags);

    Some(tid)
}

/// Give a new task what its creator holds: its capabilities and its band.
///
/// A thread is not a new principal: it runs in its creator's address space and
/// can already do anything its creator can. It used to start with nothing — no
/// capability, so it could call no server, not even the VFS about a file its
/// program had opened — and the ordinary band whatever its program's. Then
/// with a copy of each, which was the creator's as it stood: what either was
/// given afterwards the other did not have. Now a `thread` uses its
/// program's capabilities (`cap::share`), as it uses its program's
/// descriptors, and anything it was given before it started becomes the
/// program's. A forked child gets a copy, as it gets copies of the
/// descriptors (`fdtable::copy_into`). Either way the band is the creator's.
/// False if a child could not be given all its creator holds: a space grows
/// as it is written, and there may be no memory for the copy.
pub fn inherit_from_creator(tid: usize, creator: usize, thread: bool) -> bool {
    if tid >= MAX_TASKS || creator >= MAX_TASKS || tid == creator {
        return false;
    }
    let whole = if thread {
        crate::cap::share(tid, creator)
    } else {
        crate::cap::copy_into(tid, creator)
    };
    let flags = irq_save();
    unsafe {
        let band = (*slot(creator)).as_ref().map(|t| t.base_priority);
        if let (Some(band), Some(dst)) = (band, (*slot(tid)).as_mut()) {
            dst.base_priority = band;
            dst.priority = band;
        }
    }
    irq_restore(flags);
    whole
}

/// Configure and start a previously created empty task for userspace entry.
/// Start `tid` in `cr3` at `rip`, with `arg` in RDI.
///
/// `arg` is how a thread receives its closure; ordinary spawns pass 0.
pub fn start_task(tid: usize, rip: u64, rsp: u64, cr3: usize, arg: u64) -> Result<(), ()> {
    if tid >= MAX_TASKS {
        return Err(());
    }
    // Interrupts off, as `start_forked` has them: this is a system call's
    // doing and those run with interrupts on, and putting a task on the
    // ready queue is three writes. A tick between them queued the task it
    // preempted in the same place, and the one being started was ready and
    // in no queue — a program that was started and never ran.
    let flags = irq_save();
    let started = unsafe { start_task_locked(tid, rip, rsp, cr3, arg) };
    irq_restore(flags);
    started
}

/// [`start_task`], with interrupts off.
unsafe fn start_task_locked(tid: usize, rip: u64, rsp: u64, cr3: usize, arg: u64) -> Result<(), ()> {
    unsafe {
        let task = match (*slot(tid)).as_mut() {
            Some(t) => t,
            None => return Err(()),
        };
        if task.state != TaskState::Blocked {
            return Err(());
        }

        // A task made for one program cannot be started in another: servers
        // may already hold things for it under that program's name.
        let space = crate::userspace::space_of(cr3);
        if task.space != 0 && task.space != space {
            return Err(());
        }
        task.space = space;

        // Another task is now running here. Threads share an address space, so
        // this is what stops the first one to exit destroying it.
        task.cr3 = cr3;
        crate::userspace::addrspace_ref(cr3);

        // Set up context so context_switch enters enter_user_trampoline
        // with r12=rip, r13=rsp, r14=cr3
        let stack_top = task.kernel_stack_base as usize + task.kernel_stack_size;
        let stack_top = stack_top & !0xF;

        let trampoline_addr =
            crate::userspace::enter_user_trampoline as *const () as usize as u64;

        // Write trampoline return address on kernel stack
        let sp = stack_top as *mut u64;
        core::ptr::write(sp.sub(1), crate::task::task_exit_trampoline as *const () as u64);

        task.context.rip = trampoline_addr;
        task.context.rsp = (stack_top - 8) as u64;
        task.context.rbp = 0;
        task.context.r12 = rip;
        task.context.r13 = rsp;
        task.context.r14 = cr3 as u64;
        task.context.r15 = arg;

        task.state = TaskState::Ready;
        enqueue(tid);
        Ok(())
    }
}

/// Grant a capability to a task.
pub fn grant_cap(tid: usize, cap: u32) -> Result<(), ()> {
    if tid >= MAX_TASKS || unsafe { (*slot(tid)).is_none() } {
        return Err(());
    }
    // The bits, and wildcard/full-range caps for them in the CSpace.
    if crate::cap::add_bits(tid, cap) { Ok(()) } else { Err(()) }
}

/// Set a file descriptor entry on a task.
pub fn set_fd(tid: usize, fd: usize, entry: crate::task::FdKind) -> Result<(), ()> {
    if tid >= MAX_TASKS || fd >= crate::task::FD_MOST {
        return Err(());
    }
    let old = crate::fdtable::replace(tid, fd, entry)?;
    // Whatever the descriptor named before is closed, as dup2 closes it.
    if !old.is_empty() {
        crate::pipe::release_fd(&old);
    }
    Ok(())
}

/// Claim the lowest free descriptor on the current task.
///
/// Sockets are created by the task that will use them, rather than wired up by
/// a parent the way stdio and pipes are, so there is nobody to say which fd
/// number to use.
pub fn current_alloc_fd(entry: crate::task::FdKind) -> Result<usize, ()> {
    // 0, 1 and 2 are stdio by convention even when unset, and handing one out
    // would silently redirect a program's output.
    crate::fdtable::install(crate::percpu::current(), entry, 3).ok_or(())
}

/// Set the pager task for a given task.
pub fn set_pager(tid: usize, pager_tid: usize) -> Result<(), ()> {
    // pager_tid is used to index TASK_IPC in ipc::fault_call, so it has to be
    // in range too -- not just tid.
    if tid >= MAX_TASKS || pager_tid >= MAX_TASKS {
        return Err(());
    }
    unsafe {
        match (*slot(tid)).as_mut() {
            Some(task) => {
                task.pager_tid = pager_tid;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Get the pager TID for the current task. Returns 0 if no pager.
pub fn current_task_pager() -> usize {
    let tid = current_tid();
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => task.pager_tid,
            None => 0,
        }
    }
}

/// How many ranges of its program's memory a system call can be remembered
/// to have checked. More than that and all of the program's memory is held.
const PINS: usize = 8;


/// The current task's system call has checked `len` bytes at `addr`.
pub fn pin(addr: u64, len: u64) {
    if len == 0 {
        return;
    }
    let tid = current_tid();
    let flags = irq_save();
    unsafe {
        let n = st(tid).npinned as usize;
        if n < PINS {
            st(tid).pinned[n] = (addr & !0xFFF, addr.saturating_add(len - 1) | 0xFFF);
        }
        // One past the last there is room for means "everything".
        st(tid).npinned = (n + 1).min(PINS + 1) as u8;
    }
    irq_restore(flags);
}

/// The current task's system call is over: what it checked is its
/// program's to lose again.
#[inline]
pub fn unpin() {
    let tid = current_tid();
    unsafe { st(tid).npinned = 0 };
}

/// Whether a system call some task of program `space` is in has checked
/// the page at `va`. Interrupts must be off.
pub fn pinned(space: u64, va: u64) -> bool {
    unsafe {
        for tid in tids().filter(|&t| t >= 1) {
            let n = st(tid).npinned as usize;
            if n == 0 {
                continue;
            }
            if !matches!(*slot(tid), Some(ref t) if t.space == space && t.state != TaskState::Dead) {
                continue;
            }
            if n > PINS || st(tid).pinned[..n].iter().any(|&(from, to)| (from..=to).contains(&va)) {
                return true;
            }
        }
    }
    false
}

/// Whether a system call another task of program `space` is in has checked
/// any of `[from, to)`. Interrupts must be off, and stay off until what is
/// decided by it is done: that is what refuses an unmap of a buffer a call
/// is waiting to copy into (`SYS_MUNMAP`, `SYS_ADDRSPACE_GIVE`,
/// `shmem::unmap`).
pub fn pinned_by_another(space: u64, from: u64, to: u64) -> bool {
    let me = current_tid();
    unsafe {
        for tid in tids().filter(|&t| t >= 1 && t != me) {
            let n = st(tid).npinned as usize;
            if n == 0 {
                continue;
            }
            if !matches!(*slot(tid), Some(ref t) if t.space == space && t.state != TaskState::Dead) {
                continue;
            }
            if n > PINS || st(tid).pinned[..n].iter().any(|&(lo, hi)| lo < to && from <= hi) {
                return true;
            }
        }
    }
    false
}

/// Every task of program `space`, ended or not, until it is taken apart —
/// the idle task aside — to `f`, with interrupts off.
pub fn each_task_of(space: u64, mut f: impl FnMut(usize)) {
    if space == 0 {
        return;
    }
    let flags = irq_save();
    unsafe {
        for t in tids().filter(|&t| t >= 1) {
            if matches!(*slot(t), Some(ref task) if task.space == space) {
                f(t);
            }
        }
    }
    irq_restore(flags);
}

/// Whether task `tid` is, or was, a task of program `space`.
pub fn task_in_space(tid: usize, space: u64) -> bool {
    let flags = irq_save();
    let is = space != 0 && unsafe { matches!(*slot(tid), Some(ref t) if t.space == space) };
    irq_restore(flags);
    is
}

/// Every task of the process `tid` is a task of that has not been taken
/// apart — its threads, dead or alive, and what it began as — to `f`, with
/// interrupts off.
pub fn each_task_of_process(tid: usize, mut f: impl FnMut(usize)) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let pid = st(tid).process_id;
        for t in tids() {
            if st(t).process_id == pid {
                f(t);
            }
        }
    }
    irq_restore(flags);
}

/// The processor task `tid` is running on, if it is running. Interrupts must
/// be off.
pub fn running_on(tid: usize) -> Option<usize> {
    if tid >= MAX_TASKS {
        return None;
    }
    let cpu = unsafe { st(tid).on_cpu };
    (cpu != NO_CPU).then_some(cpu as usize)
}

/// The processor task `tid` is running on, if it is running and that is not
/// this one. Interrupts must be off.
pub fn running_elsewhere(tid: usize) -> Option<usize> {
    if tid >= MAX_TASKS {
        return None;
    }
    let cpu = unsafe { st(tid).on_cpu };
    (cpu != NO_CPU && cpu as usize != crate::percpu::index()).then_some(cpu as usize)
}

/// Whether the current task is a driver or a server: what memory is taken
/// from others for, and not from.
pub fn current_is_privileged() -> bool {
    let tid = current_tid();
    unsafe { matches!(*slot(tid), Some(ref t) if t.base_priority < PRIO_NORMAL) }
}

/// Whether memory may be taken from program `space` to be written out:
/// not from a driver's or a server's, which are what it would be written
/// out with. Interrupts must be off.
pub fn space_gives_memory(space: u64) -> bool {
    unsafe {
        let mut any = false;
        for tid in tids().filter(|&t| t >= 1) {
            if let Some(ref t) = *slot(tid) {
                if t.space == space && t.state != TaskState::Dead {
                    if t.base_priority < PRIO_NORMAL {
                        return false;
                    }
                    any = true;
                }
            }
        }
        any
    }
}

/// Check if the current task can allocate `pages` more pages.
/// Returns false if the allocation would exceed the task's memory limit.
pub fn current_task_check_mem(pages: usize) -> bool {
    let tid = current_tid();
    unsafe {
        match (*slot(tid)).as_ref() {
            Some(task) => {
                if task.mem_limit == 0 {
                    true // unlimited
                } else {
                    task.mem_pages + pages <= task.mem_limit
                }
            }
            None => false,
        }
    }
}

/// Pages charged to the current task.
pub fn current_task_mem() -> usize {
    let tid = current_tid();
    unsafe { (*slot(tid)).as_ref().map_or(0, |t| t.mem_pages) }
}

/// Add `pages` to the current task's memory usage counter.
pub fn current_task_charge_mem(pages: usize) {
    let tid = current_tid();
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            task.mem_pages += pages;
        }
    }
}

/// Subtract `pages` from the current task's memory usage counter.
pub fn current_task_uncharge_mem(pages: usize) {
    let tid = current_tid();
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            task.mem_pages = task.mem_pages.saturating_sub(pages);
        }
    }
}

/// Subtract `pages` from a specific task's memory usage counter.
pub fn uncharge_task_mem(tid: usize, pages: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    unsafe {
        if let Some(ref mut task) = *slot(tid) {
            task.mem_pages = task.mem_pages.saturating_sub(pages);
        }
    }
}

/// Set the memory limit (in pages) for a task. 0 = unlimited.
pub fn set_mem_limit(tid: usize, limit: usize) -> Result<(), ()> {
    if tid >= MAX_TASKS {
        return Err(());
    }
    unsafe {
        match (*slot(tid)).as_mut() {
            Some(task) => {
                task.mem_limit = limit;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Get a file descriptor entry for the current task.
pub fn current_fd(fd: usize) -> crate::task::FdKind {
    if fd >= crate::task::FD_MOST {
        return crate::task::FdKind::Empty;
    }
    crate::fdtable::get(crate::percpu::current(), fd)
}

/// The top of the running task's kernel stack.
///
/// The syscall stub sets RSP to this and pushes a [`UserFrame`], so it is also
/// where that frame is, less its size. Published to the per-CPU area on every
/// switch, and by `enter_user_inner` when a task first goes to user mode.
pub fn current_kernel_stack_top() -> u64 {
    let flags = irq_save();
    let top = unsafe {
        match (*slot(current_tid())).as_ref() {
            Some(t) if !t.kernel_stack_base.is_null() => {
                t.kernel_stack_base as u64 + t.kernel_stack_size as u64
            }
            _ => 0,
        }
    };
    irq_restore(flags);
    top
}

/// The same, where it is: for what changes where a task goes back to — a
/// handler the kernel runs on its way out of the call (`signal.rs`).
pub fn current_user_frame_mut() -> Option<&'static mut crate::task::UserFrame> {
    let top = current_kernel_stack_top();
    if top == 0 {
        return None;
    }
    let at = top as usize - core::mem::size_of::<crate::task::UserFrame>();
    Some(unsafe { &mut *(at as *mut crate::task::UserFrame) })
}

/// The register state the running task will return to user mode with.
///
/// Only meaningful inside a system call, which is the only time this is asked.
fn current_user_frame() -> Option<crate::task::UserFrame> {
    let top = current_kernel_stack_top();
    if top == 0 {
        return None;
    }
    let at = top as usize - core::mem::size_of::<crate::task::UserFrame>();
    Some(unsafe { core::ptr::read(at as *const crate::task::UserFrame) })
}

/// Make a copy of the running task: a task of its own, in a copy of this
/// address space, that returns 0 from the system call this is inside.
///
/// Returns the child's TID to the caller. Everything a task has that a child
/// inherits is copied here: descriptors — with their reference counts — and
/// capabilities and band through `inherit_from_creator`, the thread pointer,
/// the memory limit, and the parent. What is not copied is the thread-list
/// word `SYS_SET_CLEAR_TID` registered: it names a lock in the *parent's*
/// thread list, and a child clearing it on exit would unlock a list it was
/// never in.
pub fn fork_current() -> Option<usize> {
    let frame = current_user_frame()?;
    let parent = current_tid();
    let (parent_cr3, fs_base, gs_base, mem_limit, uid, gid) = {
        let flags = irq_save();
        // The parent is the caller, running: its bases are the processor's,
        // which its program may have changed since it last was switched out.
        let got = unsafe {
            (*slot(parent))
                .as_ref()
                .map(|t| (t.cr3, crate::cpu::fs_base(), crate::cpu::user_gs_base(), t.mem_limit, t.uid, t.gid))
        };
        irq_restore(flags);
        got?
    };
    if parent_cr3 == 0 {
        return None;
    }

    // The address space first: it is the part that can run out, and it is the
    // part that is worth doing before a task slot is taken.
    let child_cr3 = crate::userspace::create_address_space()?;
    // In one step, with both spaces' locks held. The parent's own tables
    // are walked and changed — what it could write it cannot, until it has
    // a copy — and another thread of it, run half way through, could unmap
    // what is being walked. And before anything else happens, every
    // processor is made to forget what it remembers of the parent: this
    // one, and any that is running another of its threads, which would
    // otherwise go on writing to pages the child now has too.
    let held = crate::paging::lock_two_spaces(parent_cr3, child_cr3);
    let copied = unsafe { crate::paging::copy_user_space(parent_cr3, child_cr3) };
    crate::tlb::stale(parent_cr3);
    crate::tlb::sync();
    unsafe { crate::paging::write_cr3(crate::paging::read_cr3()) };
    drop(held);
    let pages = match copied {
        Some(n) => n,
        None => {
            drop_unused_space(child_cr3);
            return None;
        }
    };

    let tid = match create_task_in(child_cr3) {
        Some(t) => t,
        None => {
            drop_unused_space(child_cr3);
            return None;
        }
    };
    // What the parent holds, and a second descriptor for everything it has
    // open, the working directory included: the child's own, to give up or
    // close without the parent noticing. A child that could not have all of
    // them is not made.
    if !inherit_from_creator(tid, parent, false) || !crate::fdtable::copy_into(tid, parent) {
        let _ = kill_task(tid);
        return None;
    }
    {
        let flags = irq_save();
        unsafe {
            if let Some(t) = (*slot(tid)).as_mut() {
                t.fs_base = fs_base;
                t.gs_base = gs_base;
                t.mem_limit = mem_limit;
                t.mem_pages = pages;
                t.parent_tid = parent;
                t.uid = uid;
                t.gid = gid;
                // The parent's floating-point state is in the registers right
                // now — this is its system call — so it is saved from there
                // rather than copied from where the last switch left it.
                crate::fpu::save(&raw mut t.fpu);
            }
            // And it holds back the signals its parent does.
            crate::signal::task_like(tid, parent);
        }
        irq_restore(flags);
    }

    if start_forked(tid, child_cr3, &frame).is_err() {
        let _ = kill_task(tid);
        return None;
    }
    Some(tid)
}

/// Is any task running in the address space with this id?
///
/// Or still on a processor in it, having been ended from another: that one
/// is dead, and is executing there all the same until its processor is
/// brought into the kernel. This is what is asked before an address space
/// is thrown away.
pub fn space_in_use(space: u64) -> bool {
    if space == 0 {
        return false;
    }
    let flags = irq_save();
    let used = unsafe {
        space_has_live_task(space)
            || tids().filter(|&i| i >= 1).any(|i| {
                st(i).on_cpu != NO_CPU && matches!(*slot(i), Some(ref t) if t.space == space)
            })
    };
    irq_restore(flags);
    used
}

/// How many live tasks belong to a program.
pub fn space_task_count(space: u64) -> usize {
    let flags = irq_save();
    let n = unsafe {
        tids().filter(|&i| matches!(*slot(i), Some(ref t) if t.space == space && t.state != TaskState::Dead)).count()
    };
    irq_restore(flags);
    n
}

/// Become another program: keep the task, change the address space it runs in.
///
/// The task is the same task afterwards — same id, same descriptors, same
/// capabilities, same parent — which is what `execve` promises. What changes
/// is everything the old address space held, which is freed here, and the
/// program's identity: a new address space is a new program, so servers see
/// the caller become something else rather than carry on.
///
/// A program with more than one task has the others ended first, as POSIX
/// says (`end_siblings`). What they held in a server is the old program's,
/// and the old program is gone the moment the caller leaves it: whoever
/// watched it is told, and lets go of it, as when a program ends.
///
/// Does not return on success.
pub fn exec_into(cr3: usize, entry: u64, rsp: u64) -> Result<(), ()> {
    let caller = current_tid();
    let old_cr3 = crate::paging::read_cr3();
    if cr3 == 0 || cr3 == old_cr3 || !crate::userspace::is_owned_address_space(caller, cr3) {
        return Err(());
    }
    // Both a program's, or the way into it faults in the kernel: an `iretq`
    // to an address that is not canonical faults in ring 0, after `swapgs`,
    // on any processor — and a program could ask for one, and halt the
    // machine.
    let users = crate::paging::USER_MIN_ADDR..crate::paging::USER_ADDR_LIMIT;
    if !users.contains(&entry) || !users.contains(&rsp) {
        return Err(());
    }
    let space = crate::userspace::space_of(cr3);
    if space == 0 || space_in_use(space) {
        return Err(());
    }
    // What it held as the program it was, it holds no longer; and it is
    // called by the new one's name.
    crate::threads::exec(caller);
    let old_space = {
        let flags = irq_save();
        let s = unsafe { (*slot(caller)).as_ref().map(|t| t.space) };
        irq_restore(flags);
        s.unwrap_or(0)
    };
    if space_task_count(old_space) > 1 {
        let flags = irq_save();
        unsafe { end_siblings(caller, old_space) };
        irq_restore(flags);
    }

    {
        let flags = irq_save();
        unsafe {
            if let Some(t) = (*slot(caller)).as_mut() {
                t.cr3 = cr3;
                t.space = space;
                // The new image has set no thread pointer and registered no
                // word to clear: both named memory that is about to go.
                t.fs_base = 0;
                t.gs_base = 0;
                t.clear_child_tid = 0;
                t.mem_pages = 0;
                crate::fpu::clean_into(&raw mut t.fpu);
            }
            // What this call checked was memory of the program it was.
            st(caller).npinned = 0;
            crate::signal::task_became(caller);
            // The program it was has no task now, and that is a program
            // gone: whoever watched it is told, as when a program's last
            // task dies. They were not. What a server kept for the old
            // program it kept until the machine was turned off — a lock, a
            // file a C library had open for the length of one call — and
            // the kernel went on counting the program as watched: a
            // hundred and twenty-eight commands filled the table, and from
            // then on no server could be told of any program ending.
            if old_space != 0 && !space_has_live_task(old_space) {
                crate::ipc::notify_space_watchers(old_space);
                crate::iommu::program_gone(old_space);
            }
        }
        crate::userspace::addrspace_ref(cr3);
        irq_restore(flags);
    }

    // The kernel's own mappings are in PML4[0] and the upper half, which every
    // address space shares, so this kernel stack stays where it is across the
    // switch — and the old space can then be freed out from under nothing.
    //
    // The thread pointer goes with the address space it pointed into. The
    // scheduler only loads FS on a switch, and there is no switch here: left
    // alone, the new program's first thread-local — which for a C library is
    // in its own startup — would read through an address the old program had
    // and this one has not.
    unsafe {
        crate::paging::write_cr3(cr3);
        crate::cpu::set_fs_base(0);
        crate::cpu::set_user_gs_base(0);
        crate::fpu::restore_clean();
    }
    if crate::userspace::addrspace_unref(old_cr3) {
        drop_unused_space(old_cr3);
    }
    // What the old program marked as its own business goes with it.
    crate::fdtable::close_on_exec(caller);
    unsafe {
        crate::syscall::enter_usermode(entry, rsp, 0);
    }
}

/// End every task of program `space` but `caller`: it is about to become
/// another program, and they were threads of this one.
///
/// Quietly. None of them is a child anybody collects — the kernel takes
/// each apart (`UNWAITED`) — and none is a death anybody is told of: the
/// program goes on, as what `caller` is about to be. If one of them is the
/// task the program began as, `caller` takes its place as the child of
/// whoever started the program, and a parent already waiting for it looks
/// again — a wait by process id finds `caller`, which has that id.
///
/// # Safety
/// Interrupts off.
unsafe fn end_siblings(caller: usize, space: u64) { unsafe {
    let live = |t: usize| matches!(*slot(t), Some(ref x) if x.space == space && x.state != TaskState::Dead);
    let began = tids().filter(|&t| t >= 1).find(|&t| t != caller && live(t) && st(t).process_id == crate::cap::endpoint_of(t));
    if let Some(first) = began {
        let parent = (*slot(first)).as_ref().map_or(0, |t| t.parent_tid);
        if let Some(me) = (*slot(caller)).as_mut() {
            me.parent_tid = parent;
        }
        if parent != 0 && st(parent).wait_blocked && st(parent).wait_result == 0 {
            st(parent).wait_again = true;
            unblock_task(parent);
        }
    }
    for t in tids().filter(|&t| t >= 1) {
        if t != caller && live(t) {
            st(t).unwaited = true;
            let _ = end_other(t, -9);
        }
    }
}}

/// Throw away an address space no task ever ran in.
///
/// The ordinary path frees one when its last task goes; a fork that fails
/// part-way has one nothing is using, and leaving it would leak every page the
/// copy had managed.
pub fn drop_unused_space(cr3: usize) {
    unsafe {
        crate::paging::destroy_address_space(cr3);
        crate::userspace::unregister_address_space(cr3);
    }
}

/// Start a task that is a copy of another, at the point that other one is.
fn start_forked(tid: usize, cr3: usize, frame: &crate::task::UserFrame) -> Result<(), ()> {
    if tid >= MAX_TASKS {
        return Err(());
    }
    let flags = irq_save();
    unsafe {
        let Some(task) = (*slot(tid)).as_mut() else {
            irq_restore(flags);
            return Err(());
        };
        if task.state != TaskState::Blocked {
            irq_restore(flags);
            return Err(());
        }
        task.cr3 = cr3;
        crate::userspace::addrspace_ref(cr3);

        // The frame goes where a system call's frame would be, because that is
        // what it is: the child returns from the call its parent is in.
        let top = (task.kernel_stack_base as usize + task.kernel_stack_size) & !0xF;
        let at = top - core::mem::size_of::<crate::task::UserFrame>();
        core::ptr::write(at as *mut crate::task::UserFrame, *frame);
        core::ptr::write((at - 8) as *mut u64, crate::task::task_exit_trampoline as u64);

        task.context.rip = crate::userspace::fork_return_trampoline as *const () as u64;
        task.context.rsp = (at - 8) as u64;
        task.context.rbp = 0;
        task.context.r12 = at as u64;
        task.context.r14 = cr3 as u64;
        task.state = TaskState::Ready;
        enqueue(tid);
    }
    irq_restore(flags);
    Ok(())
}
