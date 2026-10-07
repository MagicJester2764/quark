//! Jobs: process groups, sessions, and programs that stop.
//!
//! A shell runs a pipeline as one thing. It has to be able to say "that" —
//! the three programs of `a | b | c` — when Ctrl-C is typed, when Ctrl-Z is,
//! and when it wants them out of the way or back. So every process is in a
//! *process group*, and a signal can be raised for a group. A terminal has
//! one group that is in front, the *foreground*: what is typed at it goes
//! there, its interrupt and suspend characters are for there, and anybody
//! else who reads it is stopped until they are put in front.
//!
//! Groups belong to a *session*: everything started from one login. A
//! terminal is the controlling terminal of at most one session, and only
//! that session's groups can be in front of it.
//!
//! And a program can be *stopped*: every task of it held where it is until
//! SIGCONT, and its parent told, as it is told when a child ends.
//!
//! All of it is the kernel's because none of it can be anybody else's. Who
//! is in front of a terminal decides who a typed character is delivered to,
//! and the terminal is here. A program that is stopped is one the scheduler
//! does not run. And a parent finds out with the same call it collects a
//! dead child with.
//!
//! The numbers are process ids, which are never used twice: a group is named
//! by the id of the process that began it, and a session likewise. They are
//! kept by task, beside the process id and for the reason it is — a parent
//! asks about a child after the child's program has gone.

use crate::scheduler;
use crate::task::{TaskState, MAX_TASKS};

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

/// What this module keeps about a task, in its record (`TaskRec::job`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    /// The process group and the session the task's process is in.
    pgid: u64,
    sid: u64,
    /// The signal that stopped the program the task is of; 0 while it runs.
    stopped: u8,
    /// What a process's parent has not been told yet.
    report: u8,
    /// Its group is to be hung up on ([`hang_up`]).
    hang_up: bool,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            pgid: 0,
            sid: 0,
            stopped: 0,
            report: 0,
            hang_up: false,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.job,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


/// In a task's `report`, and what a wait asks to hear of.
pub const HAS_STOPPED: u8 = 1;
pub const HAS_CONTINUED: u8 = 2;

const SIGHUP: u8 = 1;
const SIGCONT: u8 = 18;

/// Why a change to a group or a session was refused.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// There is no such process, or it is not the caller's to move.
    NoSuch,
    /// There is, and the rules say no.
    NotAllowed,
}

/// A live task, as the scheduler has it: its process, its parent and its
/// program. `None` for a slot that is empty or a task that has died.
fn live(tid: usize) -> Option<(u64, usize, u64)> {
    match scheduler::task_info(tid) {
        Some((state, _, _, parent)) if state != TaskState::Dead => {
            Some((scheduler::pid_of(tid), parent, scheduler::space_of_task(tid)))
        }
        _ => None,
    }
}

/// A new task: it is in its creator's group and session. One made by nobody
/// who has either — the first — begins both.
///
/// Interrupts are off.
pub fn born(tid: usize, creator: usize, pid: u64) {
    if tid >= MAX_TASKS {
        return;
    }
    unsafe {
        let (group, session) = if creator < MAX_TASKS { (st(creator).pgid, st(creator).sid) } else { (0, 0) };
        st(tid).pgid = if group != 0 { group } else { pid };
        st(tid).sid = if session != 0 { session } else { pid };
        st(tid).stopped = 0;
        st(tid).report = 0;
    }
}

/// A task started in its creator's address space is a thread of the
/// creator's process, and is wherever that is.
pub fn joined(tid: usize, of: usize) {
    if tid >= MAX_TASKS || of >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        st(tid).pgid = st(of).pgid;
        st(tid).sid = st(of).sid;
        st(tid).stopped = st(of).stopped;
        st(tid).report = 0;
    }
    irq_restore(flags);
}

/// A task's slot is being given up.
///
/// Interrupts are off.
pub fn forget(tid: usize) {
    if tid < MAX_TASKS {
        unsafe {
            st(tid).pgid = 0;
            st(tid).sid = 0;
            st(tid).stopped = 0;
            st(tid).report = 0;
            st(tid).hang_up = false;
        }
    }
}

/// The process group `tid`'s process is in; 0 for no task.
pub fn pgid_of(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    unsafe { st(tid).pgid }
}

/// The session `tid`'s process is in; 0 for no task.
pub fn sid_of(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    unsafe { st(tid).sid }
}

/// Whether `tid`'s program is stopped.
pub fn is_stopped(tid: usize) -> bool {
    tid < MAX_TASKS && unsafe { st(tid).stopped != 0 }
}

/// The signal that stopped the program `tid` is a task of; 0 if it runs.
pub fn stopped_by(tid: usize) -> u8 {
    if tid < MAX_TASKS { unsafe { st(tid).stopped } } else { 0 }
}

/// Every task there is but the idle task and the first, which are nobody's
/// job: what the walks here go through.
fn tasks() -> impl Iterator<Item = usize> {
    scheduler::tids().filter(|&t| t >= 2)
}

/// Every live task of process `pid`, to `each`.
///
/// Interrupts are off.
fn each_task_of(pid: u64, mut each: impl FnMut(usize)) {
    for tid in tasks() {
        if matches!(live(tid), Some((p, _, _)) if p == pid) {
            each(tid);
        }
    }
}

/// The next process in group `pgid`, by its table's number at or past
/// `*from`, and a live task of it to raise a signal at; `*from` is moved past
/// it. One at a time, because a signal raised for one may end it, or the
/// caller.
pub fn next_member(pgid: u64, from: &mut usize) -> Option<usize> {
    if pgid == 0 {
        return None;
    }
    while let Some((table, first)) = crate::fdtable::next_program(*from) {
        *from = table + 1;
        let mut at = None;
        crate::fdtable::each_task(first, |t| {
            if at.is_none() && t >= 2 && unsafe { st(t).pgid } == pgid {
                at = Some(t);
            }
        });
        if at.is_some() {
            return at;
        }
    }
    None
}

/// Whether group `pgid` exists in session `sid`: some process is in it,
/// running or ended and not yet collected.
///
/// The second matters to a shell. It starts a pipeline one program at a
/// time and puts each in the group the first began, and the first may have
/// finished by then — `true | cat`. The group is there for as long as its
/// leader is there to be collected, which is what a shell that holds off
/// collecting until the pipeline is made is relying on.
pub fn group_in_session(pgid: u64, sid: u64) -> bool {
    if pgid == 0 {
        return false;
    }
    let flags = irq_save();
    let found = tasks()
        .any(|tid| scheduler::task_info(tid).is_some() && unsafe { st(tid).pgid == pgid && st(tid).sid == sid });
    irq_restore(flags);
    found
}

/// Whether task `tid` ties its group to the rest of its session: its parent
/// is another process, alive, in the same session and in a different group.
fn tied(tid: usize) -> bool {
    let Some((_, parent, space)) = live(tid) else { return false };
    if parent == 0 {
        return false;
    }
    let Some((_, _, parent_space)) = live(parent) else { return false };
    // A thread's parent is the task that made it, in the program they share.
    parent_space != space && unsafe { st(parent).sid == st(tid).sid && st(parent).pgid != st(tid).pgid }
}

/// Is group `pgid` orphaned: has it no member whose parent is in another
/// group of the same session?
///
/// It matters because of who would continue it. A job a shell started is
/// stopped by Ctrl-Z and started again by the shell — its parent, in the
/// same session, in a group of its own. A group with no such parent has
/// nobody to start it again, so the signals that would stop it from the
/// terminal are not allowed to: Ctrl-Z at a shell that runs its commands in
/// its own group does nothing, where it would otherwise stop the shell, the
/// command and the login that started them, for good.
pub fn orphaned(pgid: u64) -> bool {
    let flags = irq_save();
    let tied_in = tasks().any(|tid| unsafe { st(tid).pgid } == pgid && tied(tid));
    irq_restore(flags);
    !tied_in
}

/// `setpgid`: put process `pid` — the caller's own if that is 0 — in group
/// `pgid`, or in a new group of its own if that is 0 or its own id.
///
/// A process moves itself or a child of its own, within their session, into
/// a group of its own or one that is already there. A session's leader stays
/// where it is: its group is what the session is named after.
pub fn set_pgid(caller: usize, pid: u64, pgid: u64) -> Result<(), Refused> {
    let flags = irq_save();
    let out = (|| {
        let mine = scheduler::pid_of(caller);
        let pid = if pid == 0 { mine } else { pid };
        let target = tasks()
            .find(|&tid| matches!(live(tid), Some((p, _, _)) if p == pid))
            .ok_or(Refused::NoSuch)?;
        if pid != mine {
            // A child: some task of it was made by a task of the caller's.
            let is_child = tasks().any(|tid| {
                matches!(live(tid), Some((p, parent, _)) if p == pid && scheduler::pid_of(parent) == mine)
            });
            if !is_child {
                return Err(Refused::NoSuch);
            }
        }
        let (session, target_session) = unsafe { (st(caller).sid, st(target).sid) };
        if target_session != session || target_session == pid {
            return Err(Refused::NotAllowed);
        }
        let pgid = if pgid == 0 { pid } else { pgid };
        if pgid != pid && !group_in_session(pgid, session) {
            return Err(Refused::NotAllowed);
        }
        each_task_of(pid, |tid| unsafe { st(tid).pgid = pgid });
        Ok(())
    })();
    irq_restore(flags);
    out
}

/// `setsid`: the caller's process begins a session, and a group in it, both
/// named after itself. Returns the session.
///
/// Not for a process that already leads a group: the others in that group
/// would be left in a session their leader was not in.
pub fn set_sid(caller: usize) -> Result<u64, Refused> {
    let flags = irq_save();
    let pid = scheduler::pid_of(caller);
    let leads = pid == 0
        || tasks().any(|tid| live(tid).is_some() && unsafe { st(tid).pgid } == pid);
    let out = if leads {
        Err(Refused::NotAllowed)
    } else {
        each_task_of(pid, |tid| unsafe {
            st(tid).pgid = pid;
            st(tid).sid = pid;
        });
        Ok(pid)
    };
    irq_restore(flags);
    out
}

/// Stop the program `tid` is a task of: none of its tasks runs until it is
/// continued. Its parent is told, and woken if it was waiting to hear.
///
/// A program already stopped stays as it is, and is not reported again.
///
/// The caller may be one of the tasks stopped — a program that stops itself,
/// or reads a terminal it is not in front of. It is held like the rest and
/// goes on running only until it next gives up the processor, which is for
/// whoever called this to do once it has nothing left to finish
/// ([`scheduler::stop_here`]).
pub fn stop(tid: usize, signo: u8) {
    let flags = irq_save();
    let pid = scheduler::pid_of(tid);
    if pid != 0 && !is_stopped(tid) {
        each_task_of(pid, |t| {
            unsafe {
                st(t).stopped = signo;
                st(t).report = HAS_STOPPED;
            }
            scheduler::hold_task(t);
        });
        each_task_of(pid, |t| scheduler::child_changed(t, HAS_STOPPED));
    }
    irq_restore(flags);
}

/// Continue the program `tid` is a task of, if it was stopped: its tasks run
/// again from where they were, and its parent is told.
///
/// A task that was reading a terminal stops waiting and looks again, as it
/// would on its way back into a call a signal had interrupted: whether it is
/// in front of that terminal may have changed while it was stopped, and a
/// job put in the background must not go on to take what is typed.
pub fn resume(tid: usize) {
    let flags = irq_save();
    let pid = scheduler::pid_of(tid);
    if pid != 0 && is_stopped(tid) {
        each_task_of(pid, |t| {
            unsafe {
                st(t).stopped = 0;
                st(t).report = HAS_CONTINUED;
            }
            scheduler::release_task(t);
        });
        each_task_of(pid, |t| {
            crate::pty::interrupt(t);
            scheduler::child_changed(t, HAS_CONTINUED);
        });
    }
    irq_restore(flags);
}

/// What the parent of `tid`'s process has not heard, of the kinds in `want`:
/// the signal that stopped it, or 0 for having been continued. It has been
/// told once this returns.
///
/// Interrupts are off.
pub fn take_report(tid: usize, want: u8) -> Option<u8> {
    if tid >= MAX_TASKS {
        return None;
    }
    let report = unsafe { st(tid).report } & want;
    if report == 0 {
        return None;
    }
    let (pid, signo) = (scheduler::pid_of(tid), unsafe { st(tid).stopped });
    each_task_of(pid, |t| unsafe { st(t).report = 0 });
    Some(if report & HAS_STOPPED != 0 { signo } else { 0 })
}

/// Raise `signo` for every process in group `pgid`, the caller's own last:
/// its default may be the end of the caller, or a stop, and the rest must
/// have been told by then. Returns how many were told.
pub fn raise_for_group(pgid: u64, signo: u8) -> usize {
    let mine = scheduler::pid_of(scheduler::current_tid());
    let mut told = 0;
    let mut own = None;
    let mut from = 0;
    while let Some(tid) = next_member(pgid, &mut from) {
        if scheduler::pid_of(tid) == mine {
            own = Some(tid);
        } else if crate::signal::raise(tid, signo).is_ok() {
            told += 1;
        }
    }
    if let Some(tid) = own {
        if crate::signal::raise(tid, signo).is_ok() {
            told += 1;
        }
    }
    told
}

/// A group is to be hung up on: orphaned, with something stopped in it.
/// Which groups is marked on their tasks (`PerTask::hang_up`), so that
/// nothing has to be found room for at a death.
static mut HANGUPS: bool = false;

/// The last task of a process has died. `tid` is that task, already marked
/// dead.
///
/// Two things are tidied. A terminal whose session this process led is
/// nobody's controlling terminal any more, and can be taken by the next. And
/// a group this death has orphaned, with a member that is stopped, is to be
/// hung up on and continued — there is nobody left who would have continued
/// it, and a stopped program nobody can start is a task slot lost for good.
///
/// The hanging up is not done here. This runs in the middle of something
/// being ended, by whoever is ending it, and a hangup ends programs: one of
/// them may be the caller's own, and then nothing after it would be done —
/// the rest of the program it was ending included. The group is written
/// down, and the next tick sees to it ([`hang_up`]).
///
/// Interrupts are off.
pub fn process_ended(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let pid = scheduler::pid_of(tid);
    let (group, session) = unsafe { (st(tid).pgid, st(tid).sid) };
    if pid != 0 && session == pid {
        crate::pty::session_gone(session);
    }
    // The groups this process tied to its session: its own, if its parent
    // was the tie, and each child's, if it was theirs.
    let check = |g: u64| unsafe {
        let tied_still = tasks().any(|t| st(t).pgid == g && tied(t));
        let has_stopped = tasks().any(|t| live(t).is_some() && st(t).pgid == g && st(t).stopped != 0);
        if g != 0 && !tied_still && has_stopped {
            for t in tasks().filter(|&t| st(t).pgid == g) {
                st(t).hang_up = true;
            }
            *core::ptr::addr_of_mut!(HANGUPS) = true;
        }
    };
    if let Some((_, _, _, parent)) = scheduler::task_info(tid) {
        let was_tie = parent != 0
            && live(parent).is_some()
            && unsafe { st(parent).sid == session && st(parent).pgid != group };
        if was_tie {
            check(group);
        }
    }
    for child in tasks() {
        let Some((_, parent, _)) = live(child) else { continue };
        if parent == tid && unsafe { st(child).sid == session && st(child).pgid != group } {
            check(unsafe { st(child).pgid });
        }
    }
}

/// Hang up on the groups written down for it: SIGHUP for every process in
/// one, and SIGCONT, so that a stopped one hears it. Called on every tick.
///
/// One group at a time, each taken off the list before anything is raised,
/// and the running program's own signal last — for the reason an alarm is
/// seen to before it is raised: this may be the last thing that is done
/// here. What is still on the list is still there at the next tick.
pub fn hang_up() {
    if !unsafe { *core::ptr::addr_of!(HANGUPS) } {
        return;
    }
    loop {
        // A group marked, its marks taken off before anything is raised.
        let flags = irq_save();
        let group = unsafe {
            let group = tasks().find(|&t| st(t).hang_up).map(|t| st(t).pgid);
            match group {
                Some(g) => {
                    for t in tasks().filter(|&t| st(t).pgid == g) {
                        st(t).hang_up = false;
                    }
                }
                None => *core::ptr::addr_of_mut!(HANGUPS) = false,
            }
            group
        };
        irq_restore(flags);
        let Some(group) = group else { return };
        let mine = scheduler::pid_of(scheduler::current_tid());
        let mut own = None;
        let mut from = 0;
        while let Some(tid) = next_member(group, &mut from) {
            if scheduler::pid_of(tid) == mine {
                own = Some(tid);
            } else {
                let _ = crate::signal::raise(tid, SIGHUP);
                let _ = crate::signal::raise(tid, SIGCONT);
            }
        }
        // Running, so not stopped, and there is nothing to continue.
        if let Some(tid) = own {
            let _ = crate::signal::raise(tid, SIGHUP);
        }
    }
}
