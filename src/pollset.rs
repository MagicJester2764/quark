//! Waiting on more than one descriptor.
//!
//! A set is itself a descriptor, which is what makes it composable and is why
//! this is epoll-shaped rather than a call taking an array: the set is built
//! once and waited on many times, instead of being marshalled across the
//! boundary on every turn of a loop that runs once per frame. The call that
//! takes an array exists too, beside this one, because `poll(2)` is what most
//! software actually calls.
//!
//! Readiness is evaluated at wake-up rather than stored. A stored bit has to be
//! invalidated by everything that could change it, and the way to get that
//! wrong is a task that sleeps through data already waiting for it. A set's
//! entries scanned is cheaper than being wrong.
//!
//! What is stored is whether something happened. A watch is reported at every
//! wait while what it watches is ready, unless it says otherwise: an edge
//! ([`EDGE`], epoll's EPOLLET) is reported when what it watches has been
//! noted — written, read, said to be ready, the `note_*` functions below —
//! since it was last looked at, and is ready then; a one-shot ([`ONCE`]) is
//! reported once, and then not until it is modified. A change has to reach
//! an edge whether or not anybody is waiting, so the sets that have one are
//! looked at by every change, as the sets somebody is parked on are.
//!
//! And a set is something a set can watch: ready while a wait on it would
//! report something. A chain of them goes no deeper than Linux lets it
//! ([`DEEPEST`]), and no set watches itself, through others or not.

use crate::grow::Grow;
use crate::task::FdKind;
use crate::waitlist::{self, On, Waiters};
use crate::{pipe, stream};

/// How many sets a chain of them may have below the one waited on: Linux's
/// EP_MAX_NESTS.
const DEEPEST: usize = 4;

pub const READABLE: u32 = 1;
pub const WRITABLE: u32 = 2;
pub const HANGUP: u32 = 4;
/// A descriptor that cannot be waited on at all.
///
/// `poll(2)` reports this in `revents` rather than failing the call, because
/// one bad entry should not deny the caller the answer about the others. The
/// set-shaped interface refuses at `ctl` time instead: there the caller is
/// building something to reuse, and a watch that can never fire is a mistake
/// worth hearing about once rather than on every wait.
pub const INVALID: u32 = 8;
/// Asked for beside READABLE, and said beside a hangup: epoll's EPOLLRDHUP,
/// the other end gone.
pub const PEER_GONE: u32 = 0x10;
/// In what a watch is for: reported when what it watches has been noted
/// since it was last looked at (EPOLLET).
pub const EDGE: u32 = 1 << 16;
/// In what a watch is for: reported once, and then not until it is modified
/// (EPOLLONESHOT).
pub const ONCE: u32 = 1 << 17;

#[derive(Clone, Copy)]
struct Watch {
    fd: usize,
    /// What it is for: READABLE, WRITABLE, PEER_GONE, and how — EDGE, ONCE.
    events: u32,
    token: u64,
    used: bool,
    /// An edge's: what it watches has been noted since it was last looked
    /// at, or the watch is new or modified.
    stirred: bool,
    /// A one-shot's: reported, and quiet until it is modified.
    spent: bool,
}

const UNUSED: Watch = Watch { fd: 0, events: 0, token: 0, used: false, stirred: false, spent: false };

struct PollSet {
    /// The descriptor table its watches are numbers in — a program's, so any
    /// thread of the program that made the set may use it.
    owner: usize,
    /// Some watch is an edge, which every change that reaches it has to stir.
    edges: bool,
    /// What it watches, room made as watches are added — one removed leaves
    /// its room to the next — and never more than a program can have
    /// descriptors. There was room for thirty-two.
    watches: Grow<Watch>,
    /// Who is parked on it (`waitlist.rs`), asleep in a receive from
    /// themselves. There was one.
    waiters: Waiters,
}

/// Every set, from its making to its descriptor's close — or the end of the
/// `poll` that made it (`table.rs`). There were sixty-four.
static mut SETS: crate::table::Table<PollSet> = crate::table::Table::new(crate::table::MOST);

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

#[inline(always)]
unsafe fn sets() -> &'static mut crate::table::Table<PollSet> { unsafe {
    &mut *core::ptr::addr_of_mut!(SETS)
}}

/// Set `set`, if there is one.
///
/// # Safety
/// Interrupts off.
#[inline(always)]
unsafe fn set_at(set: usize) -> Option<&'static mut PollSet> {
    unsafe { sets().get(set) }
}

/// A set for `tid`'s program. One a program keeps (`SYS_POLLSET_CREATE`) is
/// made as whatever a program makes is, through `reclaim`; one made for the
/// length of a call (`SYS_POLL`, `for_a_call`) is not: refused while memory
/// is short, a poll would fail a main loop over a moment's pressure.
pub fn create(tid: usize, for_a_call: bool) -> Option<usize> {
    let table = crate::fdtable::table_index(tid);
    if table == usize::MAX || (!for_a_call && !crate::reclaim::may_make()) {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        sets().lowest_free(0).filter(|&i| {
            let s = PollSet { owner: table, edges: false, watches: Grow::new(UNUSED), waiters: Waiters::NONE };
            sets().fill_at(i, s).is_ok()
        })
    };
    irq_restore(flags);
    out
}

/// The set goes, with its watches. Whoever is parked on it — a thread whose
/// sibling closed it — is woken to look again, and finds it gone.
pub fn destroy(set: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(s) = set_at(set) {
            waitlist::take_each(&mut s.waiters, |t| {
                crate::ipc::wake_sleeper(t);
            });
            sets().empty(set);
        }
    }
    irq_restore(flags);
}

/// Can this descriptor ever become ready?
///
/// Only the kinds with a buffer can. An IPC endpoint has nothing that becomes
/// ready, so adding one is refused here rather than reported as never ready —
/// a caller waiting for ever on something that cannot arrive deserves an error
/// and not silence.
pub fn watchable(tid: usize, fd: usize) -> bool {
    if fd >= crate::task::FD_MOST {
        return false;
    }
    matches!(
        crate::fdtable::get(tid, fd),
        FdKind::PipeRead(_)
            | FdKind::PipeWrite(_)
            | FdKind::StreamEnd { .. }
            | FdKind::PtyEnd { .. }
            | FdKind::Timer { .. }
            | FdKind::Event { .. }
            | FdKind::Served { .. }
            | FdKind::Signals { .. }
            | FdKind::Local { .. }
            | FdKind::PollSet { .. }
    )
}

/// Why a watch was not added, modified or removed: what SYS_POLLSET_CTL says
/// to a caller that asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Not a set of the caller's program; or the set itself, to be watched.
    NotOne = 1,
    /// It is watched already.
    Exists = 2,
    /// It is not watched.
    Absent = 3,
    /// It can never be ready: a file, memory, an endpoint.
    Cannot = 4,
    /// A set that would lead back to this one, or a chain of sets deeper
    /// than [`DEEPEST`].
    Loop = 5,
    /// The set watches as many as it can.
    Full = 6,
}

/// Watch `i` of `set`, as it is now: unused, past the end or with no set.
fn watch(set: usize, i: usize) -> Watch {
    let flags = irq_save();
    let w = unsafe { set_at(set).and_then(|s| s.watches.get(i).copied()).unwrap_or(UNUSED) };
    irq_restore(flags);
    w
}

/// How much room `set` has for watches: one past the last there can be.
fn room(set: usize) -> usize {
    let flags = irq_save();
    let n = unsafe { set_at(set).map_or(0, |s| s.watches.len()) };
    irq_restore(flags);
    n
}

/// Change watch `i` of `set`, if it is still the one `was` was: what a scan
/// found may have been changed by another thread of the program since.
fn update(set: usize, i: usize, was: &Watch, change: impl FnOnce(&mut Watch)) {
    let flags = irq_save();
    unsafe {
        if let Some(w) = set_at(set).and_then(|s| s.watches.get_mut(i)) {
            if w.used && w.fd == was.fd && w.token == was.token {
                change(w);
            }
        }
    }
    irq_restore(flags);
}

/// A task whose table `set`'s watches are numbers in.
fn owner_task(set: usize) -> Option<usize> {
    let flags = irq_save();
    let owner = unsafe { set_at(set).map(|s| s.owner) };
    irq_restore(flags);
    crate::fdtable::a_task_of(owner?)
}

/// The set descriptor `fd` of `tid`'s table names, if it names one.
fn set_named(tid: usize, fd: usize) -> Option<usize> {
    if fd >= crate::task::FD_MOST {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        FdKind::PollSet { set } => Some(set),
        _ => None,
    }
}

/// Whether `set`, `level` sets below the one that would watch it, may be
/// watched by `target`: it does not lead back to `target` through the sets
/// it watches, and no chain of them would go deeper than [`DEEPEST`].
fn may_go_under(set: usize, target: usize, level: usize) -> bool {
    if set == target || level > DEEPEST {
        return false;
    }
    let Some(tid) = owner_task(set) else { return true };
    (0..room(set)).all(|i| {
        let w = watch(set, i);
        !w.used || set_named(tid, w.fd).is_none_or(|inner| may_go_under(inner, target, level + 1))
    })
}

/// op: 0 add, 1 modify, 2 remove. A watch added or modified is ready to be
/// reported: an edge is reported if what it watches is ready, as Linux has
/// it, and a one-shot is armed again.
pub fn ctl(set: usize, tid: usize, op: u64, fd: usize, events: u32, token: u64) -> Result<(), Refused> {
    if op > 2 {
        return Err(Refused::NotOne);
    }
    // A set watched by a set: never itself, and never one that leads back.
    if op == 0 {
        if let Some(inner) = set_named(tid, fd) {
            if inner == set {
                return Err(Refused::NotOne);
            }
            if !may_go_under(inner, set, 1) {
                return Err(Refused::Loop);
            }
        }
    }
    let table = crate::fdtable::table_index(tid);
    let flags = irq_save();
    let out = unsafe {
        match set_at(set) {
            Some(s) if s.owner == table => {
                let at = s.watches.iter().position(|w| w.used && w.fd == fd);
                let done = match (op, at) {
                    (2, Some(i)) => {
                        if let Some(w) = s.watches.get_mut(i) {
                            *w = UNUSED;
                        }
                        Ok(())
                    }
                    (1, Some(i)) => {
                        if let Some(w) = s.watches.get_mut(i) {
                            w.events = events;
                            w.token = token;
                            w.stirred = true;
                            w.spent = false;
                        }
                        Ok(())
                    }
                    (_, None) if op != 0 => Err(Refused::Absent),
                    (_, Some(_)) => Err(Refused::Exists),
                    // In the room of one removed, or in new room: none past
                    // as many descriptors as a program can have, and none
                    // with no memory for it.
                    (_, None) => {
                        let free = s.watches.iter().position(|w| !w.used).unwrap_or(s.watches.len());
                        match s.watches.ensure(free, 8, crate::task::FD_MOST) {
                            Ok(w) => {
                                *w = Watch { fd, events, token, used: true, stirred: true, spent: false };
                                Ok(())
                            }
                            Err(_) => Err(Refused::Full),
                        }
                    }
                };
                s.edges = s.watches.iter().any(|w| w.used && w.events & EDGE != 0);
                done
            }
            _ => Err(Refused::NotOne),
        }
    };
    irq_restore(flags);
    out
}

/// What a descriptor can do right now, `level` sets below the one waited on.
fn readiness_at(tid: usize, fd: usize, level: usize) -> u32 {
    if fd >= crate::task::FD_MOST {
        return 0;
    }
    let kind = crate::fdtable::get(tid, fd);
    let mut out = 0;
    match kind {
        FdKind::PipeRead(h) => {
            if pipe::readable(h) {
                out |= READABLE;
            }
            if pipe::ended(h) {
                out |= HANGUP;
            }
        }
        FdKind::PipeWrite(h) => {
            if pipe::writable(h) {
                out |= WRITABLE;
            }
            if pipe::no_readers(h) {
                out |= HANGUP;
            }
        }
        FdKind::StreamEnd { stream: s, end } => {
            if stream::readable(s, end) {
                out |= READABLE;
            }
            if stream::writable(s, end) {
                out |= WRITABLE;
            }
            if stream::peer_gone(s, end) {
                out |= HANGUP;
            }
        }
        FdKind::Timer { timer } => {
            if crate::timerfd::pending(timer) > 0 {
                out |= READABLE;
            }
        }
        FdKind::Event { ev } => {
            if crate::eventfd::readable(ev) {
                out |= READABLE;
            }
            if crate::eventfd::writable(ev) {
                out |= WRITABLE;
            }
        }
        FdKind::PtyEnd { pty, end } => {
            // A read that would return at once is readable, whether it has
            // bytes or an end of file somebody typed. The other end having
            // gone is that and a hangup — which a terminal emulator reads as
            // its shell having exited.
            use crate::pty::Pending;
            match crate::pty::readable(pty, end) {
                Pending::Bytes(0) => {}
                Pending::Bytes(_) | Pending::End => out |= READABLE,
                Pending::Gone => out |= READABLE | HANGUP,
            }
            if crate::pty::writable(pty, end) {
                out |= WRITABLE;
            }
        }
        // A file never keeps anybody waiting: a read answers with what is
        // there, the end included, and a write is taken. What is not a file
        // is what its server last said.
        FdKind::Served { obj } => out |= crate::served::readiness(obj).unwrap_or(READABLE | WRITABLE),
        // A listener with a connection waiting to be accepted.
        FdKind::Local { l } => {
            if crate::local::readable(l) {
                out |= READABLE;
            }
        }
        // Whose signals are the reader's: here the one asking.
        FdKind::Signals { sfd } => {
            if crate::signal::waiting_for(tid) & crate::sigfd::mask(sfd) != 0 {
                out |= READABLE;
            }
        }
        // A set is readable while a wait on it would report something.
        FdKind::PollSet { set } => {
            if level <= DEEPEST && reports(set, level + 1) {
                out |= READABLE;
            }
        }
        _ => {}
    }
    out
}

/// What watch `w` would report of what `tid`'s descriptor is now, the
/// descriptor `level` sets down.
fn hit(tid: usize, w: &Watch, level: usize) -> u32 {
    let r = readiness_at(tid, w.fd, level);
    // Hangup is reported whether it was asked for or not: a caller waiting
    // for readable on a descriptor whose peer has gone would otherwise be
    // waiting for something that can never arrive.
    let mut hit = (r & w.events & (READABLE | WRITABLE)) | (r & HANGUP);
    if hit & HANGUP != 0 && w.events & PEER_GONE != 0 {
        hit |= PEER_GONE;
    }
    hit
}

/// Whether a wait on `set`, `level` sets down, would report anything now.
/// It takes nothing: an edge stays stirred, a one-shot armed.
fn reports(set: usize, level: usize) -> bool {
    let Some(tid) = owner_task(set) else { return false };
    (0..room(set)).any(|i| {
        let w = watch(set, i);
        w.used && !w.spent && (w.events & EDGE == 0 || w.stirred) && hit(tid, &w, level) != 0
    })
}

/// What one descriptor can do now, for callers with no set.
pub fn readiness_of(tid: usize, fd: usize) -> u32 {
    readiness_at(tid, fd, 1)
}

/// Register as a waiter on a set, before the last scan and the block.
///
/// The order matters and is the whole of why this is correct: anything that
/// becomes ready after this either happened before the scan that follows — so
/// the scan sees it and we never block — or after it, and then `note_pipe`
/// finds us parked and wakes us. There is no window between looking and
/// sleeping.
///
/// Any number may be parked on one set, each woken by what they are all
/// waiting for, as epoll's are: one waiter a set, and a second thread
/// waiting on it took the first one's place, which was then woken by
/// nothing.
pub fn park(set: usize, tid: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(s) = set_at(set) {
            waitlist::add(&mut s.waiters, tid, On::PollSet(set as u32));
        }
    }
    irq_restore(flags);
}

pub fn unpark(set: usize, tid: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(s) = set_at(set) {
            waitlist::remove(&mut s.waiters, tid, On::PollSet(set as u32));
        }
    }
    irq_restore(flags);
}

/// Set `set`'s waiters. For `waitlist::forget`.
///
/// # Safety
/// Interrupts off.
pub unsafe fn waiters(set: usize) -> Option<&'static mut Waiters> {
    unsafe { set_at(set).map(|s| &mut s.waiters) }
}

/// Wake everybody parked on `set`, to look again. They stay on its list:
/// each takes itself off when it has looked.
fn wake_parked(set: usize) {
    let flags = irq_save();
    unsafe {
        if let Some(s) = set_at(set) {
            waitlist::each(&s.waiters, |t| {
                crate::ipc::wake_sleeper(t);
            });
        }
    }
    irq_restore(flags);
}

/// Does this task's descriptor `fd` name pipe `handle`?
fn names_pipe(tid: usize, fd: usize, handle: usize) -> bool {
    if fd >= crate::task::FD_MOST {
        return false;
    }
    let kind = crate::fdtable::get(tid, fd);
    match kind {
        FdKind::PipeRead(h) | FdKind::PipeWrite(h) => h == handle,
        FdKind::StreamEnd { stream: s, end } => match stream::pipes_for(s, end) {
            Some((rd, wr)) => rd == handle || wr == handle,
            None => false,
        },
        _ => false,
    }
}

/// A pipe changed state. Wake any set watching a descriptor that names it.
///
/// Sets are scanned rather than pipes carrying a list of their watchers: the
/// alternative puts a back pointer in every pipe for the benefit of the rare
/// one anybody watches. Only sets somebody is parked on, or that have an
/// edge, are looked at, so a system with nobody waiting pays a walk of the
/// sets that are there.
pub fn note_pipe(handle: usize) {
    note(|tid, fd| names_pipe(tid, fd, handle));
}

/// Whether descriptor `fd` of `tid`'s table is what changed, or a set that
/// leads to it, `level` sets down.
fn leads_to(tid: usize, fd: usize, names: &dyn Fn(usize, usize) -> bool, level: usize) -> bool {
    if names(tid, fd) {
        return true;
    }
    if level > DEEPEST {
        return false;
    }
    let Some(inner) = set_named(tid, fd) else { return false };
    let Some(owner) = owner_task(inner) else { return false };
    (0..room(inner)).any(|i| {
        let w = watch(inner, i);
        w.used && leads_to(owner, w.fd, names, level + 1)
    })
}

/// Something `names` changed: stir every edge that watches it — in a set
/// somebody waits on or not, directly or through a set it watches — and
/// wake every task parked on a set that watches it.
///
/// The watches are looked at one at a time with the lock held, and matched
/// without it, because matching reaches into the descriptor tables and the
/// stream table. The set a task is parked on is the one whose watches are
/// looked at — it used to be the first set that task owned, which is a
/// different set for a program holding two.
fn note(names: impl Fn(usize, usize) -> bool) {
    // The sets to look at, one at a time: those somebody is parked on, and
    // those with an edge.
    let mut at = 0;
    loop {
        let flags = irq_save();
        let next = unsafe {
            let mut found = None;
            while let Some(i) = sets().next_used(at) {
                at = i + 1;
                if set_at(i).is_some_and(|s| !s.waiters.is_empty() || s.edges) {
                    found = Some(i);
                    break;
                }
            }
            found
        };
        irq_restore(flags);
        let Some(set) = next else { break };

        let Some(tid) = owner_task(set) else { continue };
        let mut found = false;
        for i in 0..room(set) {
            let w = watch(set, i);
            if !w.used || !leads_to(tid, w.fd, &names, 1) {
                continue;
            }
            found = true;
            if w.events & EDGE != 0 {
                update(set, i, &w, |w| w.stirred = true);
            }
        }
        if found {
            wake_parked(set);
        }
    }
}

/// Wake whoever is parked on any set, to look again.
fn wake_all() {
    let flags = irq_save();
    unsafe {
        let mut at = 0;
        while let Some(i) = sets().next_used(at) {
            at = i + 1;
            if let Some(s) = set_at(i) {
                waitlist::each(&s.waiters, |t| {
                    crate::ipc::wake_sleeper(t);
                });
            }
        }
    }
    irq_restore(flags);
}

/// Something changed at one end of a pty: wake whoever is waiting on a set
/// that watches it.
///
/// The twin of `note_pipe`, and for the same reason — a set is waited on by a
/// task that is asleep, and nothing else would tell it that a shell has
/// printed something or gone.
pub fn note_pty(pty: usize) {
    note(|tid, fd| names_pty(tid, fd, pty));
}

/// A timer fired: wake whoever is waiting on a set, and let the scan decide
/// whether it was one of theirs. Unlike a pipe or a pty there is no handle to
/// match on here, because the tick fires every armed timer there is and the
/// scan is cheaper than working out whose. An edge on a timer is stirred by
/// any of them firing, and the scan says whether its own is ready.
pub fn note_timer() {
    note(|tid, fd| fd < crate::task::FD_MOST && matches!(crate::fdtable::get(tid, fd), FdKind::Timer { .. }));
    wake_all();
}

/// A signal has come to wait for a program, where a signal descriptor's
/// reader would take it: wake whoever is waiting on a set that watches one,
/// and let the scan say whose it was.
pub fn note_signals() {
    note(|tid, fd| fd < crate::task::FD_MOST && matches!(crate::fdtable::get(tid, fd), FdKind::Signals { .. }));
}

/// What served object `obj` is ready for has changed, as its server said:
/// wake whoever is waiting on a set that watches it.
pub fn note_served(obj: usize) {
    note(|tid, fd| fd < crate::task::FD_MOST && crate::fdtable::get(tid, fd) == FdKind::Served { obj });
}

/// A connection has come to wait on listener `l`: wake whoever is waiting on
/// a set that watches it.
pub fn note_local(l: usize) {
    note(|tid, fd| fd < crate::task::FD_MOST && crate::fdtable::get(tid, fd) == FdKind::Local { l });
}

/// A counter was added to or taken from: same reasoning as `note_timer`, and
/// the same scan: finding out which sets name this one costs more than
/// waking them to look.
pub fn note_event() {
    note(|tid, fd| fd < crate::task::FD_MOST && matches!(crate::fdtable::get(tid, fd), FdKind::Event { .. }));
    wake_all();
}

fn names_pty(tid: usize, fd: usize, pty: usize) -> bool {
    if fd >= crate::task::FD_MOST {
        return false;
    }
    let kind = crate::fdtable::get(tid, fd);
    matches!(kind, FdKind::PtyEnd { pty: p, .. } if p == pty)
}

/// Collect what is ready: up to `most` of it, each handed to `out` with its
/// token. Returns how many were.
///
/// This takes what it reports: an edge looked at waits for the next noting,
/// ready now or not, and a one-shot reported is quiet until it is modified.
/// One there was no room for is left as it was, for the next wait.
pub fn scan(set: usize, tid: usize, most: usize, mut out: impl FnMut(u64, u32)) -> usize {
    let table = crate::fdtable::table_index(tid);
    let flags = irq_save();
    let mine = unsafe { set_at(set).is_some_and(|s| s.owner == table) };
    irq_restore(flags);
    if !mine {
        return 0;
    }

    let mut n = 0;
    for i in 0..room(set) {
        if n == most {
            break;
        }
        let w = watch(set, i);
        if !w.used || w.spent {
            continue;
        }
        let edge = w.events & EDGE != 0;
        if edge && !w.stirred {
            continue;
        }
        let hit = hit(tid, &w, 1);
        if edge {
            update(set, i, &w, |w| w.stirred = false);
        }
        if hit == 0 {
            continue;
        }
        out(w.token, hit);
        n += 1;
        if w.events & ONCE != 0 {
            update(set, i, &w, |w| w.spent = true);
        }
    }
    n
}
