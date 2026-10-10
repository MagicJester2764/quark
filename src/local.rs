//! Local sockets that are found by a name: what `bind`, `listen`, `accept`
//! and `connect` are for a socket of the family Unix calls local.
//!
//! A connected local socket is a stream (`stream.rs`), which is what
//! `socketpair` makes. What is here is the time before: a socket that is
//! nothing yet, one with a name, and one that listens for connections. The
//! name is a file server's — an inode whose mode says it is a socket, as a
//! named pipe's says it is a pipe — and the server joins the two: it binds a
//! socket a task that is calling it holds to a key of its own ([`bind`]), and
//! connects one to whatever is listening at a key ([`connect`]), having
//! decided by the inode's owner and mode whether the task may. A listener is
//! known by the server's endpoint, which is never given out again, and the
//! key.
//!
//! A connection is made at once, whether anybody is accepting or not: a
//! stream, one end of which becomes the connector's socket where it was in
//! its table, and the other waits in the listener's queue until `accept`
//! installs it. Each end is told who is at the other: the listener as it was
//! when it began to listen, the connector as it was when it connected.

use crate::grow::Grow;
use crate::stream::Creds;
use crate::waitlist::{self, On, Waiters};

/// Connections waiting to be accepted: the most a listener has, whatever
/// backlog it asks for — Linux's `somaxconn`. There were sixteen.
const QUEUE: usize = 4096;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Unbound,
    Bound,
    Listening,
}

struct Local {
    state: State,
    refs: usize,
    /// The server that named it, by endpoint, and the key it named it by.
    server: u64,
    key: u64,
    /// Who listens, as it was when it began to.
    creds: Creds,
    /// It has asked to be told who sent what it receives (`SO_PASSCRED`),
    /// which what it accepts is asked for too.
    passcred: bool,
    backlog: usize,
    /// Streams made and not yet accepted, whose end 1 is the accepter's,
    /// longest waiting first: room made as they come, up to the backlog.
    pending: Grow<usize>,
    npending: usize,
    /// Who is waiting to accept (`waitlist.rs`). There were four.
    waiters: Waiters,
}

impl Local {
    const fn new() -> Self {
        Local {
            state: State::Unbound,
            refs: 1,
            server: 0,
            key: 0,
            creds: Creds { pid: 0, uid: 0, gid: 0 },
            passcred: false,
            backlog: 0,
            pending: Grow::new(0),
            npending: 0,
            waiters: Waiters::NONE,
        }
    }
}

/// Every local socket, from its making to its last descriptor's close
/// (`table.rs`). There were thirty-two, named or not.
static mut LOCALS: crate::table::Table<Local> = crate::table::Table::new(crate::table::MOST);

/// The local sockets' lock (`sync::RANK_LOCAL`): the table, every socket in it
/// and the lists of those waiting to accept.
static LOCK: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(crate::sync::RANK_LOCAL, "the local sockets", ());



/// # Safety
/// Interrupts are off.
unsafe fn locals() -> &'static mut crate::table::Table<Local> {
    unsafe { &mut *core::ptr::addr_of_mut!(LOCALS) }
}

/// Socket `l`, if there is one.
///
/// # Safety
/// Interrupts are off.
unsafe fn local(l: usize) -> Option<&'static mut Local> {
    unsafe { locals().get(l) }
}

/// The socket named `key` by `server`, if one is, listening or not.
///
/// # Safety
/// Interrupts are off.
unsafe fn named(server: u64, key: u64) -> Option<usize> {
    unsafe {
        let mut at = 0;
        while let Some(l) = locals().next_used(at) {
            at = l + 1;
            if local(l).is_some_and(|o| o.state != State::Unbound && o.server == server && o.key == key) {
                return Some(l);
            }
        }
        None
    }
}

/// A new socket that is nothing yet, with one reference: the descriptor
/// about to name it. `None` with no room for it, as `reclaim` decides for
/// whatever a program makes.
pub fn create() -> Option<usize> {
    if !crate::reclaim::may_make() {
        return None;
    }
    let held = LOCK.lock();
    let made = unsafe { locals().lowest_free(0).filter(|&l| locals().fill_at(l, Local::new()).is_ok()) };
    drop(held);
    made
}

/// Another descriptor names `l`.
pub fn retain(l: usize) {
    let held = LOCK.lock();
    unsafe {
        if let Some(it) = local(l) {
            it.refs += 1;
        }
    }
    drop(held);
}

/// A descriptor for `l` is gone. The last takes it with it, and with it
/// every connection still waiting to be accepted: each connector finds the
/// other end gone.
pub fn release(l: usize) {
    let held = LOCK.lock();
    let pending = unsafe {
        match local(l) {
            Some(it) => {
                it.refs = it.refs.saturating_sub(1);
                if it.refs > 0 {
                    None
                } else {
                    // Whoever waits to accept on it looks again, and finds
                    // it gone.
                    waitlist::wake_all(&mut it.waiters);
                    let n = it.npending;
                    let pending = it.pending.take();
                    locals().empty(l);
                    Some((pending, n))
                }
            }
            None => None,
        }
    };
    drop(held);
    if let Some((pending, n)) = pending {
        for &stream in pending.iter().take(n) {
            crate::stream::close_end(stream, 1);
        }
    }
}

/// Why a name could not be given or reached.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refused {
    /// Not a socket that could be: one with a name already, or connected.
    NotOne,
    /// The name is another socket's.
    Taken,
    /// Nobody listens at the name.
    Nobody,
    /// Somebody does, with as many connections waiting as it has room for.
    Full,
}

/// `l` is named `key` by the server whose endpoint is `server`.
pub fn bind(l: usize, server: u64, key: u64) -> Result<(), Refused> {
    let held = LOCK.lock();
    let out = unsafe {
        if !local(l).is_some_and(|it| it.state == State::Unbound) {
            Err(Refused::NotOne)
        } else if named(server, key).is_some() {
            Err(Refused::Taken)
        } else if let Some(it) = local(l) {
            it.state = State::Bound;
            it.server = server;
            it.key = key;
            Ok(())
        } else {
            Err(Refused::NotOne)
        }
    };
    drop(held);
    out
}

/// `l`, which has a name, listens, with room for `backlog` connections
/// waiting (and never more than [`QUEUE`]); `creds` is who it is, which
/// every connector is told. Listening again changes the room.
pub fn listen(l: usize, backlog: usize, creds: Creds) -> bool {
    let held = LOCK.lock();
    let ok = unsafe {
        match local(l) {
            Some(it) if matches!(it.state, State::Bound | State::Listening) => {
                it.state = State::Listening;
                it.backlog = backlog.clamp(1, QUEUE);
                it.creds = creds;
                true
            }
            _ => false,
        }
    };
    drop(held);
    ok
}

/// The listener at `key` for `server`, and whether it has room for another
/// connection.
///
/// # Safety
/// Interrupts are off.
unsafe fn listener(server: u64, key: u64) -> Option<(usize, bool)> {
    unsafe {
        let l = named(server, key)?;
        let it = local(l)?;
        (it.state == State::Listening).then_some((l, it.npending < it.backlog))
    }
}

/// Connect to whatever listens at `key` for the server `server`: a stream,
/// whose end 0 is the connector's — `creds` — and whose end 1 waits to be
/// accepted. The stream.
pub fn connect(server: u64, key: u64, creds: Creds, tid: usize) -> Result<usize, Refused> {
    let held = LOCK.lock();
    let found = unsafe { listener(server, key) };
    drop(held);
    let l = match found {
        None => return Err(Refused::Nobody),
        Some((_, false)) => return Err(Refused::Full),
        Some((l, true)) => l,
    };
    let stream = crate::stream::create(tid, false).ok_or(Refused::Full)?;
    let held = LOCK.lock();
    let queued = unsafe {
        match local(l) {
            Some(it) if it.state == State::Listening && it.server == server && it.key == key && it.npending < it.backlog => {
                match it.pending.ensure(it.npending, 16, QUEUE) {
                    Ok(at) => {
                        *at = stream;
                        it.npending += 1;
                        crate::stream::connected(stream, it.creds, creds, it.passcred);
                        // Whoever is waiting to accept looks again.
                        waitlist::wake_all(&mut it.waiters);
                        Ok(())
                    }
                    Err(_) => Err(Refused::Full),
                }
            }
            _ => Err(Refused::Nobody),
        }
    };
    drop(held);
    if let Err(why) = queued {
        // It went, or filled, in between; or there was no memory to queue it.
        crate::stream::close_end(stream, 0);
        crate::stream::close_end(stream, 1);
        return Err(why);
    }
    crate::pollset::note_local(l);
    Ok(stream)
}

/// Take the connection that has waited longest on listener `l`: the stream,
/// whose end 1 is the caller's to install. `None` if none is waiting, or
/// `l` does not listen.
pub fn take(l: usize) -> Option<usize> {
    let held = LOCK.lock();
    let out = unsafe {
        match local(l) {
            Some(it) if it.state == State::Listening && it.npending > 0 => {
                let stream = it.pending.get(0).copied();
                for i in 1..it.npending {
                    let next = it.pending.get(i).copied().unwrap_or(0);
                    if let Some(at) = it.pending.get_mut(i - 1) {
                        *at = next;
                    }
                }
                it.npending -= 1;
                stream
            }
            _ => None,
        }
    };
    drop(held);
    out
}

/// Park until a connection waits on listener `l`. False — and no wait —
/// if one already does, `l` does not listen, or a signal the caller is to
/// run is waiting.
pub fn wait(l: usize) -> bool {
    let tid = crate::scheduler::current_tid();
    let held = LOCK.lock();
    let parked = unsafe {
        match local(l) {
            Some(it) if it.state == State::Listening && it.npending == 0 && !crate::signal::ends_wait(tid) => {
                waitlist::add(&mut it.waiters, tid, On::Local(l as u32));
                crate::scheduler::block_task(tid);
                true
            }
            _ => false,
        }
    };
    drop(held);
    if parked {
        crate::scheduler::yield_now();
    }
    parked
}

/// `tid` off the list of the listener it waits on, if it waits on one:
/// under the local sockets' lock (`waitlist::forget`).
///
/// # Safety
/// Interrupts off, and [`LOCK`] not held.
pub unsafe fn forget(tid: usize) -> bool {
    let _held = LOCK.lock();
    unsafe {
        waitlist::forget_held(tid, |on| match on {
            On::Local(l) => Some(local(l as usize).map(|it| &mut it.waiters)),
            _ => None,
        })
    }
}

/// Whether `l` is a socket that is nothing yet: one a name can be given to,
/// or a connection made with.
pub fn unbound(l: usize) -> bool {
    let held = LOCK.lock();
    let is = unsafe { local(l).is_some_and(|it| it.state == State::Unbound) };
    drop(held);
    is
}

/// Whether a connection waits on `l` to be accepted.
pub fn readable(l: usize) -> bool {
    let held = LOCK.lock();
    let ready = unsafe { local(l).is_some_and(|it| it.state == State::Listening && it.npending > 0) };
    drop(held);
    ready
}

/// Whether `l` has asked to be told who sent what it receives; and, with
/// `set`, that it has or has not.
pub fn passcred(l: usize, set: Option<bool>) -> Option<bool> {
    let held = LOCK.lock();
    let out = unsafe {
        local(l).map(|it| {
            let was = it.passcred;
            if let Some(on) = set {
                it.passcred = on;
            }
            was
        })
    };
    drop(held);
    out
}
