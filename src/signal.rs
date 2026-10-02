//! Signals, as far as a program that forks and execs needs them.
//!
//! A signal is said to a *program*. What happens is the program's to choose,
//! per signal: nothing (ignored), the default, or a handler of its own.
//!
//! The default is the kernel's to carry out, and for nearly every signal it
//! is the end of the program, with the negated signal number as its status —
//! the number a fault already ends one with, so a parent reads both the same
//! way. A program that is not looking is ended all the same: this half needs
//! nothing from it.
//!
//! A handler is the program's to run, and the kernel never runs one. There
//! is no frame pushed on a stack here and nothing returned from. The signal
//! is recorded as waiting, a word in the program's own memory is set so that
//! its runtime finds out on its way out of whatever system call it makes
//! next, and — because a program waiting for a key is making no system call
//! — the three waits a program sits in at a prompt are ended early: a read
//! of a terminal, a poll, a sleep. Each answers [`INTERRUPTED`]. The runtime
//! takes what is waiting (`SYS_SIG_TAKE`) and calls the handlers itself.
//!
//! One signal ends one wait: the first to look. A wait that was ended and
//! goes back to waiting without having taken anything waits, so a program
//! that pays no attention to the answer is slowed by nothing.
//!
//! So a handler runs at a system-call boundary and nowhere else. A program
//! that has asked to handle a signal and then computes for ever without a
//! call is not interrupted by it.
//!
//! Four signals stop a program instead of ending it, and one starts it
//! again; that, and who a signal typed at a terminal is for, is `job.rs`.
//! What is decided here is only whether one of them does what it does: a
//! program can ignore or handle three of the four, and a group with nobody
//! to continue it is not stopped from a terminal at all.
//!
//! Two signals the kernel raises of its own accord, because nothing else
//! can: SIGALRM when a program's alarm is due ([`alarm`], [`tick`]), and
//! SIGCHLD for a program when a child of it ends ([`child_ended`]).
//!
//! What a program has said lives in its descriptor table's record
//! (`fdtable.rs`), for the reason its umask does: `fork` copies it and `exec`
//! keeps what is ignored — which is how a shell starts a background job that
//! Ctrl-C does not reach.

use crate::fdtable::{self, Disposition};
use crate::scheduler;
use crate::task::{FdKind, MAX_TASKS};

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

/// What a wait that a signal ended answers, where a count would be.
pub const INTERRUPTED: u64 = 0xFFFF_FFFD;

pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;
pub const SIGKILL: u8 = 9;
const SIGALRM: u8 = 14;
const SIGCHLD: u8 = 17;
pub const SIGCONT: u8 = 18;
pub const SIGSTOP: u8 = 19;
pub const SIGTSTP: u8 = 20;
pub const SIGTTIN: u8 = 21;
pub const SIGTTOU: u8 = 22;
const SIGURG: u8 = 23;
const SIGWINCH: u8 = 28;
/// The highest signal there is.
pub const NSIG: u8 = 64;

/// The signals that do nothing to a program that has said nothing: Linux's
/// list. SIGCONT is here because what it does — start a stopped program —
/// it has done by the time anybody asks what else.
fn harmless(signo: u8) -> bool {
    matches!(signo, SIGCHLD | SIGURG | SIGWINCH | SIGCONT)
}

/// The signals that stop a program that has said nothing.
fn stops(signo: u8) -> bool {
    matches!(signo, SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU)
}

/// `SYS_SIG_ACTION`: what the caller's program does about a signal, as a
/// number, set to `how` if that is one of the three.
pub fn action(tid: usize, signo: u64, how: u64) -> u64 {
    if signo == 0 || signo > NSIG as u64 {
        return u64::MAX;
    }
    let signo = signo as u8;
    let new = match how {
        0 => Some(Disposition::Default),
        1 => Some(Disposition::Ignore),
        2 => Some(Disposition::Catch),
        _ => None,
    };
    // Two signals are not a program's to refuse.
    if new.is_some_and(|d| d != Disposition::Default) && (signo == SIGKILL || signo == SIGSTOP) {
        return u64::MAX;
    }
    fdtable::sig_action(tid, signo, new).map_or(u64::MAX, |old| old as u64)
}

/// Raise `signo` for the program `tid` is a task of.
pub fn raise(tid: usize, signo: u8) -> Result<(), ()> {
    if signo == 0 || signo > NSIG || tid <= 1 || tid >= MAX_TASKS {
        return Err(());
    }
    // A stopped program is started by SIGCONT whatever it has said about
    // the signal: ignoring it does not keep a program stopped, and a handler
    // for it could not run until this had happened.
    if signo == SIGCONT {
        crate::job::resume(tid);
    }
    let said = if signo == SIGKILL {
        Disposition::Default
    } else {
        match fdtable::sig_post(tid, signo) {
            Some((Disposition::Catch, word)) => {
                tell(tid, word);
                wake(tid);
                return Ok(());
            }
            Some((said, _)) => said,
            // In no program: not started yet, or gone.
            None => return Err(()),
        }
    };
    if said == Disposition::Ignore || harmless(signo) {
        return Ok(());
    }
    if stops(signo) {
        // SIGSTOP stops. The other three are how a terminal stops a job,
        // and a job is stopped for somebody to start again: a group with
        // nobody to do that — a shell that runs its commands in its own
        // group, a login — is left running.
        if signo == SIGSTOP || !crate::job::orphaned(crate::job::pgid_of(tid)) {
            crate::job::stop(tid, signo);
            // If that was the caller's own program, this is where it stops.
            scheduler::stop_here();
        }
        return Ok(());
    }
    scheduler::end_program(tid, -(signo as i32))
}

/// Set the word the program asked to be told through.
///
/// It is in the program's memory, which is not the memory this is running
/// on: the caller is whoever raised the signal. So the page is found through
/// the program's own tables and written through the identity map. A word
/// that is not there to write — unmapped since it was named — is not written;
/// the signal is still waiting, and a wait it ends says so.
fn tell(tid: usize, word: usize) {
    if word == 0 || word & 3 != 0 || !crate::paging::user_range_ok(word & !0xFFF, 1) {
        return;
    }
    let cr3 = scheduler::task_cr3(tid);
    if cr3 == 0 {
        return;
    }
    let flags = irq_save();
    unsafe {
        let writable = crate::paging::walk_flags(cr3, word)
            .is_some_and(|f| f & crate::paging::USER != 0 && f & crate::paging::WRITABLE != 0);
        if writable {
            if let Some(phys) = crate::paging::translate(cr3, word) {
                if phys + 4 <= crate::paging::identity_end() {
                    core::ptr::write_volatile(phys as *mut u32, 1);
                }
            }
        }
    }
    irq_restore(flags);
}

/// End the waits of `tid`'s program that a signal ends: a task asleep or in
/// a poll, one reading a terminal, and one waiting for the other end of a
/// named pipe to be opened.
fn wake(tid: usize) {
    let mut tasks = [0usize; 16];
    let n = fdtable::tasks_of(tid, &mut tasks);
    for &t in &tasks[..n] {
        crate::ipc::wake_sleeper(t);
        crate::pty::interrupt(t);
        crate::pipe::interrupt(t);
    }
}

/// `SYS_SIG_ALARM`: have SIGALRM raised for `tid`'s program `first`
/// nanoseconds from now, and every `every` after that if that is not 0; no
/// time is no alarm. With `ask`, nothing is changed. Answers with how the
/// alarm stood before: the nanoseconds left of it, and what it repeated at.
/// `None` for a task in no program.
pub fn alarm(tid: usize, first: u64, every: u64, ask: bool) -> Option<(u64, u64)> {
    let new = if ask { None } else { Some((first, every)) };
    let now = crate::clock::now();
    let was = fdtable::alarm(tid, now, new);
    if was.is_some() && !ask && first != 0 {
        crate::clock::due(now.saturating_add(first));
    }
    was
}

/// The clock's part in that: SIGALRM for each program whose alarm is due at
/// `now`.
///
/// One at a time, each alarm seen to before its signal is raised, because
/// raising it may be the last thing this does: a program that has said
/// nothing about SIGALRM is ended by it, and if that is the program the
/// clock interrupted there is nothing to come back to. Whatever else was
/// due is still due the next time the clock looks.
pub fn alarms(now: u64) {
    while let Some(tid) = fdtable::alarm_due(now) {
        let _ = raise(tid, SIGALRM);
    }
}

/// A child of `parent` has ended: SIGCHLD for the parent's program, which
/// does nothing to one that has not asked to hear of it.
pub fn child_ended(parent: usize) {
    let _ = raise(parent, SIGCHLD);
}

/// Should the running task not wait, because a signal has arrived for a
/// handler to hear about? True once for each time one is raised: the wait
/// that asks is the wait that is ended.
pub fn interrupted(tid: usize) -> bool {
    fdtable::sig_interrupted(tid)
}

/// `SYS_SIG_TAKE`.
pub fn take(tid: usize, word: usize) -> u64 {
    fdtable::sig_take(tid, word)
}

/// A character typed at a terminal raised a signal: it is for the group in
/// front of the terminal.
///
/// That is what a terminal with a session has: a shell with job control puts
/// each job in a group of its own and says which is in front, and Ctrl-C is
/// for that one and not for the shell or for what it left running behind.
///
/// A terminal nobody has made the controlling terminal of a session has no
/// group in front of it, and the signal goes to every program that has the
/// terminal open. What keeps a shell alive under its own Ctrl-C there is
/// what does on Unix when a shell has no job control: the shell handles the
/// signal, what it runs in the background is started ignoring it, and what
/// it runs in the foreground is not.
pub fn from_terminal(pty: usize, signo: u8) {
    if let Some((session, front)) = crate::pty::job(pty) {
        if session != 0 {
            crate::job::raise_for_group(front, signo);
            return;
        }
    }
    let mut holders = [0usize; 32];
    let n = fdtable::holders(
        |kind| matches!(kind, FdKind::PtyEnd { pty: p, end: 1 } if *p == pty),
        &mut holders,
    );
    // The caller's own program last, if it is one of them: its default may be
    // the end of the caller, and the rest must have been told by then.
    let me = scheduler::current_tid();
    let mut mine = [0usize; 16];
    let own = fdtable::tasks_of(me, &mut mine);
    let is_mine = |tid: usize| mine[..own].contains(&tid);
    for &tid in holders[..n].iter().filter(|&&tid| !is_mine(tid)) {
        let _ = raise(tid, signo);
    }
    if let Some(&tid) = holders[..n].iter().find(|&&tid| is_mine(tid)) {
        let _ = raise(tid, signo);
    }
}
