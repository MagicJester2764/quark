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

use crate::stream::Creds;

pub const MAX_LOCALS: usize = 32;
/// Connections waiting to be accepted: the most a listener has, whatever
/// backlog it asks for.
const QUEUE: usize = 16;
const MAX_WAITERS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    Unbound,
    Bound,
    Listening,
}

#[derive(Clone, Copy)]
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
    /// Streams made and not yet accepted, whose end 1 is the accepter's.
    pending: [usize; QUEUE],
    npending: usize,
    waiters: [usize; MAX_WAITERS],
    nwaiters: usize,
}

const FREE: Local = Local {
    state: State::Free,
    refs: 0,
    server: 0,
    key: 0,
    creds: Creds { pid: 0, uid: 0, gid: 0 },
    passcred: false,
    backlog: 0,
    pending: [0; QUEUE],
    npending: 0,
    waiters: [0; MAX_WAITERS],
    nwaiters: 0,
};

static mut LOCALS: [Local; MAX_LOCALS] = [FREE; MAX_LOCALS];

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
unsafe fn locals() -> &'static mut [Local; MAX_LOCALS] {
    unsafe { &mut *core::ptr::addr_of_mut!(LOCALS) }
}

/// A new socket that is nothing yet, with one reference: the descriptor
/// about to name it.
pub fn create() -> Option<usize> {
    let flags = irq_save();
    let made = unsafe {
        locals().iter().position(|l| l.state == State::Free).inspect(|&l| {
            locals()[l] = Local { state: State::Unbound, refs: 1, ..FREE };
        })
    };
    irq_restore(flags);
    made
}

/// Another descriptor names `l`.
pub fn retain(l: usize) {
    if l < MAX_LOCALS {
        let flags = irq_save();
        unsafe {
            if locals()[l].state != State::Free {
                locals()[l].refs += 1;
            }
        }
        irq_restore(flags);
    }
}

/// A descriptor for `l` is gone. The last takes it with it, and with it
/// every connection still waiting to be accepted: each connector finds the
/// other end gone.
pub fn release(l: usize) {
    if l >= MAX_LOCALS {
        return;
    }
    let mut pending = [0usize; QUEUE];
    let flags = irq_save();
    let n = unsafe {
        let it = &mut locals()[l];
        if it.state == State::Free {
            0
        } else {
            it.refs = it.refs.saturating_sub(1);
            if it.refs > 0 {
                0
            } else {
                let n = it.npending;
                pending[..n].copy_from_slice(&it.pending[..n]);
                *it = FREE;
                n
            }
        }
    };
    irq_restore(flags);
    for &stream in &pending[..n] {
        crate::stream::close_end(stream, 1);
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
    if l >= MAX_LOCALS {
        return Err(Refused::NotOne);
    }
    let flags = irq_save();
    let out = unsafe {
        if locals()[l].state != State::Unbound {
            Err(Refused::NotOne)
        } else if locals()
            .iter()
            .any(|o| matches!(o.state, State::Bound | State::Listening) && o.server == server && o.key == key)
        {
            Err(Refused::Taken)
        } else {
            let it = &mut locals()[l];
            it.state = State::Bound;
            it.server = server;
            it.key = key;
            Ok(())
        }
    };
    irq_restore(flags);
    out
}

/// `l`, which has a name, listens, with room for `backlog` connections
/// waiting (and never more than [`QUEUE`]); `creds` is who it is, which
/// every connector is told. Listening again changes the room.
pub fn listen(l: usize, backlog: usize, creds: Creds) -> bool {
    if l >= MAX_LOCALS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let it = &mut locals()[l];
        if matches!(it.state, State::Bound | State::Listening) {
            it.state = State::Listening;
            it.backlog = backlog.clamp(1, QUEUE);
            it.creds = creds;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    ok
}

/// Connect to whatever listens at `key` for the server `server`: a stream,
/// whose end 0 is the connector's — `creds` — and whose end 1 waits to be
/// accepted. The stream.
pub fn connect(server: u64, key: u64, creds: Creds, tid: usize) -> Result<usize, Refused> {
    let flags = irq_save();
    let found = unsafe {
        locals()
            .iter()
            .position(|o| o.state == State::Listening && o.server == server && o.key == key)
            .map(|l| (l, locals()[l].npending >= locals()[l].backlog))
    };
    irq_restore(flags);
    let l = match found {
        None => return Err(Refused::Nobody),
        Some((_, true)) => return Err(Refused::Full),
        Some((l, false)) => l,
    };
    let stream = crate::stream::create(tid, false).ok_or(Refused::Full)?;
    let flags = irq_save();
    let queued = unsafe {
        let it = &mut locals()[l];
        if it.state == State::Listening && it.server == server && it.key == key && it.npending < it.backlog {
            crate::stream::connected(stream, it.creds, creds, it.passcred);
            it.pending[it.npending] = stream;
            it.npending += 1;
            // Whoever is waiting to accept looks again.
            for &t in &it.waiters[..it.nwaiters] {
                crate::scheduler::unblock_task(t);
            }
            it.nwaiters = 0;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    if !queued {
        // It went, or filled, in between.
        crate::stream::close_end(stream, 0);
        crate::stream::close_end(stream, 1);
        return Err(Refused::Nobody);
    }
    crate::pollset::note_local(l);
    Ok(stream)
}

/// Take the connection that has waited longest on listener `l`: the stream,
/// whose end 1 is the caller's to install. `None` if none is waiting, or
/// `l` does not listen.
pub fn take(l: usize) -> Option<usize> {
    if l >= MAX_LOCALS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let it = &mut locals()[l];
        (it.state == State::Listening && it.npending > 0).then(|| {
            let stream = it.pending[0];
            it.pending.copy_within(1..it.npending, 0);
            it.npending -= 1;
            stream
        })
    };
    irq_restore(flags);
    out
}

/// Park until a connection waits on listener `l`. False — and no wait —
/// if one already does, `l` does not listen, a signal the caller is to run
/// is waiting, or there is no room to be recorded as a waiter.
pub fn wait(l: usize) -> bool {
    if l >= MAX_LOCALS {
        return false;
    }
    let tid = crate::scheduler::current_tid();
    let flags = irq_save();
    let parked = unsafe {
        let it = &mut locals()[l];
        if it.state != State::Listening
            || it.npending > 0
            || it.nwaiters >= MAX_WAITERS
            || crate::signal::ends_wait(tid)
        {
            false
        } else {
            it.waiters[it.nwaiters] = tid;
            it.nwaiters += 1;
            crate::scheduler::block_task(tid);
            true
        }
    };
    irq_restore(flags);
    if parked {
        crate::scheduler::yield_now();
    }
    parked
}

/// A task parked on `l` has gone, or is to look again for a signal: it is
/// waiting for nothing here now. True if it was.
pub fn forget_waiter(l: usize, tid: usize) -> bool {
    if l >= MAX_LOCALS {
        return false;
    }
    let flags = irq_save();
    let found = unsafe {
        let it = &mut locals()[l];
        crate::pipe::forget_in(&mut it.waiters, &mut it.nwaiters, tid)
    };
    irq_restore(flags);
    found
}

/// Whether `l` is a socket that is nothing yet: one a name can be given to,
/// or a connection made with.
pub fn unbound(l: usize) -> bool {
    if l >= MAX_LOCALS {
        return false;
    }
    let flags = irq_save();
    let is = unsafe { locals()[l].state == State::Unbound };
    irq_restore(flags);
    is
}

/// Whether a connection waits on `l` to be accepted.
pub fn readable(l: usize) -> bool {
    if l >= MAX_LOCALS {
        return false;
    }
    let flags = irq_save();
    let ready = unsafe { locals()[l].state == State::Listening && locals()[l].npending > 0 };
    irq_restore(flags);
    ready
}

/// Whether `l` has asked to be told who sent what it receives; and, with
/// `set`, that it has or has not.
pub fn passcred(l: usize, set: Option<bool>) -> Option<bool> {
    if l >= MAX_LOCALS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let it = &mut locals()[l];
        (it.state != State::Free).then(|| {
            let was = it.passcred;
            if let Some(on) = set {
                it.passcred = on;
            }
            was
        })
    };
    irq_restore(flags);
    out
}
