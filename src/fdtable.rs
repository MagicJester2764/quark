//! Descriptor tables: one for each program.
//!
//! A descriptor belongs to a program, not to a task. Every thread of a program
//! uses one table, so a file one thread opens is the file its siblings read,
//! and a pipe end one of them closes is closed. It used to be a table per
//! task, with a thread starting on a *copy* of its creator's: what either was
//! given or closed afterwards the other never saw, and a pipe end that was
//! open when a thread started stayed open until that thread went.
//!
//! The table is here rather than in the task for that reason, and because it
//! has to outlive whichever task happened to be first: a program whose first
//! thread exits is still a program. A table goes when the last task using it
//! does, and that is when what it named is let go.
//!
//! One slot past the ordinary ones, `FD_CWD`, holds the program's working
//! directory. It is a descriptor like the rest — copied by `fork`, kept by
//! `exec`, handed to a child by its spawner — so that where a program *is*
//! follows it the way what it has *open* does, and no server has to be told
//! that one program became another.
//!
//! Sharing is what makes every operation here take the lock. With a table per
//! task, only that task changed it. Now a sibling can be preempted half way
//! through finding a free slot, so "find a free slot and fill it" is one step
//! with interrupts off, and a task about to block on what a descriptor names
//! takes a reference of its own first (`hold`) — or a sibling closing that
//! descriptor would free the object under it, and the next thing to take the
//! slot would be read by a task that never held it.

use crate::signal::{Info, Waiting};
use crate::task::{FdKind, MAX_FDS, MAX_TASKS};

/// The working directory's slot: a descriptor number one past the last
/// ordinary one. It can be copied to and from and asked about; it is never
/// read, written or waited on, and no allocation ever chooses it.
pub const FD_CWD: usize = MAX_FDS;
/// Every slot of a table, the working directory included.
pub const SLOTS: usize = MAX_FDS + 1;

const NONE: u16 = u16::MAX;

struct Table {
    /// Tasks using this table. Zero is a free table.
    tasks: u16,
    fds: [FdKind; SLOTS],
    /// One bit per slot: closed when the program becomes another (`exec`).
    cloexec: u128,
    /// The permission bits this program does not want on what it makes.
    ///
    /// The kernel makes no files and never reads this. It is here because it
    /// has to follow a program exactly as its descriptors do — copied by
    /// `fork`, kept by `exec` — and a C library's own memory does neither:
    /// `umask 077` in a shell has to be true of the `touch` it then runs.
    umask: u16,
    /// What the program has said to do about each signal, bit `n - 1` for
    /// signal `n`: leave it alone, or run a handler for it. Neither bit is
    /// the default. Here for the reason the umask is — a forked child has
    /// its parent's, and an exec keeps what is ignored — and because a
    /// signal is said to a program, which is what a table is.
    sig_ignore: u64,
    sig_catch: u64,
    /// The signals it has a handler for that the kernel is to run
    /// (`Disposition::Run`): for each, a bit here, and below what is held
    /// back while its handler runs and how it is to be run. One place in
    /// the program is entered for all of them (`sig_entry`).
    sig_run: u64,
    sig_masks: [u64; 64],
    sig_flags: [u32; 64],
    sig_cookies: [u64; 64],
    sig_entry: usize,
    /// It has said that a call a signal cuts short is to answer as Unix
    /// would have it (`signal::handle` for signal 0).
    sig_unix: bool,
    /// Signals with a handler that have been raised and not yet taken, or
    /// not yet run.
    sig_pending: u64,
    /// Signals the program has said nothing about that were raised while
    /// every task of it held them back: what each does, it does when one
    /// of them lets it through.
    sig_held: u64,
    /// One of those has not ended a wait yet. A signal ends one wait, the
    /// first to look: a wait that was ended and went back to waiting without
    /// taking anything is waiting for something else, and is left to.
    sig_interrupt: bool,
    /// Where in the program's own memory to say there is something to take:
    /// a word its runtime looks at on its way out of every system call. 0
    /// until it has said where.
    sig_word: usize,
    /// When SIGALRM is next raised for the program, in the clock's
    /// nanoseconds, 0 for never; and how long after that to raise it again,
    /// 0 for not at all. One for the program, so one for all its threads.
    /// `exec` keeps it, which is what lets a program be started with a time
    /// to finish in; `fork` does not copy it, since the child set no alarm.
    alarm_at: u64,
    alarm_every: u64,
    /// What the program's ended tasks used, and the children it collected
    /// (`usage.rs`).
    used_gone: crate::usage::Usage,
    used_children: crate::usage::Usage,
    /// How nice it is to the rest of its band, -20 to 19.
    nice: i8,
    /// How many seconds of processor time it may have: SIGXCPU past the
    /// first, the end at the second; `u64::MAX` for none. And the second
    /// it was last sent SIGXCPU for.
    cpu_soft: u64,
    cpu_hard: u64,
    xcpu_sent: u64,
    /// What the program was started as: its arguments, each ended by a
    /// nought, as much as fits — said by whoever started it, or by itself
    /// (`SYS_PROGRAM_NAME`). `fork` copies it; an `exec` is a new program,
    /// and the one exec'ing says what.
    cmdline: [u8; CMDLINE],
    cmdline_len: u8,
}

/// How much of a program's command line is kept.
pub const CMDLINE: usize = 128;

/// What a program that has said nothing leaves off: write for group and other.
const DEFAULT_UMASK: u16 = 0o022;

const EMPTY: Table = Table {
    tasks: 0,
    fds: [FdKind::Empty; SLOTS],
    cloexec: 0,
    umask: DEFAULT_UMASK,
    sig_ignore: 0,
    sig_catch: 0,
    sig_run: 0,
    sig_masks: [0; 64],
    sig_flags: [0; 64],
    sig_cookies: [0; 64],
    sig_entry: 0,
    sig_unix: false,
    sig_pending: 0,
    sig_held: 0,
    sig_interrupt: false,
    sig_word: 0,
    alarm_at: 0,
    alarm_every: 0,
    used_gone: crate::usage::Usage::ZERO,
    used_children: crate::usage::Usage::ZERO,
    nice: 0,
    cpu_soft: u64::MAX,
    cpu_hard: u64::MAX,
    xcpu_sent: u64::MAX,
    cmdline: [0; CMDLINE],
    cmdline_len: 0,
};

/// As many tables as tasks: each task uses exactly one.
static mut TABLES: [Table; MAX_TASKS] = [EMPTY; MAX_TASKS];
/// Which table each task uses.
static mut OF_TASK: [u16; MAX_TASKS] = [NONE; MAX_TASKS];
/// What came with each signal waiting for the program a table is, and the
/// real-time signals waiting behind one of their number: beside the tables
/// rather than in them, so that it is zeroed memory and not the kernel's
/// image.
static mut WAITING: [Waiting<{ crate::signal::QUEUE }>; MAX_TASKS] = [Waiting::EMPTY; MAX_TASKS];
/// What each task is in the middle of using, held so that it cannot go away.
static mut HELD: [FdKind; MAX_TASKS] = [FdKind::Empty; MAX_TASKS];

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

/// # Safety
/// Interrupts are off.
#[inline(always)]
unsafe fn tables() -> &'static mut [Table; MAX_TASKS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLES) }
}

/// The table `tid` uses.
///
/// # Safety
/// Interrupts are off.
unsafe fn table_mut(tid: usize) -> Option<&'static mut Table> {
    unsafe {
        if tid >= MAX_TASKS {
            return None;
        }
        let i = (*core::ptr::addr_of!(OF_TASK))[tid];
        if i == NONE { None } else { Some(&mut tables()[i as usize]) }
    }
}

/// The table `tid` uses, and what came with the signals waiting for it.
///
/// # Safety
/// Interrupts are off.
unsafe fn signals_mut(tid: usize) -> Option<(&'static mut Table, &'static mut Waiting<{ crate::signal::QUEUE }>)> {
    unsafe {
        if tid >= MAX_TASKS {
            return None;
        }
        let i = (*core::ptr::addr_of!(OF_TASK))[tid];
        if i == NONE {
            None
        } else {
            Some((&mut tables()[i as usize], &mut (*core::ptr::addr_of_mut!(WAITING))[i as usize]))
        }
    }
}

/// Give a new task an empty table of its own. False if it already has one.
pub fn attach_new(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let of = &mut *core::ptr::addr_of_mut!(OF_TASK);
        if of[tid] != NONE {
            false
        } else {
            // There is always one: a table per task, and this task has none.
            match tables().iter().position(|t| t.tasks == 0) {
                Some(i) => {
                    tables()[i] = EMPTY;
                    tables()[i].tasks = 1;
                    (*core::ptr::addr_of_mut!(WAITING))[i].clear();
                    crate::ptimer::clear(i);
                    of[tid] = i as u16;
                    true
                }
                None => false,
            }
        }
    };
    irq_restore(flags);
    ok
}

/// Which table a task uses, as a number two tasks of one program agree on.
/// `usize::MAX` for a task with none.
/// The word program `space` is told of signals through, or 0 if it has
/// named none. Interrupts must be off.
pub fn sig_word_of_space(space: u64) -> usize {
    let Some(tid) = crate::scheduler::task_of_space(space) else { return 0 };
    let slot = table_of(tid);
    unsafe { (*core::ptr::addr_of!(TABLES)).get(slot).map_or(0, |t| t.sig_word) }
}

pub fn table_of(tid: usize) -> usize {
    if tid >= MAX_TASKS {
        return usize::MAX;
    }
    let flags = irq_save();
    let i = unsafe { (*core::ptr::addr_of!(OF_TASK))[tid] };
    irq_restore(flags);
    if i == NONE { usize::MAX } else { i as usize }
}

/// A task using table `table`, if any does.
pub fn a_task_of(table: usize) -> Option<usize> {
    let flags = irq_save();
    let found = unsafe {
        (*core::ptr::addr_of!(OF_TASK)).iter().position(|&t| t != NONE && t as usize == table)
    };
    irq_restore(flags);
    found
}

/// Timer `id`'s `signo` is still waiting for `tid`'s program: it counts
/// `by` more overruns. False if it is not.
pub fn sig_timer_bump(tid: usize, signo: u8, id: u64, by: u64) -> bool {
    let bit = sig_bit(signo);
    let flags = irq_save();
    let bumped = unsafe {
        signals_mut(tid).is_some_and(|(t, w)| w.bump_timer(signo, id, (t.sig_pending | t.sig_held) & bit != 0, by))
    };
    irq_restore(flags);
    bumped
}

/// Leave the table `tid` uses. If no task uses it any more, what it held is
/// returned for the caller to release — outside the lock, since releasing a
/// pipe end wakes whoever was waiting on it.
fn leave(tid: usize) -> Option<[FdKind; SLOTS]> {
    if tid >= MAX_TASKS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let of = &mut *core::ptr::addr_of_mut!(OF_TASK);
        let i = of[tid];
        if i == NONE {
            None
        } else {
            of[tid] = NONE;
            let t = &mut tables()[i as usize];
            t.tasks = t.tasks.saturating_sub(1);
            if t.tasks == 0 {
                let fds = t.fds;
                *t = EMPTY;
                crate::ptimer::clear(i as usize);
                Some(fds)
            } else {
                None
            }
        }
    };
    irq_restore(flags);
    out
}

fn release_all(fds: &[FdKind; SLOTS]) {
    for kind in fds.iter() {
        if !kind.is_empty() {
            crate::pipe::release_fd(kind);
        }
    }
}

/// A task has died. It lets go of what it was in the middle of using and
/// leaves its table, and if it was the last task there, everything the table
/// named is released.
pub fn task_gone(tid: usize) {
    // If it died where it was waiting, it is still on that list, under an id
    // the next task will be given. Off it before the reference goes.
    if tid < MAX_TASKS {
        let flags = irq_save();
        let held = unsafe { (*core::ptr::addr_of!(HELD))[tid] };
        irq_restore(flags);
        if !held.is_empty() {
            crate::pipe::forget_waiter(&held, tid);
        }
    }
    unhold(tid);
    if let Some(fds) = leave(tid) {
        release_all(&fds);
    }
}

/// Make `tid` a thread of `with`'s program: it uses that table from now on.
///
/// The table it had — the empty one it was made with, or whatever its creator
/// put there — is given up, and what that held is released.
pub fn share(tid: usize, with: usize) -> bool {
    if tid >= MAX_TASKS || with >= MAX_TASKS || tid == with {
        return false;
    }
    let flags = irq_save();
    let target = unsafe { (*core::ptr::addr_of!(OF_TASK))[with] };
    if target == NONE {
        irq_restore(flags);
        return false;
    }
    if unsafe { (*core::ptr::addr_of!(OF_TASK))[tid] } == target {
        irq_restore(flags);
        return true;
    }
    // Joined before the old one is left, with the lock held across both, so
    // nothing sees the task with no table.
    unsafe { tables()[target as usize].tasks += 1 };
    let old = leave(tid);
    unsafe { (*core::ptr::addr_of_mut!(OF_TASK))[tid] = target };
    irq_restore(flags);
    if let Some(fds) = old {
        release_all(&fds);
    }
    true
}

/// Give `child`'s table a copy of everything in `parent`'s, as `fork` does:
/// a second descriptor for each object, the working directory and the
/// close-on-exec marks included. A descriptor that cannot be copied — a poll
/// set counts no holders — is left out.
pub fn copy_into(child: usize, parent: usize) {
    if child >= MAX_TASKS || parent >= MAX_TASKS {
        return;
    }
    // One step: a sibling of the parent closing a descriptor between its being
    // read and its being retained would have this retain something freed.
    let flags = irq_save();
    unsafe {
        let (src_fds, src_cloexec, src_umask, src_signals, src_run, src_name) = match table_mut(parent) {
            Some(t) => (
                t.fds,
                t.cloexec,
                t.umask,
                (t.sig_ignore, t.sig_catch, t.sig_word),
                (t.sig_run, t.sig_masks, t.sig_flags, t.sig_cookies, t.sig_entry, t.sig_unix),
                (t.cmdline, t.cmdline_len),
            ),
            None => {
                irq_restore(flags);
                return;
            }
        };
        if let Some((dst, dst_waiting)) = signals_mut(child) {
            dst.umask = src_umask;
            (dst.cmdline, dst.cmdline_len) = src_name;
            // The child is a copy of the program, handlers and the word they
            // are told through included. What was raised for the parent and
            // not yet taken is the parent's.
            (dst.sig_ignore, dst.sig_catch, dst.sig_word) = src_signals;
            (dst.sig_run, dst.sig_masks, dst.sig_flags, dst.sig_cookies, dst.sig_entry, dst.sig_unix) = src_run;
            dst.sig_pending = 0;
            dst.sig_held = 0;
            dst_waiting.clear();
            dst.sig_interrupt = false;
            for (i, kind) in src_fds.iter().enumerate() {
                if kind.is_empty() || !dst.fds[i].is_empty() {
                    continue;
                }
                if crate::pipe::retain_fd(kind).is_ok() {
                    dst.fds[i] = *kind;
                    if src_cloexec & (1u128 << i) != 0 {
                        dst.cloexec |= 1u128 << i;
                    }
                }
            }
        }
    }
    irq_restore(flags);
}

/// What descriptor `fd` of `tid`'s program names. `FD_CWD` is a slot too.
pub fn get(tid: usize, fd: usize) -> FdKind {
    if fd >= SLOTS {
        return FdKind::Empty;
    }
    let flags = irq_save();
    let kind = unsafe {
        match table_mut(tid) {
            Some(t) => t.fds[fd],
            None => FdKind::Empty,
        }
    };
    irq_restore(flags);
    kind
}

/// What `fd` names, with a reference taken for a copy of it: reading the
/// descriptor and retaining what it names are one step, so a sibling closing
/// it in between cannot leave the caller retaining something freed.
pub fn get_retained(tid: usize, fd: usize) -> Option<FdKind> {
    if fd >= SLOTS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid) {
            Some(t) if !t.fds[fd].is_empty() => {
                let kind = t.fds[fd];
                if crate::pipe::retain_fd(&kind).is_ok() { Some(kind) } else { None }
            }
            _ => None,
        }
    };
    irq_restore(flags);
    out
}

/// Put `kind` at `fd`, and return what was there for the caller to release.
/// The slot's close-on-exec mark is cleared: it belonged to what was there.
pub fn replace(tid: usize, fd: usize, kind: FdKind) -> Result<FdKind, ()> {
    if fd >= SLOTS {
        return Err(());
    }
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid) {
            Some(t) => {
                t.cloexec &= !(1u128 << fd);
                Ok(core::mem::replace(&mut t.fds[fd], kind))
            }
            None => Err(()),
        }
    };
    irq_restore(flags);
    out
}

/// Put `new` at `fd` if `expected` is what is there, keeping the slot's
/// close-on-exec mark: the same descriptor, become what its object became.
/// What was there is the caller's to release. False, and nothing changed,
/// if the slot holds something else.
pub fn swap_if(tid: usize, fd: usize, expected: FdKind, new: FdKind) -> bool {
    if fd >= SLOTS {
        return false;
    }
    let flags = irq_save();
    let swapped = unsafe {
        table_mut(tid).is_some_and(|t| {
            let same = t.fds[fd] == expected;
            if same {
                t.fds[fd] = new;
            }
            same
        })
    };
    irq_restore(flags);
    swapped
}

/// Empty `fd` and return what it named, for the caller to release.
pub fn take(tid: usize, fd: usize) -> FdKind {
    replace(tid, fd, FdKind::Empty).unwrap_or(FdKind::Empty)
}

/// Put `kind` in the lowest free descriptor at or above `floor`, and say
/// which. Finding the slot and filling it are one step.
pub fn install(tid: usize, kind: FdKind, floor: usize) -> Option<usize> {
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid) {
            Some(t) => match (floor..MAX_FDS).find(|&fd| t.fds[fd].is_empty()) {
                Some(fd) => {
                    t.fds[fd] = kind;
                    t.cloexec &= !(1u128 << fd);
                    Some(fd)
                }
                None => None,
            },
            None => None,
        }
    };
    irq_restore(flags);
    out
}

/// The lowest free descriptor at or above `floor`, for a caller that has to
/// know the number before it has the object. Somebody else may take it before
/// the caller does; `replace` then closes what they put there, as `dup2` would.
pub fn free_at_or_above(tid: usize, floor: usize) -> Option<usize> {
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid) {
            Some(t) => (floor..MAX_FDS).find(|&fd| t.fds[fd].is_empty()),
            None => None,
        }
    };
    irq_restore(flags);
    out
}

/// Whether any descriptor of `tid`'s program — the working directory's slot
/// included — is one `wanted` says yes to.
pub fn any(tid: usize, wanted: impl Fn(&FdKind) -> bool) -> bool {
    let flags = irq_save();
    let fds = unsafe { table_mut(tid).map(|t| t.fds) };
    irq_restore(flags);
    match fds {
        Some(fds) => fds.iter().any(|k| !k.is_empty() && wanted(k)),
        None => false,
    }
}

/// What a program has said to do about a signal.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Nothing said: the kernel does what the signal does.
    Default = 0,
    Ignore = 1,
    /// The program has a handler, and is told.
    Catch = 2,
    /// The program has a handler, and the kernel runs it.
    Run = 3,
}

/// How a handler the kernel runs is to be run.
#[derive(Clone, Copy)]
pub struct Handler {
    /// The signals held back while it runs, besides its own.
    pub mask: u64,
    /// `signal::NODEFER`, `RESETHAND`, `ONSTACK`, `RESTARTS` in the low
    /// byte; above it, the program's own, handed back in the frame.
    pub flags: u32,
    /// The program's word for the handler, handed back in the frame: which
    /// function it is, usually.
    pub cookie: u64,
    /// Where the program is entered: the same for every signal.
    pub entry: usize,
}

fn sig_bit(signo: u8) -> u64 {
    1u64 << (signo - 1)
}

/// What `tid`'s program does about signal `signo`, changed to `new` if that
/// is given. Returns what it was; `None` for a task in no program, or a
/// number that is not a signal's.
pub fn sig_action(tid: usize, signo: u8, new: Option<Disposition>) -> Option<Disposition> {
    if signo == 0 || signo > 64 {
        return None;
    }
    let bit = sig_bit(signo);
    let flags = irq_save();
    let old = unsafe {
        signals_mut(tid).map(|(t, w)| {
            let old = said(t, bit);
            if let Some(new) = new {
                // A signal waiting for a handler the kernel was to run, and
                // held back, is still waiting when the handler has gone: it
                // does what it does by default when it is let through.
                let waiting = t.sig_run & t.sig_pending & bit;
                t.sig_ignore &= !bit;
                t.sig_catch &= !bit;
                t.sig_run &= !bit;
                match new {
                    Disposition::Ignore => {
                        t.sig_ignore |= bit;
                        t.sig_held &= !bit;
                    }
                    Disposition::Catch => {
                        t.sig_catch |= bit;
                        t.sig_held &= !bit;
                    }
                    Disposition::Default => t.sig_held |= waiting,
                    // Said with what to run: `sig_handle`.
                    Disposition::Run => {}
                }
                // A signal waiting for a handler that is no longer there is
                // not waiting for anything.
                if new != Disposition::Catch {
                    t.sig_pending &= !bit;
                }
                if (t.sig_pending | t.sig_held) & bit == 0 {
                    w.forget(signo);
                }
            }
            old
        })
    };
    irq_restore(flags);
    old
}

/// `tid`'s program enters its handlers at `entry`, and — with `unix` — a
/// call a signal cuts short answers it as Unix would have it.
pub fn sig_enter_at(tid: usize, entry: usize, unix: bool) -> bool {
    let flags = irq_save();
    let done = unsafe {
        table_mut(tid).map(|t| {
            t.sig_entry = entry;
            t.sig_unix = unix;
        })
    };
    irq_restore(flags);
    done.is_some()
}

/// Whether `tid`'s program has said where its handlers are entered.
pub fn sig_has_entry(tid: usize) -> bool {
    let flags = irq_save();
    let has = unsafe { table_mut(tid).is_some_and(|t| t.sig_entry != 0) };
    irq_restore(flags);
    has
}

/// Whether `tid`'s program has said so.
pub fn sig_unix(tid: usize) -> bool {
    let flags = irq_save();
    let unix = unsafe { table_mut(tid).is_some_and(|t| t.sig_unix) };
    irq_restore(flags);
    unix
}

/// What a table says about the signal whose bit is `bit`.
fn said(t: &Table, bit: u64) -> Disposition {
    if t.sig_run & bit != 0 {
        Disposition::Run
    } else if t.sig_catch & bit != 0 {
        Disposition::Catch
    } else if t.sig_ignore & bit != 0 {
        Disposition::Ignore
    } else {
        Disposition::Default
    }
}

/// `tid`'s program has a handler for `signo` that the kernel is to run, as
/// `how` says. Returns what it said before.
pub fn sig_handle(tid: usize, signo: u8, how: Handler) -> Option<Disposition> {
    if signo == 0 || signo > 64 {
        return None;
    }
    let bit = sig_bit(signo);
    let flags = irq_save();
    let old = unsafe {
        table_mut(tid).map(|t| {
            let old = said(t, bit);
            t.sig_ignore &= !bit;
            t.sig_catch &= !bit;
            t.sig_run |= bit;
            // Raised while it was held back with nothing said: it is for
            // the handler now.
            if t.sig_held & bit != 0 {
                t.sig_held &= !bit;
                t.sig_pending |= bit;
            }
            t.sig_masks[signo as usize - 1] = how.mask;
            t.sig_flags[signo as usize - 1] = how.flags;
            t.sig_cookies[signo as usize - 1] = how.cookie;
            old
        })
    };
    irq_restore(flags);
    old
}

/// Whether `tid`'s program has anything a task of it with `mask` should be
/// doing something about on its way out of the kernel: a handler to be run,
/// or a signal that was held back and is not by this task.
pub fn sig_ready(tid: usize, mask: u64) -> bool {
    let flags = irq_save();
    let ready = unsafe {
        table_mut(tid).is_some_and(|t| ((t.sig_pending & t.sig_run) | t.sig_held) & !mask != 0)
    };
    irq_restore(flags);
    ready
}

/// Take the lowest signal waiting for a handler the kernel runs that `mask`
/// does not hold back: the signal, how to run its handler, and what came
/// with it. It is no longer waiting, unless another of its number was
/// behind it.
pub fn sig_run_take(tid: usize, mask: u64) -> Option<(u8, Handler, Info)> {
    let flags = irq_save();
    let out = unsafe {
        signals_mut(tid).and_then(|(t, w)| {
            let ready = t.sig_pending & t.sig_run & !mask;
            if ready == 0 || t.sig_entry == 0 {
                return None;
            }
            let signo = ready.trailing_zeros() as u8 + 1;
            let (info, more) = w.take(signo);
            if !more {
                t.sig_pending &= !sig_bit(signo);
            }
            Some((signo, begin(t, signo), info))
        })
    };
    irq_restore(flags);
    out
}

/// How the handler for `signo` is to be run, with what running it changes
/// done: one that is to run once is not there for the next.
fn begin(t: &mut Table, signo: u8) -> Handler {
    let i = signo as usize - 1;
    let how = Handler { mask: t.sig_masks[i], flags: t.sig_flags[i], cookie: t.sig_cookies[i], entry: t.sig_entry };
    if how.flags & crate::signal::RESETHAND as u32 != 0 {
        t.sig_run &= !sig_bit(signo);
    }
    how
}

/// The handler `tid`'s program has for `signo`, if it has one the kernel
/// runs: for a signal that is not raised but happens — a fault.
pub fn sig_run_begin(tid: usize, signo: u8) -> Option<Handler> {
    if signo == 0 || signo > 64 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        table_mut(tid).and_then(|t| {
            (t.sig_run & sig_bit(signo) != 0 && t.sig_entry != 0).then(|| begin(t, signo))
        })
    };
    irq_restore(flags);
    out
}

/// Take the lowest signal that was held back with nothing said about it and
/// that `mask` does not hold back.
pub fn sig_held_take(tid: usize, mask: u64) -> Option<u8> {
    let flags = irq_save();
    let out = unsafe {
        signals_mut(tid).and_then(|(t, w)| {
            let ready = t.sig_held & !mask;
            if ready == 0 {
                return None;
            }
            let signo = ready.trailing_zeros() as u8 + 1;
            if !w.take(signo).1 {
                t.sig_held &= !sig_bit(signo);
            }
            Some(signo)
        })
    };
    irq_restore(flags);
    out
}

/// `signo` was raised for `tid`'s program, with `info`, which has said
/// nothing about it, while every task of it held it back or one waited to
/// take it. False if it cannot wait: a real-time signal with as many of it
/// waiting as can.
pub fn sig_hold(tid: usize, signo: u8, info: Info) -> bool {
    let bit = sig_bit(signo);
    let flags = irq_save();
    let held = unsafe {
        signals_mut(tid).is_none_or(|(t, w)| {
            let ok = w.put(signo, info, (t.sig_held | t.sig_pending) & bit != 0);
            if ok {
                t.sig_held |= bit;
            }
            ok
        })
    };
    irq_restore(flags);
    held
}

/// `signo` is no longer held for `tid`'s program.
pub fn sig_unhold(tid: usize, signo: u8) {
    let flags = irq_save();
    unsafe {
        if let Some((t, w)) = signals_mut(tid) {
            t.sig_held &= !sig_bit(signo);
            if t.sig_pending & sig_bit(signo) == 0 {
                w.forget(signo);
            }
        }
    }
    irq_restore(flags);
}

/// Every signal waiting for `tid`'s program: to be told of, to be run, or
/// held back with nothing said.
pub fn sig_pending_set(tid: usize) -> u64 {
    let flags = irq_save();
    let set = unsafe { table_mut(tid).map_or(0, |t| (t.sig_pending & (t.sig_run | t.sig_catch)) | t.sig_held) };
    irq_restore(flags);
    set
}

/// Take the lowest of `set` waiting for `tid`'s program, whatever was said
/// about it, without anything being done about it: the signal and what came
/// with it.
pub fn sig_take_one(tid: usize, set: u64) -> Option<(u8, Info)> {
    let flags = irq_save();
    let out = unsafe {
        signals_mut(tid).and_then(|(t, w)| {
            let waiting = ((t.sig_pending & (t.sig_run | t.sig_catch)) | t.sig_held) & set;
            if waiting == 0 {
                return None;
            }
            let signo = waiting.trailing_zeros() as u8 + 1;
            let (info, more) = w.take(signo);
            if !more {
                t.sig_pending &= !sig_bit(signo);
                t.sig_held &= !sig_bit(signo);
            }
            Some((signo, info))
        })
    };
    irq_restore(flags);
    out
}

/// Signal `signo` has been raised for `tid`'s program, with `info`.
/// Returns what the program said to do about it and, when that is to tell
/// it, where — with the signal now waiting to be taken, or to be run; or
/// `Err` for a real-time signal with as many of it waiting as can.
pub fn sig_post(tid: usize, signo: u8, info: Info) -> Option<Result<(Disposition, usize), ()>> {
    if signo == 0 || signo > 64 {
        return None;
    }
    let bit = sig_bit(signo);
    let flags = irq_save();
    let out = unsafe {
        signals_mut(tid).map(|(t, w)| {
            if t.sig_run & bit != 0 || t.sig_catch & bit != 0 {
                if !w.put(signo, info, (t.sig_pending | t.sig_held) & bit != 0) {
                    return Err(());
                }
                t.sig_pending |= bit;
            }
            Ok(if t.sig_run & bit != 0 {
                (Disposition::Run, 0)
            } else if t.sig_catch & bit != 0 {
                t.sig_interrupt = true;
                (Disposition::Catch, t.sig_word)
            } else if t.sig_ignore & bit != 0 {
                (Disposition::Ignore, 0)
            } else {
                (Disposition::Default, 0)
            })
        })
    };
    irq_restore(flags);
    out
}

/// The signals raised for `tid`'s program that it has a handler for, each
/// of which is no longer waiting once this returns — unless another of its
/// number was behind it, and then the program is to be told again, through
/// the word this answers with. `word`, if not 0, is where it wants to be
/// told of the next.
pub fn sig_take(tid: usize, word: usize) -> (u64, Option<usize>) {
    let flags = irq_save();
    let taken = unsafe {
        match signals_mut(tid) {
            Some((t, w)) => {
                if word != 0 {
                    t.sig_word = word;
                }
                t.sig_interrupt = false;
                // Those the program is told of. One the kernel runs a
                // handler for is taken by running it.
                let taken = t.sig_pending & t.sig_catch;
                let mut again = false;
                for signo in 1..=64u8 {
                    if taken & sig_bit(signo) != 0 {
                        if w.take(signo).1 {
                            again = true;
                        } else {
                            t.sig_pending &= !sig_bit(signo);
                        }
                    }
                }
                if again {
                    t.sig_interrupt = true;
                }
                (taken, again.then_some(t.sig_word))
            }
            None => (0, None),
        }
    };
    irq_restore(flags);
    taken
}

/// Has a signal with a handler been raised for `tid`'s program and ended no
/// wait yet? A task about to wait asks, and if so does not wait; asking is
/// what uses the answer up.
pub fn sig_interrupted(tid: usize) -> bool {
    let flags = irq_save();
    let was = unsafe {
        match table_mut(tid) {
            Some(t) => core::mem::replace(&mut t.sig_interrupt, false),
            None => false,
        }
    };
    irq_restore(flags);
    was
}

/// How the alarm of `tid`'s program stands at `now`: the nanoseconds left
/// of it, which is 0 only if there is none, and what it repeats at. With
/// `new` it is then set to that — `(nanoseconds from now, repeat)`, 0 from
/// now for no alarm. `None` for a task in no program.
pub fn alarm(tid: usize, now: u64, new: Option<(u64, u64)>) -> Option<(u64, u64)> {
    let flags = irq_save();
    let was = unsafe {
        table_mut(tid).map(|t| {
            let left = match t.alarm_at {
                0 => 0,
                at => at.saturating_sub(now).max(1),
            };
            let was = (left, t.alarm_every);
            if let Some((first, every)) = new {
                t.alarm_at = if first == 0 { 0 } else { now.saturating_add(first) };
                t.alarm_every = if first == 0 { 0 } else { every };
            }
            was
        })
    };
    irq_restore(flags);
    was
}

/// When an alarm that was due at `at` and is seen to at `now` is next due:
/// on its own beat, the first time after now, or never for one that does
/// not repeat.
fn alarm_next(at: u64, every: u64, now: u64) -> u64 {
    if every == 0 {
        return 0;
    }
    at.saturating_add(((now - at) / every + 1).saturating_mul(every))
}

/// A task of a program whose alarm is due at `now`, the alarm having been
/// set for its next time or turned off. `None` when no program's is.
pub fn alarm_due(now: u64) -> Option<usize> {
    let flags = irq_save();
    let due = unsafe {
        let of = &*core::ptr::addr_of!(OF_TASK);
        let mut found = None;
        for (i, t) in tables().iter_mut().enumerate() {
            if t.tasks == 0 || t.alarm_at == 0 || t.alarm_at > now {
                continue;
            }
            t.alarm_at = alarm_next(t.alarm_at, t.alarm_every, now);
            found = of.iter().position(|&table| table != NONE && table as usize == i);
            if found.is_some() {
                break;
            }
        }
        found
    };
    irq_restore(flags);
    due
}

/// When the earliest alarm will be due once those due at `now` have been
/// seen to ([`alarm_due`]), or `u64::MAX` if no program will have one.
pub fn alarm_after(now: u64) -> u64 {
    let flags = irq_save();
    let mut next = u64::MAX;
    unsafe {
        for t in tables().iter() {
            if t.tasks == 0 || t.alarm_at == 0 {
                continue;
            }
            let at = if t.alarm_at > now { t.alarm_at } else { alarm_next(t.alarm_at, t.alarm_every, now) };
            if at != 0 {
                next = next.min(at);
            }
        }
    }
    irq_restore(flags);
    next
}

/// The tasks of `tid`'s program — every task using its table — into `out`.
/// Returns how many.
pub fn tasks_of(tid: usize, out: &mut [usize]) -> usize {
    if tid >= MAX_TASKS {
        return 0;
    }
    let mut n = 0;
    let flags = irq_save();
    unsafe {
        let of = &*core::ptr::addr_of!(OF_TASK);
        let table = of[tid];
        if table != NONE {
            for (other, &t) in of.iter().enumerate() {
                if t == table && n < out.len() {
                    out[n] = other;
                    n += 1;
                }
            }
        }
    }
    irq_restore(flags);
    n
}

/// One task from each program holding a descriptor `wanted` says yes to,
/// into `out`. Returns how many.
pub fn holders(wanted: impl Fn(&FdKind) -> bool, out: &mut [usize]) -> usize {
    let mut n = 0;
    let flags = irq_save();
    unsafe {
        let of = &*core::ptr::addr_of!(OF_TASK);
        for (i, table) in tables().iter().enumerate() {
            if table.tasks == 0 || !table.fds[..MAX_FDS].iter().any(&wanted) {
                continue;
            }
            if let Some(tid) = of.iter().position(|&t| t as usize == i && t != NONE) {
                if n < out.len() {
                    out[n] = tid;
                    n += 1;
                }
            }
        }
    }
    irq_restore(flags);
    n
}

/// What the ended tasks of `tid`'s program used.
pub fn usage_gone(tid: usize) -> crate::usage::Usage {
    let flags = irq_save();
    let used = unsafe { table_mut(tid).map_or(crate::usage::Usage::ZERO, |t| t.used_gone) };
    irq_restore(flags);
    used
}

/// A task of `tid`'s program has ended, having used `used`.
pub fn usage_gone_add(tid: usize, used: &crate::usage::Usage) {
    let flags = irq_save();
    unsafe {
        if let Some(t) = table_mut(tid) {
            t.used_gone.add(used);
        }
    }
    irq_restore(flags);
}

/// What the children `tid`'s program collected used.
pub fn usage_children(tid: usize) -> crate::usage::Usage {
    let flags = irq_save();
    let used = unsafe { table_mut(tid).map_or(crate::usage::Usage::ZERO, |t| t.used_children) };
    irq_restore(flags);
    used
}

/// `tid`'s program has collected a child that used `used`.
pub fn usage_children_add(tid: usize, used: &crate::usage::Usage) {
    let flags = irq_save();
    unsafe {
        if let Some(t) = table_mut(tid) {
            t.used_children.add(used);
        }
    }
    irq_restore(flags);
}

/// `tid`'s program runs as `from`'s does: as nice, and with the same limit
/// on how long. Not what it has used, which for a new program is nothing.
pub fn runs_like(tid: usize, from: usize) {
    let flags = irq_save();
    unsafe {
        if let Some((nice, soft, hard)) = table_mut(from).map(|t| (t.nice, t.cpu_soft, t.cpu_hard)) {
            if let Some(t) = table_mut(tid) {
                (t.nice, t.cpu_soft, t.cpu_hard) = (nice, soft, hard);
            }
        }
    }
    irq_restore(flags);
}

/// Say what `tid`'s program was started as: `cmdline`, as much as fits.
pub fn set_cmdline(tid: usize, cmdline: &[u8]) -> bool {
    let flags = irq_save();
    let done = unsafe {
        match table_mut(tid) {
            Some(t) => {
                let n = cmdline.len().min(CMDLINE);
                t.cmdline[..n].copy_from_slice(&cmdline[..n]);
                t.cmdline[n..].fill(0);
                t.cmdline_len = n as u8;
                true
            }
            None => false,
        }
    };
    irq_restore(flags);
    done
}

/// What `tid`'s program was started as, into `out`: how long it is.
pub fn cmdline_of(tid: usize, out: &mut [u8; CMDLINE]) -> Option<usize> {
    let flags = irq_save();
    let len = unsafe {
        table_mut(tid).map(|t| {
            *out = t.cmdline;
            t.cmdline_len as usize
        })
    };
    irq_restore(flags);
    len
}

/// How nice `tid`'s program is.
pub fn nice_of(tid: usize) -> i8 {
    let flags = irq_save();
    let nice = unsafe { table_mut(tid).map_or(0, |t| t.nice) };
    irq_restore(flags);
    nice
}

pub fn set_nice(tid: usize, nice: i8) {
    let flags = irq_save();
    unsafe {
        if let Some(t) = table_mut(tid) {
            t.nice = nice;
        }
    }
    irq_restore(flags);
}

/// How many seconds of processor time `tid`'s program may have: SIGXCPU past
/// the first, the end at the second.
pub fn cpu_limit_of(tid: usize) -> (u64, u64) {
    let flags = irq_save();
    let limit = unsafe { table_mut(tid).map_or((u64::MAX, u64::MAX), |t| (t.cpu_soft, t.cpu_hard)) };
    irq_restore(flags);
    limit
}

pub fn set_cpu_limit(tid: usize, soft: u64, hard: u64) {
    let flags = irq_save();
    unsafe {
        if let Some(t) = table_mut(tid) {
            (t.cpu_soft, t.cpu_hard) = (soft, hard);
        }
    }
    irq_restore(flags);
}

/// Whether `tid`'s program, having used `seconds`, is owed a SIGXCPU it has
/// not been sent: one a second. Taken if it is.
pub fn xcpu_due(tid: usize, seconds: u64) -> bool {
    let flags = irq_save();
    let due = unsafe {
        table_mut(tid).is_some_and(|t| {
            let due = t.xcpu_sent == u64::MAX || seconds > t.xcpu_sent;
            if due {
                t.xcpu_sent = seconds;
            }
            due
        })
    };
    irq_restore(flags);
    due
}

/// Set the program's umask and return what it was; `None` only reads it.
pub fn umask(tid: usize, new: Option<u16>) -> u16 {
    let flags = irq_save();
    let old = unsafe {
        match table_mut(tid) {
            Some(t) => {
                let old = t.umask;
                if let Some(mask) = new {
                    t.umask = mask & 0o777;
                }
                old
            }
            None => DEFAULT_UMASK,
        }
    };
    irq_restore(flags);
    old
}

/// Whether `fd` is closed when the program becomes another.
pub fn cloexec(tid: usize, fd: usize) -> Option<bool> {
    if fd >= SLOTS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid) {
            Some(t) if !t.fds[fd].is_empty() => Some(t.cloexec & (1u128 << fd) != 0),
            _ => None,
        }
    };
    irq_restore(flags);
    out
}

pub fn set_cloexec(tid: usize, fd: usize, on: bool) -> bool {
    if fd >= SLOTS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        match table_mut(tid) {
            Some(t) if !t.fds[fd].is_empty() => {
                if on {
                    t.cloexec |= 1u128 << fd;
                } else {
                    t.cloexec &= !(1u128 << fd);
                }
                true
            }
            _ => false,
        }
    };
    irq_restore(flags);
    ok
}

/// The program is becoming another: close every descriptor marked for it.
pub fn close_on_exec(tid: usize) {
    let mut gone = [FdKind::Empty; SLOTS];
    let flags = irq_save();
    unsafe {
        if let Some((t, w)) = signals_mut(tid) {
            for fd in 0..SLOTS {
                if t.cloexec & (1u128 << fd) != 0 {
                    gone[fd] = core::mem::replace(&mut t.fds[fd], FdKind::Empty);
                }
            }
            t.cloexec = 0;
            // A handler is an address in the program that has just gone, and
            // so is the word it was told through. What was ignored still is.
            t.sig_catch = 0;
            t.sig_run = 0;
            t.sig_entry = 0;
            t.sig_unix = false;
            t.sig_pending = 0;
            w.keep(t.sig_held);
            t.sig_interrupt = false;
            t.sig_word = 0;
        }
    }
    irq_restore(flags);
    // And its timers, which were set for the program that has gone.
    crate::ptimer::clear(table_of(tid));
    release_all(&gone);
}

/// A signal has arrived for `tid`, which may be parked on what it is using:
/// if it is, it is taken off that list and woken, and goes to see. True if
/// it was.
pub fn interrupt(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let held = unsafe { (*core::ptr::addr_of!(HELD))[tid] };
    let found = !held.is_empty() && crate::pipe::forget_waiter(&held, tid);
    if found {
        crate::scheduler::unblock_task(tid);
    }
    irq_restore(flags);
    found
}

/// What `fd` names, held for as long as the calling task is using it.
///
/// A task about to wait on a pipe, a terminal or a server has only the
/// descriptor's reference to rely on, and a sibling may close the descriptor
/// while it waits. This takes one for the task itself, which `unhold` gives
/// back — or `task_gone`, for a task killed where it was waiting.
pub fn hold(tid: usize, fd: usize) -> FdKind {
    if tid >= MAX_TASKS || fd >= MAX_FDS {
        return FdKind::Empty;
    }
    let flags = irq_save();
    let kind = unsafe {
        match table_mut(tid) {
            Some(t) => t.fds[fd],
            None => FdKind::Empty,
        }
    };
    // Only what a task can be parked on. Memory and poll sets are never
    // waited on through a read or a write, and an endpoint has no object.
    let waits = matches!(
        kind,
        FdKind::PipeRead(_)
            | FdKind::PipeWrite(_)
            | FdKind::StreamEnd { .. }
            | FdKind::PtyEnd { .. }
            | FdKind::Timer { .. }
            | FdKind::Event { .. }
            | FdKind::Served { .. }
            | FdKind::Signals { .. }
            | FdKind::Local { .. }
    );
    if waits && crate::pipe::retain_fd(&kind).is_ok() {
        unsafe { (*core::ptr::addr_of_mut!(HELD))[tid] = kind };
    }
    irq_restore(flags);
    kind
}

/// Give back what `hold` took.
pub fn unhold(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    let kind = unsafe {
        core::mem::replace(&mut (*core::ptr::addr_of_mut!(HELD))[tid], FdKind::Empty)
    };
    irq_restore(flags);
    if !kind.is_empty() {
        crate::pipe::release_fd(&kind);
    }
}
