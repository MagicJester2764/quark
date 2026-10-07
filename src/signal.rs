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
//! A handler is run one of two ways, and a program says which for each
//! signal.
//!
//! **It is told** ([`Disposition::Catch`]), and runs the handler itself. The
//! signal is recorded as waiting, a word in the program's own memory is set
//! so that its runtime finds out on its way out of whatever system call it
//! makes next, and — because a program waiting for a key is making no
//! system call — the waits a program sits in at a prompt are ended early: a
//! read of a terminal, a poll, a sleep. Each answers [`INTERRUPTED`]. The
//! runtime takes what is waiting (`SYS_SIG_TAKE`) and calls the handlers.
//! One signal ends one wait: the first to look. A wait that was ended and
//! goes back to waiting without having taken anything waits, so a program
//! that pays no attention to the answer is slowed by nothing. Such a
//! handler runs at a system-call boundary and nowhere else: a program that
//! computes for ever without a call is not interrupted. It is what a
//! program written for this system wants — a shell that would like to hear
//! of Ctrl-C when it next looks.
//!
//! **Or the kernel runs it** ([`Disposition::Run`]). A task of the program
//! is turned aside on its way out of the kernel — from a system call, an
//! interrupt, a fault, whichever comes first, and an interrupt comes within
//! a tick — to a place the program named, with a record on its stack of
//! where it was ([`Frame`]). When the handler has run, the program gives
//! the record back (`SYS_SIG_RETURN`) and is where it was. That is what a
//! program written for Unix means by a handler, and what interrupts one
//! that is computing. See [`deliver`].
//!
//! With the second comes a **mask**, which is each task's own
//! (`SYS_SIG_MASK`): the signals it holds back. A signal is run by a task
//! that does not hold it back, and waits while every task does — whatever
//! it would have done, the ending of the program included. A handler runs
//! with its own signal held back, and whatever else the program asked;
//! giving the record back puts the mask back as it was.
//!
//! What the kernel does *not* do for a handler it runs is keep the
//! floating-point registers: the place the program is entered saves and
//! restores them itself, in ring 3, where a state that is not one is the
//! program's fault and not the kernel's.
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

use crate::fdtable::{self, Disposition, Handler};
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

/// What a wait that a signal ended answers, where a count would be — and,
/// where the kernel ran a handler on the way out, a handler that did not
/// ask for what it cut short to be made again.
pub const INTERRUPTED: u64 = 0xFFFF_FFFD;
/// A wait a signal ended, and the handler the kernel ran on the way out
/// asked for what it cut short to be made again ([`RESTARTS`]): the call is
/// made again — unless it is one Unix never makes again, a sleep or a poll,
/// which say they were interrupted whatever the handler asked.
pub const RESTART: u64 = 0xFFFF_FFFC;
/// A wait a signal ended, and in the end nothing was run here: another task
/// took the signal first, or what it did was nothing. The call is made
/// again, and nobody is told of anything.
pub const AGAIN: u64 = 0xFFFF_FFFB;

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
pub const SIGWINCH: u8 = 28;
const SIGSEGV: u8 = 11;
pub const SIGSYS: u8 = 31;
/// The highest signal there is.
pub const NSIG: u8 = 64;

/// How a handler the kernel runs is to be run (`SYS_SIG_ACTION`, arg3): its
/// own signal is not held back while it runs; it is run once, and the
/// signal is then as if nothing had been said; it is run on the stack the
/// task named for the purpose.
pub const NODEFER: u8 = 1;
pub const RESETHAND: u8 = 2;
pub const ONSTACK: u8 = 4;
/// And a call it cuts short is to be made again ([`RESTART`]).
pub const RESTARTS: u8 = 8;

/// The two that are not a program's to hold back.
const UNBLOCKABLE: u64 = (1 << (SIGKILL - 1)) | (1 << (SIGSTOP - 1));

/// What `Frame::code` says of why: a program raised it (`value` is who),
/// the kernel did, or it is something the task itself did (`value` is the
/// address it faulted at). [`Info`] says more, and these are what a program
/// written before it was there reads.
const BY_PROGRAM: u64 = 0;
const BY_KERNEL: u64 = 1;
const BY_FAULT: u64 = 2;

/// What came with a signal: what a program is told of it in `siginfo_t` —
/// Linux's `si_code`, and the two words after it, as Linux lays them out.
/// It is the end of a handler's [`Frame`], and what `SYS_SIG_WAIT` writes
/// when it is asked for it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// Linux's `si_code`, sign-extended: [`SI_USER`] and the rest.
    pub code: i64,
    /// The process id that raised it in the low half and its user in the
    /// high: for SIGCHLD the child's; for a timer, its id and how many more
    /// times it fired while its signal was waiting.
    pub who: u64,
    /// What it carried: the value it was queued with, a timer's, a child's
    /// status, or the address a fault was at.
    pub value: u64,
}

impl Info {
    /// The kernel raised it, of its own accord.
    pub const KERNEL: Info = Info { code: SI_KERNEL, who: 0, value: 0 };

    /// Raised by task `tid`'s program, as `code` says, carrying `value`.
    pub fn from_task(tid: usize, code: i64, value: u64) -> Info {
        let pid = scheduler::pid_of(tid) & 0xFFFF_FFFF;
        let uid = scheduler::task_uid_gid(tid).map_or(0, |(uid, _)| uid) as u64;
        Info { code, who: pid | uid << 32, value }
    }

    /// What a frame said before there was this, and what `SYS_SIG_WAIT`
    /// says when not asked for more: who, with the top bit set, for a signal
    /// a program raised, and nothing for the kernel's.
    fn old_value(&self) -> u64 {
        if matches!(self.code, SI_USER | SI_QUEUE | SI_TKILL) {
            (self.who & 0xFFFF_FFFF) | 1 << 63
        } else {
            0
        }
    }
}

/// Linux's `si_code`s: raised by a program with `kill`, by the kernel, by a
/// program with a value, by a timer, by a program for one task.
pub const SI_USER: i64 = 0;
pub const SI_KERNEL: i64 = 0x80;
pub const SI_QUEUE: i64 = -1;
pub const SI_TIMER: i64 = -2;
pub const SI_TKILL: i64 = -6;
/// SIGCHLD's: the child exited, was ended by a signal, stopped, or was
/// continued — and the status is what it exited with or the signal.
pub const CLD_EXITED: i64 = 1;
pub const CLD_KILLED: i64 = 2;
pub const CLD_STOPPED: i64 = 5;
pub const CLD_CONTINUED: i64 = 6;
/// A fault's: nothing at the address, or something that may not be touched
/// so; an address not on its boundary, or one that could not be had; a
/// division by nought; an instruction that is not one.
pub const SEGV_MAPERR: i64 = 1;
pub const SEGV_ACCERR: i64 = 2;
pub const BUS_ADRALN: i64 = 1;
pub const BUS_ADRERR: i64 = 2;
pub const FPE_INTDIV: i64 = 1;
pub const ILL_ILLOPN: i64 = 2;
/// For 31: a system call a program's trap turned aside (Linux's
/// `SYS_USER_DISPATCH`), and the architecture its record says it was made
/// on (`AUDIT_ARCH_X86_64`).
pub const SYS_USER_DISPATCH: i64 = 2;
const AUDIT_ARCH_X86_64: u64 = 0xC000_003E;

/// The first real-time signal. From here up, one raised while one of its
/// number is waiting waits behind it, with what it carried, rather than
/// being the same one again — as many as [`QUEUE`] a program and
/// [`TQUEUE`] a task, beyond the first of each number. Below it a signal
/// waiting keeps what came with the raise that made it wait.
pub const SIGRTMIN: u8 = 32;
pub const QUEUE: usize = 64;
const TQUEUE: usize = 16;

/// Why a signal was not raised.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NotRaised {
    /// No such program, or no such signal.
    Nobody,
    /// A real-time signal with as many of it waiting as can.
    Full,
}

/// What came with each signal waiting, and the real-time signals waiting
/// behind one of their number, in the order they were raised: a program's
/// (`fdtable`) and each task's. Which signals are waiting is said by bits
/// kept beside this; this is only what each carries.
#[derive(Clone, Copy)]
pub struct Waiting<const N: usize> {
    info: [Info; 64],
    behind: [(u8, Info); N],
    queued: usize,
}

impl<const N: usize> Waiting<N> {
    /// All noughts, which nothing reads before it is written: so that the
    /// tables of these are the kernel's zeroed memory, and not its image.
    const NOTHING: Info = Info { code: 0, who: 0, value: 0 };
    pub const EMPTY: Self = Waiting { info: [Self::NOTHING; 64], behind: [(0, Self::NOTHING); N], queued: 0 };

    /// `signo` has been raised with `info`, and `already` one of its number
    /// was waiting. False if this one cannot wait: a real-time signal with
    /// as many behind as there is room for.
    pub fn put(&mut self, signo: u8, info: Info, already: bool) -> bool {
        if !already {
            self.info[signo as usize - 1] = info;
            return true;
        }
        if signo < SIGRTMIN {
            return true;
        }
        if self.queued == N {
            return false;
        }
        self.behind[self.queued] = (signo, info);
        self.queued += 1;
        true
    }

    /// The `signo` waiting is taken: what came with it, and whether another
    /// of its number has taken its place.
    pub fn take(&mut self, signo: u8) -> (Info, bool) {
        let i = signo as usize - 1;
        let info = self.info[i];
        match self.behind[..self.queued].iter().position(|b| b.0 == signo) {
            Some(at) => {
                self.info[i] = self.behind[at].1;
                self.behind.copy_within(at + 1..self.queued, at);
                self.queued -= 1;
                (info, true)
            }
            None => (info, false),
        }
    }

    /// Nothing is waiting.
    pub fn clear(&mut self) {
        self.queued = 0;
    }

    /// Timer `id`'s `signo` is waiting — the one waiting first if `first`
    /// says one is, or behind it — and its overruns are counted up by `by`.
    /// False if it is not waiting.
    pub fn bump_timer(&mut self, signo: u8, id: u64, first: bool, by: u64) -> bool {
        let mine = |info: &Info| info.code == SI_TIMER && info.who & 0xFFFF_FFFF == id;
        let at = if first && mine(&self.info[signo as usize - 1]) {
            Some(&mut self.info[signo as usize - 1])
        } else {
            self.behind[..self.queued].iter_mut().find(|b| b.0 == signo && mine(&b.1)).map(|b| &mut b.1)
        };
        match at {
            Some(info) => {
                let overruns = (info.who >> 32).saturating_add(by).min(i32::MAX as u64);
                info.who = id | overruns << 32;
                true
            }
            None => false,
        }
    }

    /// Nothing is waiting behind but signals in `set`.
    pub fn keep(&mut self, set: u64) {
        let mut kept = 0;
        for i in 0..self.queued {
            if set & 1 << (self.behind[i].0 - 1) != 0 {
                self.behind[kept] = self.behind[i];
                kept += 1;
            }
        }
        self.queued = kept;
    }

    /// No `signo` is waiting any more.
    pub fn forget(&mut self, signo: u8) {
        self.keep(!(1 << (signo - 1)));
    }
}

/// The registers of a task in ring 3, as a handler's record keeps them and
/// `SYS_SIG_RETURN` puts them back: this order is the ABI.
pub type Regs = [u64; 18];
pub const RAX: usize = 0;
pub const RBX: usize = 1;
pub const RCX: usize = 2;
pub const RDX: usize = 3;
pub const RSI: usize = 4;
pub const RDI: usize = 5;
pub const RBP: usize = 6;
pub const R8: usize = 7;
pub const R9: usize = 8;
pub const R10: usize = 9;
pub const R11: usize = 10;
pub const R12: usize = 11;
pub const RIP: usize = 15;
pub const RFLAGS: usize = 16;
pub const RSP: usize = 17;

/// What is put on a task's stack when the kernel runs a handler: why, and
/// everything needed to be where it was. The program is entered with RDI
/// pointing at it, and gives the same address to `SYS_SIG_RETURN`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Frame {
    pub signo: u64,
    pub code: u64,
    pub value: u64,
    /// The task's mask before the handler, which giving the frame back
    /// restores. A handler may change it here.
    pub mask: u64,
    /// Bit 0: this is on the stack named for handlers. Bits 8 to 31: the
    /// program's own bits that the handler was given with.
    pub flags: u64,
    /// The program's word for the handler, as it gave it.
    pub cookie: u64,
    pub regs: Regs,
    /// What came with it, of which `code` and `value` above say part: what
    /// a program written before this was here reads.
    pub info: Info,
}

/// What this module keeps about a task, in its record (`TaskRec::sig`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    /// Each task's mask: the signals it holds back.
    mask: u64,
    /// A mask to put back when the wait a task is in has ended: it put another
    /// on for the length of the wait (`SYS_SIG_MASK`, wait).
    restore: Option<u64>,
    /// The stack each task has named for handlers: where it is and how long.
    stack: (usize, usize),
    /// Signals raised for one task and no other ([`raise_task`]), and what
    /// came with each.
    tpending: u64,
    twaiting: Waiting<TQUEUE>,
    /// The signals each task is waiting to take rather than have run
    /// (`SYS_SIG_WAIT`).
    waitset: u64,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            mask: 0,
            restore: None,
            stack: (0, 0),
            tpending: 0,
            twaiting: Waiting::EMPTY,
            waitset: 0,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.sig,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


/// The signals that do nothing to a program that has said nothing: Linux's
/// list. SIGCONT is here because what it does — start a stopped program —
/// it has done by the time anybody asks what else.
pub(crate) fn harmless(signo: u8) -> bool {
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
    let old = fdtable::sig_action(tid, signo, new).map_or(u64::MAX, |old| old as u64);
    // What was waiting for it waits for nothing now: it is ignored, or what
    // it does is nothing.
    if new == Some(Disposition::Ignore) || (new == Some(Disposition::Default) && harmless(signo)) {
        forget(tid, signo);
    }
    old
}

/// `signo` is waiting nowhere in `tid`'s program.
fn forget(tid: usize, signo: u8) {
    let bit = 1u64 << (signo - 1);
    fdtable::sig_unhold(tid, signo);
    fdtable::each_task(tid, |t| unsafe {
        st(t).tpending &= !bit;
        st(t).twaiting.forget(signo);
    });
}

/// `SYS_SIG_ACTION` with 3: the caller's program has a handler for `signo`
/// that the kernel is to run, with `mask` held back besides while it runs,
/// and run as `flags` says. `cookie` is the program's word for it, which
/// the frame hands back: the kernel enters every handler at one place, and
/// that place needs to know which to call. Answers with what was said
/// before.
///
/// For signal 0, it is the program that is described, first: its handlers
/// are entered at `cookie`, and with bit 0 of `flags` a call a signal cuts
/// short answers as Unix would have it — [`INTERRUPTED`], [`RESTART`] or
/// [`AGAIN`] — where a program that has not said so is answered
/// [`INTERRUPTED`] whatever was run, as it always was. A C library says so
/// as it starts.
pub fn handle(tid: usize, signo: u64, mask: u64, flags: u64, cookie: u64) -> u64 {
    if signo == 0 {
        if cookie < crate::paging::USER_MIN_ADDR || cookie >= crate::paging::USER_ADDR_LIMIT || flags > 1 {
            return u64::MAX;
        }
        return if fdtable::sig_enter_at(tid, cookie as usize, flags & 1 != 0) { 0 } else { u64::MAX };
    }
    if signo > NSIG as u64 {
        return u64::MAX;
    }
    let signo = signo as u8;
    if signo == SIGKILL || signo == SIGSTOP {
        return u64::MAX;
    }
    if flags > u32::MAX as u64 || flags & 0xF0 != 0 || !fdtable::sig_has_entry(tid) {
        return u64::MAX;
    }
    let how = Handler { mask: mask & !UNBLOCKABLE, flags: flags as u32, cookie, entry: 0 };
    let old = fdtable::sig_handle(tid, signo, how).map_or(u64::MAX, |old| old as u64);
    // Raised while it was held back and nothing was said: somebody may be
    // able to run it now.
    prod(tid, signo);
    old
}

/// Raise `signo` for the program `tid` is a task of, as the kernel.
pub fn raise(tid: usize, signo: u8) -> Result<(), NotRaised> {
    raise_with(tid, signo, Info::KERNEL)
}

/// Raise `signo` for the program `tid` is a task of, with `info`: who
/// raised it, and what it carries.
pub fn raise_with(tid: usize, signo: u8, info: Info) -> Result<(), NotRaised> {
    if signo == 0 || signo > NSIG || tid <= 1 || tid >= MAX_TASKS {
        return Err(NotRaised::Nobody);
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
        match fdtable::sig_post(tid, signo, info) {
            Some(Ok((Disposition::Catch, word))) => {
                tell(tid, word);
                wake(tid);
                waiters(tid, signo);
                return Ok(());
            }
            Some(Ok((Disposition::Run, _))) => {
                // It waits for a task that does not hold it back, and one
                // that can be is made to look — or for one waiting to take
                // it, which is woken, or a reader of a signal descriptor.
                prod(tid, signo);
                waiters(tid, signo);
                crate::pollset::note_signals();
                return Ok(());
            }
            Some(Ok((said, _))) => said,
            Some(Err(())) => return Err(NotRaised::Full),
            // In no program: not started yet, or gone.
            None => return Err(NotRaised::Nobody),
        }
    };
    if said == Disposition::Ignore {
        return Ok(());
    }
    // Held back by every task of the program, or waited for by one: what it
    // does, it does when one of them lets it through — or it is taken.
    // Two cannot be held back.
    if signo != SIGKILL && signo != SIGSTOP && (held_by_all(tid, signo) || waited_for(tid, signo)) {
        if !fdtable::sig_hold(tid, signo, info) {
            return Err(NotRaised::Full);
        }
        waiters(tid, signo);
        crate::pollset::note_signals();
        return Ok(());
    }
    act(tid, signo).map_err(|()| NotRaised::Nobody)
}

/// Raise `signo` for task `t` and no other, with `info`. A handler for it
/// is run in that task; held back there, it waits there, whatever it would
/// do. What it does to a program that has said nothing, it does to the
/// whole of the program, as on Unix.
pub fn raise_task(t: usize, signo: u8, info: Info) -> Result<(), NotRaised> {
    if signo == 0 || signo > NSIG || t <= 1 || t >= MAX_TASKS {
        return Err(NotRaised::Nobody);
    }
    // Three are the program's whichever task they name.
    if signo == SIGKILL || signo == SIGSTOP || signo == SIGCONT {
        return raise_with(t, signo, info);
    }
    let bit = 1u64 << (signo - 1);
    let said = fdtable::sig_action(t, signo, None).ok_or(NotRaised::Nobody)?;
    let flags = irq_save();
    let (held, waiting) = unsafe { (st(t).mask & bit != 0, st(t).waitset & bit != 0) };
    irq_restore(flags);
    match said {
        // Told: the program runs it, from whichever task looks.
        Disposition::Catch => raise_with(t, signo, info),
        Disposition::Ignore => Ok(()),
        Disposition::Default if !held && !waiting => act(t, signo).map_err(|()| NotRaised::Nobody),
        _ => {
            let flags = irq_save();
            let put = unsafe {
                let ok = st(t).twaiting.put(signo, info, st(t).tpending & bit != 0);
                if ok {
                    st(t).tpending |= bit;
                }
                ok
            };
            irq_restore(flags);
            if !put {
                return Err(NotRaised::Full);
            }
            crate::pollset::note_signals();
            if waiting {
                crate::ipc::wake_sleeper(t);
            } else if !held {
                poke(&[t]);
            }
            Ok(())
        }
    }
}

/// Whether every task of `tid`'s program holds `signo` back.
fn held_by_all(tid: usize, signo: u8) -> bool {
    let bit = 1u64 << (signo - 1);
    let (mut tasks, mut holding) = (0, 0);
    fdtable::each_task(tid, |t| {
        tasks += 1;
        holding += (unsafe { st(t).mask } & bit != 0) as usize;
    });
    tasks != 0 && holding == tasks
}

/// Whether a task of `tid`'s program is waiting to take `signo`.
fn waited_for(tid: usize, signo: u8) -> bool {
    let bit = 1u64 << (signo - 1);
    let mut any = false;
    fdtable::each_task(tid, |t| any |= unsafe { st(t).waitset } & bit != 0);
    any
}

/// Wake whichever tasks of `tid`'s program are waiting to take `signo`.
fn waiters(tid: usize, signo: u8) {
    let bit = 1u64 << (signo - 1);
    fdtable::each_task(tid, |t| {
        if unsafe { st(t).waitset } & bit != 0 {
            crate::ipc::wake_sleeper(t);
        }
    });
}

/// Make a task of `tid`'s program that does not hold `signo` back look for
/// it: the kernel runs the handler in whichever of them leaves the kernel
/// next, and this is what makes one leave.
///
/// What [`poke`] does for one task, for every task of the program that lets
/// `signo` through, in one walk with interrupts off: the caller, if it is
/// one of them; else one running on another processor; else one in a wait
/// a signal ends.
fn prod(tid: usize, signo: u8) {
    let bit = 1u64 << (signo - 1);
    let open = |t: usize| unsafe { st(t).mask } & bit == 0;
    let me = scheduler::current_tid();
    let flags = irq_save();
    let (mut mine, mut elsewhere) = (false, None);
    fdtable::each_task(tid, |t| {
        if open(t) {
            mine |= t == me;
            elsewhere = elsewhere.or_else(|| scheduler::running_elsewhere(t));
        }
    });
    if !mine {
        match elsewhere {
            Some(cpu) => crate::smp::interrupt(cpu),
            None => {
                let mut ended = false;
                fdtable::each_task(tid, |t| ended = ended || (open(t) && end_wait(t)));
            }
        }
    }
    irq_restore(flags);
}

/// Make one of `tasks` leave the kernel, or ring 3, and so look at what is
/// waiting for it.
///
/// The caller itself, if it is one of them: it is on its way out. Else one
/// that is running on another processor, which is interrupted. Else one in
/// a wait that a signal ends, which is ended. Else nobody has to be told:
/// a task that is ready, or in a call that will return, finds it when it
/// gets there.
fn poke(tasks: &[usize]) {
    if tasks.contains(&scheduler::current_tid()) {
        return;
    }
    let flags = irq_save();
    let elsewhere = tasks.iter().find_map(|&t| scheduler::running_elsewhere(t));
    if let Some(cpu) = elsewhere {
        crate::smp::interrupt(cpu);
    }
    irq_restore(flags);
    if elsewhere.is_none() {
        for &t in tasks {
            if end_wait(t) {
                break;
            }
        }
    }
}

/// End the wait task `t` is in, if it is in one that a signal the kernel
/// runs a handler for ends: asleep or polling, reading or writing a
/// terminal, a pipe or a stream, reading a counter or a timer, waiting on a
/// futex or for a child, or for the other end of a named pipe. True if it
/// was in one. A call to a server is not one of these: woken with no reply,
/// it would fail.
pub fn end_wait(t: usize) -> bool {
    crate::ipc::wake_sleeper(t)
        || crate::pty::interrupt(t)
        || crate::pipe::interrupt(t)
        || fdtable::interrupt(t)
        || crate::futex::interrupt(t)
        || scheduler::interrupt_wait(t)
}

/// Do what `signo` does to a program that has said nothing about it.
fn act(tid: usize, signo: u8) -> Result<(), ()> {
    if harmless(signo) {
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
        // The page is reached by its frame, so one the program shares since
        // a fork is made its own first: the other side was not sent this.
        // With no memory to do that the word is not written, as below.
        let _ = crate::paging::own(cr3, word);
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
    fdtable::each_task(tid, |t| {
        crate::ipc::wake_sleeper(t);
        crate::pty::interrupt(t);
        crate::pipe::interrupt(t);
    });
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

/// The clock's part for timers (`ptimer.rs`): the signal of each timer of
/// each program due at `now`, one at a time, each re-armed before its
/// signal is raised, as an alarm's is — raising it may not return. A timer
/// whose last signal is still waiting raises no other: what is waiting
/// counts this firing, and those the clock did not look in time for, as
/// overruns.
pub fn timers(now: u64) {
    while let Some(due) = crate::ptimer::due(now) {
        let id = due.id as u64;
        let waiting = if due.task != 0 {
            task_timer_bump(due.task, due.signo, id, due.missed + 1)
        } else {
            fdtable::sig_timer_bump(due.tid, due.signo, id, due.missed + 1)
        };
        if waiting {
            continue;
        }
        let info = Info { code: SI_TIMER, who: id | due.missed.min(i32::MAX as u64) << 32, value: due.value };
        let _ = if due.task != 0 {
            raise_task(due.task, due.signo, info)
        } else {
            raise_with(due.tid, due.signo, info)
        };
    }
}

/// Timer `id`'s `signo`, raised for task `t` alone, is still waiting there:
/// it counts `by` more overruns. False if it is not.
fn task_timer_bump(t: usize, signo: u8, id: u64, by: u64) -> bool {
    if t >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let bumped = unsafe { st(t).twaiting.bump_timer(signo, id, st(t).tpending & 1 << (signo - 1) != 0, by) };
    irq_restore(flags);
    bumped
}

/// A child of `parent` has ended, stopped or been continued, as `info`
/// says: SIGCHLD for the parent's program, which does nothing to one that
/// has not asked to hear of it.
pub fn child_ended(parent: usize, info: Info) {
    let _ = raise_with(parent, SIGCHLD, info);
}

/// What SIGCHLD carries for task `child`, of which `code` says what
/// happened: its process id and user, and its status — what it exited
/// with, or the signal that ended, stopped or continued it.
pub fn child_info(child: usize, code: i64, status: u64) -> Info {
    Info::from_task(child, code, status)
}

/// Should the running task not wait, because a signal has arrived for a
/// handler to hear about? True once for each time one is raised: the wait
/// that asks is the wait that is ended.
pub fn interrupted(tid: usize) -> bool {
    // Told of one, which ends the first wait to ask and no other. Or there
    // is one for the kernel to run in this task — which it does as the task
    // leaves the call it is in, so the answer is yes until it has.
    fdtable::sig_interrupted(tid) || ends_wait(tid)
}

/// Whether a wait task `tid` is in, or is about to begin, is to end for a
/// signal of the kind the kernel runs a handler for: one is waiting to be
/// run in this task, or one has arrived that it is waiting to take. Asked
/// with interrupts off, in the same step as the wait's own question, so
/// that one raised a moment later finds the task parked and ends its wait.
pub fn ends_wait(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    ready(tid) || unsafe { st(tid).waitset } & pending_for(tid) != 0
}

/// Every signal waiting for task `tid`: its own, and its program's — what a
/// signal descriptor it reads would give it.
pub fn waiting_for(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    let flags = irq_save();
    let set = pending_for(tid);
    irq_restore(flags);
    set
}

/// A read of a signal descriptor read for `set` (`sigfd.rs`): as many of
/// `set` waiting for task `tid` — its own first, then its program's — as
/// whole records fit in `max_len` bytes at `ptr`, each taken, as Linux's
/// `signalfd_siginfo`. With `block` the first is waited for, as
/// `SYS_SIG_WAIT` waits; without, none waiting is would-block. Too little
/// room for one is refused.
pub fn read_for(tid: usize, set: u64, ptr: *mut u8, max_len: usize, block: bool) -> u64 {
    const RECORD: usize = 128;
    if tid >= MAX_TASKS || max_len < RECORD {
        return u64::MAX;
    }
    let set = set & !UNBLOCKABLE;
    let mut n = 0;
    while n + RECORD <= max_len {
        let Some((signo, info)) = take_one(tid, set) else {
            if n > 0 {
                break;
            }
            if !block {
                return crate::pipe::WOULD_BLOCK;
            }
            // Woken by one of `set` arriving, or ended by another signal
            // with a handler to run.
            let flags = irq_save();
            unsafe { st(tid).waitset = set };
            irq_restore(flags);
            let slept = crate::ipc::sys_recv_timeout(tid, u64::MAX);
            let flags = irq_save();
            unsafe { st(tid).waitset = 0 };
            let mine = pending_for(tid) & set != 0;
            irq_restore(flags);
            if matches!(slept, Err(crate::ipc::IpcError::Interrupted)) && !mine && ready(tid) {
                return INTERRUPTED;
            }
            continue;
        };
        let record = linux_record(signo, &info);
        {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::copy_nonoverlapping(record.as_ptr(), ptr.add(n), RECORD) };
        }
        n += RECORD;
    }
    n as u64
}

/// What a signal descriptor's reader is given for a signal: Linux's
/// `signalfd_siginfo`, 128 bytes — the signal, `si_code`, who raised it (a
/// timer's number and overruns, for a timer's), and what it carried, as
/// `ssi_int` and `ssi_ptr`, or for SIGCHLD as `ssi_status`.
fn linux_record(signo: u8, info: &Info) -> [u8; 128] {
    let mut r = [0u8; 128];
    let mut put = |at: usize, bytes: &[u8]| r[at..at + bytes.len()].copy_from_slice(bytes);
    put(0, &(signo as u32).to_le_bytes());
    put(8, &(info.code as i32).to_le_bytes());
    let (low, high) = (info.who as u32, (info.who >> 32) as u32);
    if info.code == SI_TIMER {
        put(24, &low.to_le_bytes());
        put(32, &high.to_le_bytes());
    } else {
        put(12, &low.to_le_bytes());
        put(16, &high.to_le_bytes());
    }
    if signo == SIGCHLD {
        put(40, &(info.value as u32).to_le_bytes());
    } else {
        put(44, &(info.value as u32).to_le_bytes());
        put(48, &info.value.to_le_bytes());
    }
    r
}

/// Whether task `tid` has something to do on its way out of the kernel: a
/// handler to run, or a signal it lets through that does what it does.
fn ready(tid: usize) -> bool {
    let mask = unsafe { st(tid).mask };
    (unsafe { st(tid).tpending } & !mask) != 0 || fdtable::sig_ready(tid, mask)
}

/// Every signal waiting for task `tid`: its own, and its program's.
fn pending_for(tid: usize) -> u64 {
    (unsafe { st(tid).tpending }) | fdtable::sig_pending_set(tid)
}

/// Whether the way out of the kernel has anything to do for task `tid`.
fn due(tid: usize) -> bool {
    (unsafe { st(tid).restore.is_some() }) || ready(tid)
}

/// The signals task `tid` holds back.
pub fn mask_of(tid: usize) -> u64 {
    if tid >= MAX_TASKS {
        return 0;
    }
    unsafe { st(tid).mask }
}

/// Task `tid` begins as `from` is: a thread of its program, or a forked
/// child. It holds back what its maker does.
pub fn task_like(tid: usize, from: usize) {
    if tid < MAX_TASKS && from < MAX_TASKS {
        unsafe { st(tid).mask = st(from).mask };
    }
}

/// Task `tid` has become another program: what it holds back it still
/// does, and the stack it named was the old program's memory.
pub fn task_became(tid: usize) {
    if tid < MAX_TASKS {
        unsafe {
            st(tid).restore = None;
            st(tid).stack = (0, 0);
        }
    }
}

/// `SYS_SIG_MASK`: change what the caller holds back. `how` 0 adds `set`
/// to it, 1 takes `set` from it, 2 makes it `set`; anything else changes
/// nothing. Answers with what it was. With 3, it is made `set` and the
/// caller waits until a signal arrives that it is then not holding back —
/// and is put back as it was when the wait is over, or when the handler
/// that ended it has run. With 4, nothing changes and the answer is what is
/// waiting that the caller holds back.
pub fn mask(tid: usize, how: u64, set: u64) -> u64 {
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    if how == 4 {
        let flags = irq_save();
        let waiting = pending_for(tid) & unsafe { st(tid).mask };
        irq_restore(flags);
        return waiting;
    }
    let set = set & !UNBLOCKABLE;
    let flags = irq_save();
    let old = unsafe {
        let old = st(tid).mask;
        match how {
            0 => st(tid).mask = old | set,
            1 => st(tid).mask = old & !set,
            2 => st(tid).mask = set,
            3 => {
                st(tid).restore = Some(old);
                st(tid).mask = set;
            }
            _ => {}
        }
        old
    };
    irq_restore(flags);
    if how == 3 {
        // A sleep with no end, which a signal this task does not hold
        // back ends — looked for before it sleeps, with interrupts off.
        let _ = crate::ipc::sys_recv_timeout(tid, u64::MAX);
        return INTERRUPTED;
    }
    old
}

/// `SYS_SIG_WAIT`: take one of `set` that is waiting for the caller, or that
/// arrives within `span` (a span; nought is not to wait, `u64::MAX` for
/// ever), without running its handler — a program that waits for signals
/// this way holds them back. Answers with the signal, having written who
/// raised it to `at` if that is not nought (a process id, with the top bit
/// set for a program) — or, with `whole`, everything that came with it, an
/// [`Info`] — or 0 if the time ran out, or [`INTERRUPTED`] when another
/// signal ended the wait with a handler run.
pub fn wait_for(tid: usize, set: u64, span: u64, at: u64, whole: bool) -> u64 {
    let size = if whole { core::mem::size_of::<Info>() as u64 } else { 8 };
    if tid >= MAX_TASKS || (at != 0 && !crate::syscall::validate_user_ptr_mut(at, size)) {
        return u64::MAX;
    }
    let set = set & !UNBLOCKABLE;
    let deadline = if span == u64::MAX { u64::MAX } else { crate::clock::after(crate::clock::span(span)) };
    loop {
        if let Some((signo, info)) = take_one(tid, set) {
            if at != 0 {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    if whole {
                        core::ptr::write_unaligned(at as *mut Info, info);
                    } else {
                        core::ptr::write_unaligned(at as *mut u64, info.old_value());
                    }
                }
            }
            return signo as u64;
        }
        let now = crate::clock::now();
        if span == 0 || now >= deadline {
            return 0;
        }
        let flags = irq_save();
        unsafe { st(tid).waitset = set };
        irq_restore(flags);
        // Ended by one of `set` arriving, by the time, or by another signal
        // with a handler to run, which is an interruption.
        let slept = crate::ipc::sys_recv_timeout(tid, deadline.saturating_sub(now).max(1));
        let flags = irq_save();
        unsafe { st(tid).waitset = 0 };
        let mine = pending_for(tid) & set != 0;
        irq_restore(flags);
        if matches!(slept, Err(crate::ipc::IpcError::Interrupted)) && !mine && ready(tid) {
            return INTERRUPTED;
        }
    }
}

/// Take the lowest of `set` waiting for task `tid`, its own first: the
/// signal and what came with it.
fn take_one(tid: usize, set: u64) -> Option<(u8, Info)> {
    let flags = irq_save();
    let own = unsafe { st(tid).tpending } & set;
    let out = if own != 0 {
        let signo = own.trailing_zeros() as u8 + 1;
        Some((signo, unsafe { take_own(tid, signo) }))
    } else {
        fdtable::sig_take_one(tid, set)
    };
    irq_restore(flags);
    out
}

/// Take the `signo` raised for task `tid` alone: what came with it. Another
/// of its number behind it is waiting now in its place. Interrupts are off.
unsafe fn take_own(tid: usize, signo: u8) -> Info {
    unsafe {
        let (info, more) = st(tid).twaiting.take(signo);
        if !more {
            st(tid).tpending &= !(1 << (signo - 1));
        }
        info
    }
}

/// `SYS_SIG_STACK`: the stack the caller wants handlers that ask for one
/// run on, `size` bytes from `base`; no size is none, and a `base` of
/// `u64::MAX` changes nothing. The one it replaces is written to `old`, if
/// that is not nought: where, and how long.
pub fn stack(tid: usize, base: u64, size: u64, old: u64) -> u64 {
    if tid >= MAX_TASKS || (old != 0 && !crate::syscall::validate_user_ptr_mut(old, 16)) {
        return u64::MAX;
    }
    let change = base != u64::MAX;
    if change
        && size != 0
        && (base < crate::paging::USER_MIN_ADDR
            || base.checked_add(size).is_none_or(|end| end > crate::paging::USER_ADDR_LIMIT)
            || size < 2048)
    {
        return u64::MAX;
    }
    let flags = irq_save();
    let was = unsafe { st(tid).stack };
    if change {
        unsafe { st(tid).stack = if size == 0 { (0, 0) } else { (base as usize, size as usize) } };
    }
    irq_restore(flags);
    if old != 0 {
        let _ua = crate::cpu::UserAccess::begin();
        unsafe { core::ptr::write_unaligned(old as *mut [u64; 2], [was.0 as u64, was.1 as u64]) };
    }
    0
}

/// The calling task holds back exactly `set` for the rest of the call it is
/// in — a wait — and what it held back before once the call is over, a
/// handler the new mask let through having run first. Put on as the call
/// begins, which is the same step as its wait beginning: a signal let
/// through a moment later finds it waiting.
pub fn wait_under(tid: usize, set: u64) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let old = st(tid).mask;
        st(tid).restore = Some(st(tid).restore.unwrap_or(old));
        st(tid).mask = set & !UNBLOCKABLE;
    }
    irq_restore(flags);
}

#[inline(always)]
fn cli() {
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
}

#[inline(always)]
fn sti() {
    unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
}

/// The next handler to run for task `tid` on its way out, taken: the
/// signal, how to run its handler, and what came with it. The task's own
/// signals first, then its program's. What is let through and has no
/// handler to run does what it does on the way — which may be the end of
/// the program, so that this does not return, or stop it here until it is
/// continued.
fn take_next(tid: usize) -> Option<(u8, Handler, Info)> {
    loop {
        let flags = irq_save();
        let mask = unsafe { st(tid).mask };
        let own = unsafe { st(tid).tpending } & !mask;
        let mine = (own != 0).then(|| {
            let signo = own.trailing_zeros() as u8 + 1;
            (signo, unsafe { take_own(tid, signo) })
        });
        irq_restore(flags);
        if let Some((signo, info)) = mine {
            match fdtable::sig_action(tid, signo, None) {
                Some(Disposition::Run) => {
                    if let Some(how) = fdtable::sig_run_begin(tid, signo) {
                        return Some((signo, how, info));
                    }
                }
                Some(Disposition::Catch) => {
                    let _ = raise_with(tid, signo, info);
                }
                Some(Disposition::Default) => {
                    let _ = act(tid, signo);
                }
                _ => {}
            }
            continue;
        }
        if let Some(signo) = fdtable::sig_held_take(tid, mask) {
            let _ = act(tid, signo);
            continue;
        }
        return fdtable::sig_run_take(tid, mask);
    }
}

/// A mask put on for the length of a wait comes off: nothing was run that
/// would have put it back.
fn settle(tid: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(m) = st(tid).restore.take() {
            st(tid).mask = m;
        }
    }
    irq_restore(flags);
}

/// Run the handler for `signo` in the current task, `tid`: a [`Frame`] on
/// its stack, and `regs` changed to enter the program there. A stack that
/// cannot take the frame is the end of the program, as running out of
/// stack is.
///
/// It may wait: the stack's page may have to be read back in.
fn run(tid: usize, regs: &mut Regs, signo: u8, how: Handler, info: Info) {
    // The mask to go back to afterwards: the one a wait replaced, if this
    // ends that wait, and otherwise the one it has.
    let flags = irq_save();
    let before = unsafe { st(tid).restore.take().unwrap_or(st(tid).mask) };
    irq_restore(flags);
    let old = info.old_value();
    let (code, value) = if old >> 63 != 0 { (BY_PROGRAM, old & !(1 << 63)) } else { (BY_KERNEL, old) };
    if !push(tid, regs, signo, code, value, before, how, info) {
        let _ = scheduler::end_program(tid, -(SIGSEGV as i32));
    }
}

/// Write the record for `signo` on task `tid`'s stack and point `regs` at
/// the program's handler. False if the stack cannot be written.
#[allow(clippy::too_many_arguments)]
fn push(tid: usize, regs: &mut Regs, signo: u8, code: u64, value: u64, before: u64, how: Handler, info: Info) -> bool {
    let size = core::mem::size_of::<Frame>();
    let sp = regs[RSP] as usize;
    let (base, len) = unsafe { st(tid).stack };
    let on_it = len != 0 && sp > base && sp <= base + len;
    // Below the 128 bytes a function may be using under its stack pointer —
    // or at the top of the stack named for this, if the handler asks for
    // that and the task is not on it already.
    let (top, on_stack) = if how.flags & ONSTACK as u32 != 0 && len != 0 && !on_it {
        (base + len, true)
    } else {
        (sp.saturating_sub(128), on_it)
    };
    if top < size + 16 {
        return false;
    }
    let at = (top - size) & !0xF;
    // As a function finds its stack: a multiple of sixteen once the return
    // address is on it. There is no return address; the word is nought.
    let rsp = at - 8;
    let cr3 = crate::paging::read_cr3();
    let frame = Frame {
        signo: signo as u64,
        code,
        value,
        mask: before,
        flags: on_stack as u64 | (how.flags & !0xFF) as u64,
        cookie: how.cookie,
        regs: *regs,
        info,
    };
    // Brought in, and then looked at and written in one step: between the
    // two, with interrupts on, the page could be taken again by a program
    // short of memory.
    let mut written = false;
    for _ in 0..4 {
        if unsafe { crate::paging::back_range(cr3, rsp as u64, (size + 8) as u64, true) }.is_err() {
            return false;
        }
        let flags = irq_save();
        if unsafe { crate::paging::user_range_accessible(cr3, rsp as u64, (size + 8) as u64, true) } {
            {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    core::ptr::write_volatile(rsp as *mut u64, 0);
                    core::ptr::write_volatile(at as *mut Frame, frame);
                }
            }
            // What the handler runs with held back: what was, what the
            // program asked for, and the signal itself unless it asked for
            // that not to be.
            let own = if how.flags & NODEFER as u32 != 0 { 0 } else { 1u64 << (signo - 1) };
            unsafe { st(tid).mask = (st(tid).mask | how.mask | own) & !UNBLOCKABLE };
            written = true;
        }
        irq_restore(flags);
        if written {
            break;
        }
    }
    if !written {
        return false;
    }
    regs[RIP] = how.entry as u64;
    regs[RSP] = rsp as u64;
    regs[RDI] = at as u64;
    regs[RFLAGS] = clean_flags(regs[RFLAGS]);
    true
}

/// The flags a task goes to ring 3 with: the arithmetic ones as given,
/// interrupts on, and nothing else — not the direction flag, which a
/// function expects clear, nor the trap flag, nor leave to touch ports.
fn clean_flags(rflags: u64) -> u64 {
    (rflags & 0x8D5) | 0x202
}

/// The current task has faulted in ring 3 in a way that is signal `signo`
/// to a program on Unix — `code` is Linux's word for how, `addr` where. If
/// its program has a handler the kernel runs for that and the task is not
/// holding it back, `regs` is changed to run it and this is true. If not the
/// fault is the end of the program, as it always was: a fault that is held
/// back would only happen again.
pub fn fault(regs: &mut Regs, signo: u8, code: i64, addr: u64) -> bool {
    let tid = scheduler::current_tid();
    if tid == 0 || tid >= MAX_TASKS || mask_of(tid) & (1 << (signo - 1)) != 0 {
        return false;
    }
    let Some(how) = fdtable::sig_run_begin(tid, signo) else { return false };
    let info = Info { code, who: 0, value: addr };
    push(tid, regs, signo, BY_FAULT, addr, mask_of(tid), how, info)
}

/// A system call made from where the caller's program has said none is
/// (`SYS_SYSCALL_TRAP`; Linux's syscall user dispatch): it is not made.
/// SIGSYS is raised for the task instead, the way a fault raises its
/// signal — the handler entered at once, ahead of anything else waiting —
/// with every register in the record as it was at the call, the call's
/// number in RAX as Linux's record has it, and what came with it saying
/// where the call was made (the address of the `syscall`, in the word for
/// who) and which it was (its number in the low half of the value, the
/// architecture in the high, as `si_syscall` and `si_arch` lie). The
/// handler answers by writing RAX in the record, and `SYS_SIG_RETURN` puts
/// the task back after the call with that and the rest as they were — RDX
/// and R8 to R10 included, which a call of this kernel's leaves as nought
/// and one of Linux's leaves alone. A program with no handler the kernel
/// runs for it, or a task holding it back, is ended by it.
///
/// It is how a program built for Linux's musl, run on Quark's C library,
/// is answered when its own code makes a call rather than asking the C
/// library to: the library traps every call not made from its own code
/// and answers it as it answers its own.
///
/// `args` is what the call was given, RDI to R9 as Linux passes them.
/// Returns what the task leaves the call with on its way to the handler,
/// or None for a call that is to be made.
pub fn trap_call(nr: u64, args: [u64; 6]) -> Option<u64> {
    let tid = scheduler::current_tid();
    if tid == 0 || tid >= MAX_TASKS {
        return None;
    }
    let (from, to) = fdtable::trap_of(tid)?;
    let frame = scheduler::current_user_frame_mut()?;
    let at = frame.rip as usize;
    if at >= from && at < to {
        return None;
    }
    let mut regs: Regs = [0; 18];
    regs[RAX] = nr;
    regs[RBX] = frame.rbx;
    regs[RCX] = frame.rip;
    regs[RDX] = args[2];
    regs[RSI] = args[1];
    regs[RDI] = args[0];
    regs[RBP] = frame.rbp;
    regs[R8] = args[4];
    regs[R9] = args[5];
    regs[R10] = args[3];
    regs[R11] = frame.rflags;
    regs[R12] = frame.r12;
    regs[R12 + 1] = frame.r13;
    regs[R12 + 2] = frame.r14;
    regs[R12 + 3] = frame.r15;
    regs[RIP] = frame.rip;
    regs[RFLAGS] = frame.rflags;
    regs[RSP] = frame.rsp;
    let info = Info {
        code: SYS_USER_DISPATCH,
        who: frame.rip.wrapping_sub(2),
        value: (nr & 0xFFFF_FFFF) | AUDIT_ARCH_X86_64 << 32,
    };
    let mask = mask_of(tid);
    let pushed = mask & (1 << (SIGSYS - 1)) == 0
        && fdtable::sig_run_begin(tid, SIGSYS)
            .is_some_and(|how| push(tid, &mut regs, SIGSYS, BY_KERNEL, 0, mask, how, info));
    if !pushed {
        // The end of the program, and no return from that — but for the
        // first task, which nothing ends, and which is answered as a call
        // that failed.
        let _ = scheduler::end_program(tid, -(SIGSYS as i32));
        return Some(u64::MAX);
    }
    // Out to the handler, with its record, as `leaving_call` sends a task.
    frame.rip = regs[RIP];
    frame.rsp = regs[RSP];
    frame.rdi = regs[RDI];
    frame.rflags = regs[RFLAGS];
    Some(regs[RAX])
}

/// `SYS_SIG_RETURN`: the handler has run, and the task is to be where its
/// record says it was. The record is read from `at`, the mask it holds is
/// the task's again, and its registers are what the task goes back to ring
/// 3 with — as far as a program may say: an address in its own half, its
/// own flags. Does not return to the call.
pub fn ret(at: u64) -> ! {
    let tid = scheduler::current_tid();
    let size = core::mem::size_of::<Frame>() as u64;
    let cr3 = crate::paging::read_cr3();
    // Brought in, then looked at and read in one step, as `push` writes it.
    let mut read = None;
    for _ in 0..4 {
        if at % 8 != 0 || unsafe { crate::paging::back_range(cr3, at, size, false) }.is_err() {
            break;
        }
        let flags = irq_save();
        if unsafe { crate::paging::user_range_accessible(cr3, at, size, false) } {
            let _ua = crate::cpu::UserAccess::begin();
            read = Some(unsafe { core::ptr::read_volatile(at as *const Frame) });
        }
        irq_restore(flags);
        if read.is_some() {
            break;
        }
    }
    let Some(frame) = read else {
        let _ = scheduler::end_program(tid, -(SIGSEGV as i32));
        unreachable!("a program ended by its own call goes on")
    };
    let mut regs = frame.regs;
    if regs[RIP] >= crate::paging::USER_ADDR_LIMIT || regs[RSP] >= crate::paging::USER_ADDR_LIMIT {
        let _ = scheduler::end_program(tid, -(SIGSEGV as i32));
    }
    regs[RFLAGS] = clean_flags(regs[RFLAGS]);
    let flags = irq_save();
    unsafe {
        st(tid).mask = frame.mask & !UNBLOCKABLE;
        st(tid).restore = None;
    }
    irq_restore(flags);
    // Whatever that lets through is run now, before the task is back.
    loop {
        cli();
        if !due(tid) {
            break;
        }
        sti();
        match take_next(tid) {
            Some((signo, how, info)) => run(tid, &mut regs, signo, how, info),
            None => settle(tid),
        }
    }
    crate::syscall::enter_usermode_regs(&regs)
}

/// The current task is leaving a system call with `answer`: run a handler
/// first, if there is one to run — every one there is, each frame on the
/// last, so that the one run first is the last to have arrived. The
/// registers a call leaves a task with are the ones its stub kept, the
/// answer, and noughts.
///
/// A call a signal ended says what came of it, to a program that has said it
/// wants to know ([`handle`] for signal 0): [`INTERRUPTED`] if the first
/// handler run did not ask for its calls to be made again, [`RESTART`] if
/// it did, and [`AGAIN`] if nothing was run here — a signal another task
/// took, one that did nothing, a stop and a continue.
///
/// Returns with interrupts off, having found nothing more to do with them
/// off: a signal raised after that is seen by the processor the task goes
/// on, at its next tick or call.
pub fn leaving_call(answer: u64) -> u64 {
    let tid = scheduler::current_tid();
    if tid == 0 || tid >= MAX_TASKS {
        return answer;
    }
    let mut answer = answer;
    let mut first = true;
    let unix = answer == INTERRUPTED && fdtable::sig_unix(tid);
    loop {
        cli();
        // Nearly always nothing: asked first, and cheaply.
        if !due(tid) {
            if first && unix {
                answer = AGAIN;
            }
            return answer;
        }
        sti();
        let Some(frame) = scheduler::current_user_frame_mut() else {
            cli();
            return answer;
        };
        let Some((signo, how, info)) = take_next(tid) else {
            settle(tid);
            continue;
        };
        if first && unix {
            answer = if how.flags & RESTARTS as u32 != 0 { RESTART } else { INTERRUPTED };
        }
        first = false;
        let mut regs: Regs = [0; 18];
        regs[RAX] = answer;
        regs[RBX] = frame.rbx;
        regs[RCX] = frame.rip;
        regs[RSI] = frame.rsi;
        regs[RDI] = frame.rdi;
        regs[RBP] = frame.rbp;
        regs[R11] = frame.rflags;
        regs[R12] = frame.r12;
        regs[R12 + 1] = frame.r13;
        regs[R12 + 2] = frame.r14;
        regs[R12 + 3] = frame.r15;
        regs[RIP] = frame.rip;
        regs[RFLAGS] = frame.rflags;
        regs[RSP] = frame.rsp;
        run(tid, &mut regs, signo, how, info);
        frame.rip = regs[RIP];
        frame.rsp = regs[RSP];
        frame.rdi = regs[RDI];
        frame.rflags = regs[RFLAGS];
    }
}

/// The same, for a task going back to ring 3 from an interrupt or a fault.
/// Returns with interrupts off.
pub fn leaving_interrupt(frame: &mut crate::idt::InterruptFrame) {
    let tid = scheduler::current_tid();
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    loop {
        cli();
        if !due(tid) {
            return;
        }
        sti();
        match take_next(tid) {
            Some((signo, how, info)) => {
                let mut regs = regs_of(frame);
                run(tid, &mut regs, signo, how, info);
                enter(frame, &regs);
            }
            None => settle(tid),
        }
    }
}

/// The registers an interrupt found a task with.
pub fn regs_of(frame: &crate::idt::InterruptFrame) -> Regs {
    [
        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp, frame.r8, frame.r9,
        frame.r10, frame.r11, frame.r12, frame.r13, frame.r14, frame.r15, frame.rip, frame.rflags, frame.rsp,
    ]
}

/// Have the task an interrupt found go back to where `regs` says a handler
/// is entered, instead of where it was.
pub fn enter(frame: &mut crate::idt::InterruptFrame, regs: &Regs) {
    frame.rip = regs[RIP];
    frame.rsp = regs[RSP];
    frame.rdi = regs[RDI];
    frame.rflags = regs[RFLAGS];
}

/// `SYS_SIG_TAKE`. A real-time signal with another of its number behind
/// it is taken once and is still waiting, and the program is told so again.
pub fn take(tid: usize, word: usize) -> u64 {
    let (taken, again) = fdtable::sig_take(tid, word);
    if let Some(word) = again {
        tell(tid, word);
    }
    taken
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
    // Each program holding the slave, one at a time, as the signal may end
    // it; the caller's own program last, if it is one of them: its default
    // may be the end of the caller, and the rest must have been told by then.
    let holds = |kind: &FdKind| matches!(kind, FdKind::PtyEnd { pty: p, end: 1 } if *p == pty);
    let mine = fdtable::table_index(scheduler::current_tid());
    let mut own = None;
    let mut from = 0;
    while let Some((table, tid)) = fdtable::next_holder(from, holds) {
        from = table + 1;
        if table == mine {
            own = Some(tid);
        } else {
            let _ = raise(tid, signo);
        }
    }
    if let Some(tid) = own {
        let _ = raise(tid, signo);
    }
}
