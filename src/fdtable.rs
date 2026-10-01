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
}

const EMPTY: Table = Table { tasks: 0, fds: [FdKind::Empty; SLOTS], cloexec: 0 };

/// As many tables as tasks: each task uses exactly one.
static mut TABLES: [Table; MAX_TASKS] = [EMPTY; MAX_TASKS];
/// Which table each task uses.
static mut OF_TASK: [u16; MAX_TASKS] = [NONE; MAX_TASKS];
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
pub fn table_of(tid: usize) -> usize {
    if tid >= MAX_TASKS {
        return usize::MAX;
    }
    let flags = irq_save();
    let i = unsafe { (*core::ptr::addr_of!(OF_TASK))[tid] };
    irq_restore(flags);
    if i == NONE { usize::MAX } else { i as usize }
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
        let (src_fds, src_cloexec) = match table_mut(parent) {
            Some(t) => (t.fds, t.cloexec),
            None => {
                irq_restore(flags);
                return;
            }
        };
        if let Some(dst) = table_mut(child) {
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
        if let Some(t) = table_mut(tid) {
            for fd in 0..SLOTS {
                if t.cloexec & (1u128 << fd) != 0 {
                    gone[fd] = core::mem::replace(&mut t.fds[fd], FdKind::Empty);
                }
            }
            t.cloexec = 0;
        }
    }
    irq_restore(flags);
    release_all(&gone);
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
