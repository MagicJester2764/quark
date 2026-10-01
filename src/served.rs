//! Descriptors that name an object in a server.
//!
//! A pipe is the kernel's, and so is a terminal. A file is not: it is a
//! handle in the file server's table, and until this existed the kernel knew
//! nothing of it. Every C library kept its own list of what it had open,
//! in its own memory, under numbers it made up — so what a program had open
//! did not survive `exec`, a forked child held numbers the server would not
//! answer for, a file could not be put where a program's standard output was,
//! and a program written against another runtime could not write to one at
//! all.
//!
//! A served descriptor is the same thing as any other descriptor, in the same
//! table, with one difference: what it names is a number the *server* chose —
//! its cookie — and the kernel's part is only to count who holds it.
//!
//! - A server makes one for a client that is calling it (`SYS_FD_SERVE`), so
//!   nobody can be handed one unasked.
//! - It is copied by `dup` and `fork`, kept by `exec`, sent down a stream and
//!   closed like any other. The kernel counts those.
//! - A server asked to act on a cookie asks the kernel whether the task asking
//!   holds it (`SYS_FD_HOLDS`). That is the whole of the authority: there is
//!   no way to hold a cookie except to have been given the descriptor.
//! - When the last descriptor for a cookie closes, the server is told
//!   (`TAG_FD_RELEASED`) and collects it (`SYS_FD_REAP`). The telling is a
//!   flag and the collecting is a call, so none is ever lost: a server that
//!   was busy finds every cookie still waiting when it comes to look.
//! - Reading and writing one through `SYS_FD_READ` and `SYS_FD_WRITE` is a
//!   call the kernel makes to the server on the task's behalf, lending the
//!   task's buffer. So anything that can write to descriptor 1 can write to a
//!   file put there, whatever it was written in.
//!
//! A server is known by its endpoint, which is never given to another task, so
//! a cookie outlives its server as nothing: every operation on it fails, and
//! it is forgotten when the last descriptor goes.

use crate::scheduler;
use crate::task::MAX_TASKS;

/// Objects at once, for every server together. The file server has 512
/// handles; this leaves room for a second server before anybody has to count.
pub const MAX_SERVED: usize = 1024;

/// The most one kernel-made read or write lends: what `SYS_LENT_READ` and
/// `SYS_LENT_WRITE` copy in one go. A longer one is a short read or write.
const IO_MAX: usize = crate::lend::COPY_MAX;

/// "Something of yours has no descriptors left": sender 0, no data. Collect
/// with `SYS_FD_REAP` until it says there is nothing.
pub const TAG_FD_RELEASED: u64 = 0xFFFF_0008;
/// The kernel reads for a task: `data` = `[cookie, length]`, a buffer lent for
/// writing. Reply tag 0 with the count in `data[0]`.
pub const TAG_FD_READ: u64 = 0xFFFF_0009;
/// The kernel writes for a task: `data` = `[cookie, length]`, a buffer lent
/// for reading. Reply tag 0 with the count in `data[0]`.
pub const TAG_FD_WRITE: u64 = 0xFFFF_000A;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Free,
    /// A descriptor names it.
    Live,
    /// None does, and its server has not collected it yet.
    Released,
}

#[derive(Clone, Copy)]
struct Object {
    state: State,
    server: usize,
    /// The server's endpoint when this was made. A task that takes the
    /// server's TID later has another, so it is never mistaken for it.
    endpoint: u64,
    cookie: u64,
    refs: u32,
}

const NONE: Object = Object { state: State::Free, server: 0, endpoint: 0, cookie: 0, refs: 0 };

static mut OBJECTS: [Object; MAX_SERVED] = [NONE; MAX_SERVED];
/// Per server: something is waiting to be collected, and it has not been told.
static mut NOTICE: [bool; MAX_TASKS] = [false; MAX_TASKS];

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
unsafe fn objects() -> &'static mut [Object; MAX_SERVED] {
    unsafe { &mut *core::ptr::addr_of_mut!(OBJECTS) }
}

/// Is the server that made this object still the task at its TID?
fn server_alive(o: &Object) -> bool {
    o.endpoint != 0
        && crate::cap::endpoint_of(o.server) == o.endpoint
        && scheduler::task_is_live(o.server)
}

/// Make an object for `server`'s `cookie`, with one reference: the descriptor
/// the caller is about to install.
pub fn create(server: usize, cookie: u64) -> Option<usize> {
    let endpoint = crate::cap::endpoint_of(server);
    if endpoint == 0 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match objects().iter().position(|o| o.state == State::Free) {
            Some(i) => {
                objects()[i] = Object { state: State::Live, server, endpoint, cookie, refs: 1 };
                Some(i)
            }
            None => None,
        }
    };
    irq_restore(flags);
    out
}

/// Forget an object whose descriptor never got installed. The server is not
/// told: it has not been told the object exists.
pub fn discard(obj: usize) {
    if obj >= MAX_SERVED {
        return;
    }
    let flags = irq_save();
    unsafe { objects()[obj] = NONE };
    irq_restore(flags);
}

pub fn retain(obj: usize) -> bool {
    if obj >= MAX_SERVED {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let o = &mut objects()[obj];
        if o.state == State::Live {
            o.refs += 1;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    ok
}

/// One descriptor fewer. The last one hands the object back to its server:
/// it waits, counted by nobody, until the server collects it.
pub fn release(obj: usize) {
    if obj >= MAX_SERVED {
        return;
    }
    let mut wake = None;
    let flags = irq_save();
    unsafe {
        let o = &mut objects()[obj];
        if o.state == State::Live && o.refs > 0 {
            o.refs -= 1;
            if o.refs == 0 {
                if server_alive(o) {
                    o.state = State::Released;
                    let server = o.server;
                    (*core::ptr::addr_of_mut!(NOTICE))[server] = true;
                    wake = Some(server);
                } else {
                    // Nobody to collect it.
                    *o = NONE;
                }
            }
        }
    }
    irq_restore(flags);
    if let Some(server) = wake {
        crate::ipc::wake_for_notice(server);
    }
}

/// The notice for `server`, if it is owed one. Taking it clears it; a release
/// that comes afterwards owes another.
///
/// # Safety
/// Interrupts are off.
pub unsafe fn take_notice(server: usize) -> bool {
    unsafe {
        if server >= MAX_TASKS {
            return false;
        }
        let n = &mut (*core::ptr::addr_of_mut!(NOTICE))[server];
        core::mem::replace(n, false)
    }
}

/// One of `server`'s objects that no descriptor names, forgotten as it is
/// returned. `None` when there is none.
pub fn reap(server: usize) -> Option<u64> {
    let endpoint = crate::cap::endpoint_of(server);
    if endpoint == 0 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match objects()
            .iter()
            .position(|o| o.state == State::Released && o.endpoint == endpoint)
        {
            Some(i) => {
                let cookie = objects()[i].cookie;
                objects()[i] = NONE;
                Some(cookie)
            }
            None => None,
        }
    };
    irq_restore(flags);
    out
}

/// The server and the cookie an object names, while its server lives.
pub fn of(obj: usize) -> Option<(usize, u64)> {
    if obj >= MAX_SERVED {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let o = &objects()[obj];
        if o.state == State::Live && server_alive(o) { Some((o.server, o.cookie)) } else { None }
    };
    irq_restore(flags);
    out
}

/// The cookie an object names, if it is one of `server`'s.
pub fn cookie_for(obj: usize, server: usize) -> Option<u64> {
    let endpoint = crate::cap::endpoint_of(server);
    if obj >= MAX_SERVED || endpoint == 0 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let o = &objects()[obj];
        if o.state == State::Live && o.endpoint == endpoint { Some(o.cookie) } else { None }
    };
    irq_restore(flags);
    out
}

/// A server has gone. What it had not collected has nobody to collect it.
/// Objects descriptors still name stay, as nothing, until those close.
pub fn server_gone(server: usize) {
    let endpoint = crate::cap::endpoint_of(server);
    let flags = irq_save();
    unsafe {
        if server < MAX_TASKS {
            (*core::ptr::addr_of_mut!(NOTICE))[server] = false;
        }
        if endpoint != 0 {
            for o in objects().iter_mut() {
                if o.state == State::Released && o.endpoint == endpoint {
                    *o = NONE;
                }
            }
        }
    }
    irq_restore(flags);
}

/// Read or write through a served descriptor: a call to its server on the
/// running task's behalf, lending it `len` bytes at `ptr`.
///
/// The task's own capabilities have nothing to do with it. Holding the
/// descriptor is the permission, as it is for a pipe.
pub fn io(obj: usize, write: bool, ptr: usize, len: usize) -> u64 {
    let Some((server, cookie)) = of(obj) else {
        return u64::MAX;
    };
    if len == 0 {
        return 0;
    }
    let len = len.min(IO_MAX);
    let msg = crate::ipc::Message {
        sender: 0,
        tag: if write { TAG_FD_WRITE } else { TAG_FD_READ },
        data: [cookie, len as u64, 0, 0, 0, 0],
    };
    let lent = crate::ipc::Lent {
        addr: ptr,
        len,
        // The server reads what is being written and writes what is being read.
        access: if write { crate::lend::LEND_READ } else { crate::lend::LEND_WRITE },
        frame: false,
    };
    match crate::ipc::served_call(server, &msg, lent) {
        Ok(reply) if reply.tag == 0 => reply.data[0].min(len as u64),
        _ => u64::MAX,
    }
}
