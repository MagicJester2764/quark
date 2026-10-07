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
//! Beside the numbered ones, `FD_CWD` names the program's working directory.
//! It is a descriptor like the rest — copied by `fork`, kept by `exec`, handed
//! to a child by its spawner — so that where a program *is* follows it the way
//! what it has *open* does, and no server has to be told that one program
//! became another.
//!
//! The numbered descriptors are an array that grows (`grow.rs`) to the
//! program's limit, which the program can raise: a desktop's programs hold
//! hundreds.
//!
//! Sharing is what makes every operation here take the lock. With a table per
//! task, only that task changed it. Now a sibling can be preempted half way
//! through finding a free slot, so "find a free slot and fill it" is one step
//! with interrupts off, and a task about to block on what a descriptor names
//! takes a reference of its own first (`hold`) — or a sibling closing that
//! descriptor would free the object under it, and the next thing to take the
//! slot would be read by a task that never held it.

use crate::grow::Grow;
use crate::signal::{Info, Waiting};
use crate::task::{FdKind, FD_MOST, FD_SOFT, MAX_TASKS};

/// The working directory: not one of the numbered descriptors but a field of
/// its own, named by Linux's `AT_FDCWD` (−100) as an unsigned word. It was 64,
/// one past a table of sixty-four, and a table that grows would have moved it
/// with every limit; this moves for none. It can be copied to and from and
/// asked about; it is never read, written or waited on, and no allocation
/// ever chooses it.
pub const FD_CWD: usize = 0xFFFF_FFFF_FFFF_FF9C;

/// The room a table's descriptors are given when it first needs any.
const FIRST: usize = 64;

/// One numbered descriptor, and whether it is closed when the program becomes
/// another (`exec`).
#[derive(Clone, Copy)]
struct Fd {
    kind: FdKind,
    cloexec: bool,
}

const NO_FD: Fd = Fd { kind: FdKind::Empty, cloexec: false };

const NONE: u16 = u16::MAX;

struct Table {
    /// Tasks using this table. Zero is a free table.
    tasks: u16,
    /// The numbered descriptors, in room that grows to `fd_soft`.
    fds: Grow<Fd>,
    /// The working directory (`FD_CWD`).
    cwd: FdKind,
    /// How many descriptors the program may have, and how far it may raise
    /// that: `RLIMIT_NOFILE`'s two. `fork` copies them and `exec` keeps them.
    /// A descriptor already above a limit the program lowered stays; no new
    /// one is made there.
    fd_soft: u32,
    fd_hard: u32,
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
    /// Where the program's own system calls are made from, when it has
    /// said (`SYS_SYSCALL_TRAP`): one made from anywhere else is not made,
    /// and raises SIGSYS (`signal::trap_call`). Equal for none. `fork`
    /// copies it, the child's code being where the parent's was; an `exec`
    /// is another program, and has none.
    trap_from: usize,
    trap_to: usize,
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
    fds: Grow::new(NO_FD),
    cwd: FdKind::Empty,
    fd_soft: FD_SOFT as u32,
    fd_hard: FD_MOST as u32,
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
    trap_from: 0,
    trap_to: 0,
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

impl Table {
    /// What `fd` names: the working directory for `FD_CWD`, nothing past the
    /// room there is.
    fn kind(&self, fd: usize) -> FdKind {
        if fd == FD_CWD { self.cwd } else { self.fds.get(fd).map_or(FdKind::Empty, |s| s.kind) }
    }

    /// The lowest free number at or above `floor` the program may have.
    fn lowest_free(&self, floor: usize) -> Option<usize> {
        let soft = self.fd_soft as usize;
        (floor..self.fds.len().min(soft))
            .find(|&fd| self.fds.get(fd).is_some_and(|s| s.kind.is_empty()))
            .or_else(|| Some(floor.max(self.fds.len())).filter(|&fd| fd < soft))
    }

    /// Put `kind` at `fd`, the close-on-exec mark cleared, and answer what
    /// was there. Placing something needs `fd` below the program's limit and
    /// room for it; emptying a slot needs neither.
    fn put(&mut self, fd: usize, kind: FdKind) -> Result<FdKind, ()> {
        if fd == FD_CWD {
            return Ok(core::mem::replace(&mut self.cwd, kind));
        }
        if kind.is_empty() {
            return Ok(self.fds.get_mut(fd).map_or(FdKind::Empty, |s| core::mem::replace(s, NO_FD).kind));
        }
        if fd >= self.fd_soft as usize {
            return Err(());
        }
        let slot = self.fds.ensure(fd, FIRST, FD_MOST).map_err(|_| ())?;
        slot.cloexec = false;
        Ok(core::mem::replace(&mut slot.kind, kind))
    }
}

/// What a program keeps, in one record: its descriptor table, what came with
/// the signals waiting for it, and its timers. Made with its first task
/// (`attach_new`), and given back with everything in it when its last task
/// goes (`leave`).
pub struct Program {
    table: Table,
    /// What came with each signal waiting for the program, and the
    /// real-time signals waiting behind one of their number.
    waiting: Waiting<{ crate::signal::QUEUE }>,
    /// Its timers (`ptimer.rs`): no room for them until it makes one.
    pub timers: crate::ptimer::Timers,
    /// The first of the tasks using the table, the rest linked through
    /// their records (`PerTask::next`): [`END`] for none.
    first: u16,
}

/// The end of a program's list of tasks.
const END: u16 = u16::MAX;

/// What a program's record is made from, in its room (`Table::fill_from`):
/// five kilobytes, which built on the stack were a kernel stack's.
static PROGRAM_TEMPLATE: crate::table::Template<Program> =
    crate::table::Template(Some(Program { table: EMPTY, waiting: Waiting::EMPTY, timers: crate::ptimer::Timers::NONE, first: END }));

/// Every program's record, by its table's number (`table.rs`): as many as
/// there can be tasks, since each task uses exactly one.
static mut TABLES: crate::table::Table<Program> = crate::table::Table::new(MAX_TASKS);
/// What this module keeps about a task, in its record (`TaskRec::fd`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    /// Which table the task uses.
    table: u16,
    /// What the task is in the middle of using, held so that it cannot go away.
    held: FdKind,
    /// The tasks either side of it in its program's list (`Program::first`).
    next: u16,
    prev: u16,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            table: NONE,
            held: FdKind::Empty,
            next: END,
            prev: END,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.fd,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


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
unsafe fn tables() -> &'static mut crate::table::Table<Program> {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLES) }
}

/// Table number `i`, if a program has it.
///
/// # Safety
/// Interrupts are off.
unsafe fn table_at(i: usize) -> Option<&'static mut Table> {
    unsafe { tables().get(i).map(|p| &mut p.table) }
}

/// Every program's record, with its table's number.
///
/// # Safety
/// Interrupts are off for as long as the records are used.
pub unsafe fn programs() -> impl Iterator<Item = (usize, &'static mut Program)> {
    unsafe { (0..tables().high()).filter_map(|i| tables().get(i).map(|p| (i, p))) }
}

/// Put `tid` first in table `i`'s list of the tasks using it.
///
/// # Safety
/// Interrupts are off.
unsafe fn link(i: usize, tid: usize) {
    unsafe {
        let Some(p) = tables().get(i) else { return };
        let first = p.first;
        st(tid).next = first;
        st(tid).prev = END;
        if first != END {
            st(first as usize).prev = tid as u16;
        }
        p.first = tid as u16;
    }
}

/// Take `tid` off table `i`'s list.
///
/// # Safety
/// Interrupts are off.
unsafe fn unlink(i: usize, tid: usize) {
    unsafe {
        let (next, prev) = (st(tid).next, st(tid).prev);
        if prev != END {
            st(prev as usize).next = next;
        } else if let Some(p) = tables().get(i) {
            p.first = next;
        }
        if next != END {
            st(next as usize).prev = prev;
        }
        st(tid).next = END;
        st(tid).prev = END;
    }
}

/// Each task of `tid`'s program — every task using its table, `tid` among
/// them — to `f`, with interrupts off throughout. `f` may end the task it is
/// given, which takes that one off the list, and no other.
pub fn each_task(tid: usize, mut f: impl FnMut(usize)) {
    let flags = irq_save();
    unsafe {
        let mut t = table_of(tid).map_or(END, |p| p.first);
        while t != END {
            let next = st(t as usize).next;
            f(t as usize);
            t = next;
        }
    }
    irq_restore(flags);
}

/// The first program at or past table number `from`: its table's number and
/// its first task. For a walk of every program that raises signals as it
/// goes — a signal can end a program — and so asks afresh each step.
pub fn next_program(from: usize) -> Option<(usize, usize)> {
    next_where(from, |_| true)
}

/// The first program at or past table number `from` holding a descriptor
/// `wanted` says yes to, as [`next_program`] answers.
pub fn next_holder(from: usize, wanted: impl Fn(&FdKind) -> bool) -> Option<(usize, usize)> {
    next_where(from, |t| t.fds.iter().any(|s| wanted(&s.kind)))
}

fn next_where(from: usize, take: impl Fn(&Table) -> bool) -> Option<(usize, usize)> {
    let flags = irq_save();
    let found = unsafe {
        (from..tables().high()).find_map(|i| {
            let p = tables().get(i)?;
            (p.first != END && take(&p.table)).then_some((i, p.first as usize))
        })
    };
    irq_restore(flags);
    found
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
        let i = st(tid).table;
        if i == NONE { None } else { table_at(i as usize) }
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
        let i = st(tid).table;
        if i == NONE { None } else { tables().get(i as usize).map(|p| (&mut p.table, &mut p.waiting)) }
    }
}

/// Give a new task an empty table of its own. False if it already has one.
pub fn attach_new(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        if st(tid).table != NONE {
            false
        } else {
            // A number of its own — there is always one, a table per task
            // and this task having none — and a record, if there is memory
            // for it.
            match tables().lowest_free(0) {
                Some(i) if tables().fill_from(i, &PROGRAM_TEMPLATE, |p| p.table.tasks = 1).is_ok() => {
                    st(tid).table = i as u16;
                    link(i, tid);
                    true
                }
                _ => false,
            }
        }
    };
    irq_restore(flags);
    ok
}

/// The word program `space` is told of signals through, or 0 if it has
/// named none. Interrupts must be off.
pub fn sig_word_of_space(space: u64) -> usize {
    let Some(tid) = crate::scheduler::task_of_space(space) else { return 0 };
    unsafe { table_mut(tid).map_or(0, |t| t.sig_word) }
}

/// The record of the program `tid` is a task of.
///
/// # Safety
/// Interrupts are off for as long as the record is used.
pub unsafe fn table_of(tid: usize) -> Option<&'static mut Program> {
    unsafe {
        let i = st(tid).table;
        if i == NONE { None } else { tables().get(i as usize) }
    }
}

/// Which table a task uses, as a number two tasks of one program agree on.
/// `usize::MAX` for a task with none.
pub fn table_index(tid: usize) -> usize {
    if tid >= MAX_TASKS {
        return usize::MAX;
    }
    let flags = irq_save();
    let i = unsafe { st(tid).table };
    irq_restore(flags);
    if i == NONE { usize::MAX } else { i as usize }
}

/// A task using table `table`, if any does.
pub fn a_task_of(table: usize) -> Option<usize> {
    let flags = irq_save();
    let found = unsafe { tables().get(table).filter(|p| p.first != END).map(|p| p.first as usize) };
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
fn leave(tid: usize) -> Option<(Grow<Fd>, FdKind)> {
    if tid >= MAX_TASKS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let i = st(tid).table;
        if i == NONE {
            None
        } else {
            unlink(i as usize, tid);
            st(tid).table = NONE;
            match table_at(i as usize) {
                Some(t) => {
                    t.tasks = t.tasks.saturating_sub(1);
                    if t.tasks == 0 {
                        // The record goes back, its timers with it, and what
                        // the table held is released by the caller.
                        let held = (t.fds.take(), t.cwd);
                        tables().empty(i as usize);
                        Some(held)
                    } else {
                        None
                    }
                }
                None => None,
            }
        }
    };
    irq_restore(flags);
    out
}

fn release_all((fds, cwd): &(Grow<Fd>, FdKind)) {
    for kind in fds.iter().map(|s| &s.kind).chain(core::iter::once(cwd)) {
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
        let held = unsafe { st(tid).held };
        // And off whatever list its own link names (`waitlist.rs`): a named
        // pipe's other end, say, which it holds nothing for.
        unsafe { crate::waitlist::forget(tid) };
        irq_restore(flags);
        if !held.is_empty() {
            crate::pipe::forget_waiter(&held, tid);
        }
    }
    unhold(tid);
    if let Some(held) = leave(tid) {
        release_all(&held);
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
    let target = unsafe { st(with).table };
    if target == NONE {
        irq_restore(flags);
        return false;
    }
    if unsafe { st(tid).table } == target {
        irq_restore(flags);
        return true;
    }
    // Joined before the old one is left, with the lock held across both, so
    // nothing sees the task with no table.
    unsafe {
        if let Some(t) = table_at(target as usize) {
            t.tasks += 1;
        }
    }
    let old = leave(tid);
    unsafe {
        st(tid).table = target;
        link(target as usize, tid);
    }
    irq_restore(flags);
    if let Some(held) = old {
        release_all(&held);
    }
    true
}

/// Give `child`'s table a copy of everything in `parent`'s, as `fork` does:
/// a second descriptor for each object, the working directory, the
/// close-on-exec marks and the limits included. A descriptor that cannot be
/// copied — a poll set counts no holders — is left out. False, with nothing
/// of the parent's copied, if there was no memory for the child's room.
pub fn copy_into(child: usize, parent: usize) -> bool {
    if child >= MAX_TASKS || parent >= MAX_TASKS {
        return false;
    }
    // One step: a sibling of the parent closing a descriptor between its being
    // read and its being retained would have this retain something freed.
    let flags = irq_save();
    unsafe {
        let src: *const Table = match table_mut(parent) {
            Some(t) => t,
            None => {
                irq_restore(flags);
                return false;
            }
        };
        let (src_umask, src_signals, src_run, src_name, src_trap) = match table_mut(parent) {
            Some(t) => (
                t.umask,
                (t.sig_ignore, t.sig_catch, t.sig_word),
                (t.sig_run, t.sig_masks, t.sig_flags, t.sig_cookies, t.sig_entry, t.sig_unix),
                (t.cmdline, t.cmdline_len),
                (t.trap_from, t.trap_to),
            ),
            None => {
                irq_restore(flags);
                return false;
            }
        };
        if let Some((dst, dst_waiting)) = signals_mut(child) {
            // Room first, as much as the parent has, so a copy either has
            // every descriptor or none of them.
            let room = (*src).fds.len();
            if room > 0 && dst.fds.ensure(room - 1, room, FD_MOST).is_err() {
                irq_restore(flags);
                return false;
            }
            dst.fd_soft = (*src).fd_soft;
            dst.fd_hard = (*src).fd_hard;
            dst.umask = src_umask;
            (dst.cmdline, dst.cmdline_len) = src_name;
            // The child is a copy of the program, handlers and the word they
            // are told through included. What was raised for the parent and
            // not yet taken is the parent's.
            (dst.sig_ignore, dst.sig_catch, dst.sig_word) = src_signals;
            (dst.sig_run, dst.sig_masks, dst.sig_flags, dst.sig_cookies, dst.sig_entry, dst.sig_unix) = src_run;
            (dst.trap_from, dst.trap_to) = src_trap;
            dst.sig_pending = 0;
            dst.sig_held = 0;
            dst_waiting.clear();
            dst.sig_interrupt = false;
            for (i, from) in (*src).fds.iter().enumerate() {
                let Some(to) = dst.fds.get_mut(i) else { break };
                if from.kind.is_empty() || !to.kind.is_empty() {
                    continue;
                }
                if crate::pipe::retain_fd(&from.kind).is_ok() {
                    *to = *from;
                }
            }
            if !(*src).cwd.is_empty() && dst.cwd.is_empty() && crate::pipe::retain_fd(&(*src).cwd).is_ok() {
                dst.cwd = (*src).cwd;
            }
        }
    }
    irq_restore(flags);
    true
}

/// What descriptor `fd` of `tid`'s program names. `FD_CWD` is one too.
pub fn get(tid: usize, fd: usize) -> FdKind {
    let flags = irq_save();
    let kind = unsafe { table_mut(tid).map_or(FdKind::Empty, |t| t.kind(fd)) };
    irq_restore(flags);
    kind
}

/// What `fd` names, with a reference taken for a copy of it: reading the
/// descriptor and retaining what it names are one step, so a sibling closing
/// it in between cannot leave the caller retaining something freed.
pub fn get_retained(tid: usize, fd: usize) -> Option<FdKind> {
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid).map(|t| t.kind(fd)) {
            Some(kind) if !kind.is_empty() => crate::pipe::retain_fd(&kind).is_ok().then_some(kind),
            _ => None,
        }
    };
    irq_restore(flags);
    out
}

/// Put `kind` at `fd`, and return what was there for the caller to release.
/// The slot's close-on-exec mark is cleared: it belonged to what was there.
/// Refused at or above the program's limit, or with no memory for the room.
pub fn replace(tid: usize, fd: usize, kind: FdKind) -> Result<FdKind, ()> {
    let flags = irq_save();
    let out = unsafe { table_mut(tid).map_or(Err(()), |t| t.put(fd, kind)) };
    irq_restore(flags);
    out
}

/// Put `new` at `fd` if `expected` is what is there, keeping the slot's
/// close-on-exec mark: the same descriptor, become what its object became.
/// What was there is the caller's to release. False, and nothing changed,
/// if the slot holds something else.
pub fn swap_if(tid: usize, fd: usize, expected: FdKind, new: FdKind) -> bool {
    let flags = irq_save();
    let swapped = unsafe {
        table_mut(tid).is_some_and(|t| {
            let slot = if fd == FD_CWD { Some(&mut t.cwd) } else { t.fds.get_mut(fd).map(|s| &mut s.kind) };
            match slot {
                Some(k) if *k == expected => {
                    *k = new;
                    true
                }
                _ => false,
            }
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
        table_mut(tid).and_then(|t| {
            let fd = t.lowest_free(floor)?;
            t.put(fd, kind).ok().map(|_| fd)
        })
    };
    irq_restore(flags);
    out
}

/// The lowest free descriptor at or above `floor`, for a caller that has to
/// know the number before it has the object. Somebody else may take it before
/// the caller does; `replace` then closes what they put there, as `dup2` would.
pub fn free_at_or_above(tid: usize, floor: usize) -> Option<usize> {
    let flags = irq_save();
    let out = unsafe { table_mut(tid).and_then(|t| t.lowest_free(floor)) };
    irq_restore(flags);
    out
}

/// `tid`'s program's descriptor limits: what it may have, and how far it may
/// raise that.
pub fn limit_of(tid: usize) -> Option<(usize, usize)> {
    let flags = irq_save();
    let out = unsafe { table_mut(tid).map(|t| (t.fd_soft as usize, t.fd_hard as usize)) };
    irq_restore(flags);
    out
}

/// Set what `tid`'s program may have, no higher than how far it may raise it.
pub fn set_soft_limit(tid: usize, n: usize) -> bool {
    let flags = irq_save();
    let ok = unsafe {
        table_mut(tid).is_some_and(|t| {
            let ok = n <= t.fd_hard as usize;
            if ok {
                t.fd_soft = n as u32;
            }
            ok
        })
    };
    irq_restore(flags);
    ok
}

/// Lower how far `tid`'s program may raise its limit, never below the limit.
pub fn lower_hard_limit(tid: usize, n: usize) -> bool {
    let flags = irq_save();
    let ok = unsafe {
        table_mut(tid).is_some_and(|t| {
            let ok = n >= t.fd_soft as usize && n <= t.fd_hard as usize;
            if ok {
                t.fd_hard = n as u32;
            }
            ok
        })
    };
    irq_restore(flags);
    ok
}

/// Whether any descriptor of `tid`'s program — the working directory
/// included — is one `wanted` says yes to.
pub fn any(tid: usize, wanted: impl Fn(&FdKind) -> bool) -> bool {
    let flags = irq_save();
    let found = unsafe {
        table_mut(tid).is_some_and(|t| {
            t.fds.iter().map(|s| &s.kind).chain(core::iter::once(&t.cwd)).any(|k| !k.is_empty() && wanted(k))
        })
    };
    irq_restore(flags);
    found
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
        let mut found = None;
        for i in 0..tables().high() {
            let Some(p) = tables().get(i) else { continue };
            let t = &mut p.table;
            if t.tasks == 0 || t.alarm_at == 0 || t.alarm_at > now {
                continue;
            }
            t.alarm_at = alarm_next(t.alarm_at, t.alarm_every, now);
            found = (p.first != END).then_some(p.first as usize);
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
        for i in 0..tables().high() {
            let Some(t) = table_at(i) else { continue };
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

/// Calls made by `tid`'s program from outside `from..to` raise SIGSYS
/// rather than being made; `from == to` for none (`signal::trap_call`).
pub fn set_trap(tid: usize, from: usize, to: usize) -> bool {
    let flags = irq_save();
    let set = unsafe {
        table_mut(tid).map(|t| {
            t.trap_from = from;
            t.trap_to = to;
        })
    };
    irq_restore(flags);
    set.is_some()
}

/// Where `tid`'s program has said its calls are made from, if it has.
pub fn trap_of(tid: usize) -> Option<(usize, usize)> {
    let flags = irq_save();
    let range = unsafe {
        table_mut(tid).and_then(|t| (t.trap_from != t.trap_to).then_some((t.trap_from, t.trap_to)))
    };
    irq_restore(flags);
    range
}

/// Whether `fd` is closed when the program becomes another.
pub fn cloexec(tid: usize, fd: usize) -> Option<bool> {
    let flags = irq_save();
    let out = unsafe {
        match table_mut(tid).and_then(|t| t.fds.get(fd)) {
            Some(s) if !s.kind.is_empty() => Some(s.cloexec),
            _ => None,
        }
    };
    irq_restore(flags);
    out
}

pub fn set_cloexec(tid: usize, fd: usize, on: bool) -> bool {
    let flags = irq_save();
    let ok = unsafe {
        match table_mut(tid).and_then(|t| t.fds.get_mut(fd)) {
            Some(s) if !s.kind.is_empty() => {
                s.cloexec = on;
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
    // What is closed, released once the lock is given up, as `leave`'s is.
    // Room for it is the heap's; where there is none, it is released here.
    let mut gone = Grow::new(FdKind::Empty);
    let mut n = 0;
    let flags = irq_save();
    unsafe {
        if let Some((t, w)) = signals_mut(tid) {
            for s in t.fds.iter_mut().filter(|s| s.cloexec) {
                let kind = core::mem::replace(s, NO_FD).kind;
                match gone.ensure(n, FIRST, FD_MOST) {
                    Ok(g) => {
                        *g = kind;
                        n += 1;
                    }
                    Err(_) if !kind.is_empty() => crate::pipe::release_fd(&kind),
                    Err(_) => {}
                }
            }
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
            // And where its calls were made from.
            t.trap_from = 0;
            t.trap_to = 0;
        }
        // And its timers, which were set for the program that has gone.
        if let Some(p) = table_of(tid) {
            p.timers.clear();
        }
    }
    irq_restore(flags);
    for kind in gone.iter().take(n).filter(|k| !k.is_empty()) {
        crate::pipe::release_fd(kind);
    }
}

/// A signal has arrived for `tid`, which may be parked on what it is using:
/// if it is, it is taken off that list and woken, and goes to see. True if
/// it was.
pub fn interrupt(tid: usize) -> bool {
    if tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let held = unsafe { st(tid).held };
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
    if tid >= MAX_TASKS || fd >= FD_MOST {
        return FdKind::Empty;
    }
    let flags = irq_save();
    let kind = unsafe { table_mut(tid).map_or(FdKind::Empty, |t| t.kind(fd)) };
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
        unsafe { st(tid).held = kind };
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
        core::mem::replace(&mut st(tid).held, FdKind::Empty)
    };
    irq_restore(flags);
    if !kind.is_empty() {
        crate::pipe::release_fd(&kind);
    }
}
