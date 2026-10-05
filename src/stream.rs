//! Connected pairs of byte streams.
//!
//! A pipe carries bytes one way. Anything that speaks a protocol — a display
//! server and its client, most obviously — needs both directions, and needs
//! them paired, so that one end closing is something the other can observe.
//!
//! The bytes are two pipes rather than a new buffer, because a pipe is already
//! a ring with readers, writers and tasks blocked on both. What a stream adds
//! is the pairing, and a queue of descriptors in flight: a message can carry
//! handles to objects, which is what `SCM_RIGHTS` does on Unix and the reason
//! `wl_shm` works at all.
//!
//! The queue is not tied to the bytes. A send queues its descriptors before
//! its bytes, so a receive that reads a message's first byte can take them,
//! and a receive takes what is queued, in order, as many as it has room for
//! — perhaps a later message's too. That is all that what passes descriptors
//! asks: libwayland, libdbus and xcb each gather what arrives into a queue of
//! their own and give each message its share in order.
//!
//! And each end knows who is at the other (`SO_PEERCRED`): for a pair, the
//! program that made it; for a connection made by a name (`local.rs`), the
//! listener as it listened and the connector as it connected.

use crate::pipe;
use crate::task::FdKind;

/// Streams in the system. Two pipes apiece, which `pipe::MAX_PIPES` accounts
/// for: a compositor's clients, and the pairs a program starting others
/// makes to hear how each start went.
const MAX_STREAMS: usize = 64;

/// Descriptors in flight in one direction: a burst of messages, each with a
/// few. A send that would overfill it is refused whole.
const FD_QUEUE: usize = 32;

/// Who a program is, as one end of a stream is told of the other: a process
/// id, a user and a group.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Creds {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Creds {
    /// Task `tid`'s program, as it is now.
    pub fn of(tid: usize) -> Creds {
        let (uid, gid) = crate::scheduler::task_uid_gid(tid).unwrap_or((0, 0));
        Creds { pid: crate::scheduler::pid_of(tid) as u32, uid, gid }
    }
}

struct Stream {
    in_use: bool,
    creator: usize,
    /// Written by end 0, read by end 1.
    zero_to_one: usize,
    /// Written by end 1, read by end 0.
    one_to_zero: usize,
    /// Descriptors travelling towards end 0, then towards end 1.
    q: [[FdKind; FD_QUEUE]; 2],
    q_len: [usize; 2],
    /// How many descriptors name each end.
    ///
    /// A count and not a flag, because `SYS_FD_DUP` makes a second descriptor
    /// for one end — which is exactly how a parent hands a child its side of a
    /// connection. With a flag, the parent closing its copy would tell the peer
    /// the end had gone while the child was still holding it.
    refs: [usize; 2],
    /// Who is at the other end of each end.
    peer: [Creds; 2],
    /// Each end has asked to be told who sent what it receives
    /// (`SO_PASSCRED`).
    passcred: [bool; 2],
}

impl Stream {
    const fn empty() -> Self {
        Stream {
            in_use: false,
            creator: 0,
            zero_to_one: 0,
            one_to_zero: 0,
            q: [[FdKind::Empty; FD_QUEUE]; 2],
            q_len: [0; 2],
            refs: [0; 2],
            peer: [Creds { pid: 0, uid: 0, gid: 0 }; 2],
            passcred: [false; 2],
        }
    }
}

static mut STREAMS: [Stream; MAX_STREAMS] = {
    const S: Stream = Stream::empty();
    [S; MAX_STREAMS]
};

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
unsafe fn streams() -> &'static mut [Stream; MAX_STREAMS] { unsafe {
    &mut *core::ptr::addr_of_mut!(STREAMS)
}}

/// The pipe an end reads from, and the one it writes to.
pub fn pipes_for(stream: usize, end: u8) -> Option<(usize, usize)> {
    if stream >= MAX_STREAMS || end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &streams()[stream];
        if !s.in_use {
            None
        } else if end == 0 {
            Some((s.one_to_zero, s.zero_to_one))
        } else {
            Some((s.zero_to_one, s.one_to_zero))
        }
    };
    irq_restore(flags);
    out
}

/// Make a connected pair — of byte streams, or with `packets`, of streams
/// of messages each kept whole (`SOCK_SEQPACKET`). Returns the stream index,
/// or `None`.
pub fn create(tid: usize, packets: bool) -> Option<usize> {
    // The two pipes first: if either is refused there is nothing to unwind but
    // the other, and no stream slot has been claimed.
    let a = pipe::create_for_stream(packets)?;
    let b = match pipe::create_for_stream(packets) {
        Some(b) => b,
        None => {
            pipe::drop_unreferenced(a);
            return None;
        }
    };
    // Each end holds a reader on one pipe and a writer on the other, so both
    // pipes have exactly one of each for as long as both ends live.
    let _ = pipe::add_ref(a, false);
    let _ = pipe::add_ref(a, true);
    let _ = pipe::add_ref(b, false);
    let _ = pipe::add_ref(b, true);

    let flags = irq_save();
    let idx = unsafe { streams().iter().position(|s| !s.in_use) };
    match idx {
        Some(i) => {
            unsafe {
                let s = &mut streams()[i];
                *s = Stream::empty();
                s.in_use = true;
                s.creator = tid;
                s.zero_to_one = a;
                s.one_to_zero = b;
                s.refs = [1, 1];
                s.peer = [Creds::of(tid); 2];
            }
            irq_restore(flags);
            Some(i)
        }
        None => {
            irq_restore(flags);
            pipe::drop_ref(a, false);
            pipe::drop_ref(a, true);
            pipe::drop_ref(b, false);
            pipe::drop_ref(b, true);
            None
        }
    }
}

/// Take a reference on an end, for a second descriptor naming it.
pub fn retain_end(stream: usize, end: u8) -> Result<(), ()> {
    if stream >= MAX_STREAMS || end > 1 {
        return Err(());
    }
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut streams()[stream];
        if s.in_use && s.refs[end as usize] > 0 {
            s.refs[end as usize] += 1;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    if ok { Ok(()) } else { Err(()) }
}

/// One descriptor naming this end has gone. The end goes with the last of them.
pub fn close_end(stream: usize, end: u8) {
    if stream >= MAX_STREAMS || end > 1 {
        return;
    }
    let flags = irq_save();
    let gone = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.refs[end as usize] == 0 {
            irq_restore(flags);
            return;
        }
        s.refs[end as usize] -= 1;
        if s.refs[end as usize] > 0 {
            // Somebody else still holds this end; nothing observable happens.
            irq_restore(flags);
            return;
        }
        let both = s.refs[0] == 0 && s.refs[1] == 0;
        let pipes = if end == 0 {
            (s.one_to_zero, s.zero_to_one)
        } else {
            (s.zero_to_one, s.one_to_zero)
        };
        // Descriptors waiting for *this* end will never be collected now, so
        // they are released. The ones this end sent are not touched: they are
        // in the stream rather than in the sender, and the peer can still read
        // them — a program that writes, passes a descriptor and exits has
        // delivered both, which is what a socket pair is for. Only when both
        // ends have gone is the whole queue undeliverable.
        //
        // Taken out under the lock and released after, since releasing a
        // descriptor can reach back into this table.
        let mut orphans = [FdKind::Empty; FD_QUEUE * 2];
        let mut n = 0;
        for side in 0..2 {
            if side == end as usize || both {
                for i in 0..s.q_len[side] {
                    orphans[n] = s.q[side][i];
                    n += 1;
                }
                s.q_len[side] = 0;
            }
        }
        if both {
            *s = Stream::empty();
        }
        (pipes, orphans, n)
    };
    irq_restore(flags);

    let (gone, orphans, n) = gone;
    for kind in &orphans[..n] {
        crate::pipe::release_fd(kind);
    }

    // Dropping the writer this end held is what gives the peer end-of-file,
    // and dropping the reader is what tells the peer nobody is listening. The
    // pipes free themselves when both ends have done this.
    let (rd, wr) = gone;
    pipe::drop_ref(rd, false);
    pipe::drop_ref(wr, true);
}

/// Queue descriptors for the peer of `end`, all of them or none. False if
/// there is not room for them all, or the peer has gone.
///
/// The caller has already taken an in-flight reference on each; on refusal
/// they are the caller's to give back.
pub fn push_fds(stream: usize, end: u8, kinds: &[FdKind]) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.refs[to] == 0 || s.q_len[to] + kinds.len() > FD_QUEUE {
            false
        } else {
            for &kind in kinds {
                s.q[to][s.q_len[to]] = kind;
                s.q_len[to] += 1;
            }
            true
        }
    };
    irq_restore(flags);
    ok
}

/// Take back the descriptors this end last put on its peer's queue, if they
/// are still there and are `kinds`: a send that sent nothing sends no
/// descriptor either. Their in-flight references are the caller's to
/// release.
pub fn take_back_fds(stream: usize, end: u8, kinds: &[FdKind]) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let taken = unsafe {
        let s = &mut streams()[stream];
        let n = s.q_len[to];
        if s.in_use && n >= kinds.len() && s.q[to][n - kinds.len()..n] == *kinds {
            s.q_len[to] -= kinds.len();
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    taken
}

/// Who is at the other end of `end`.
pub fn peer_of(stream: usize, end: u8) -> Option<Creds> {
    if stream >= MAX_STREAMS || end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &streams()[stream];
        s.in_use.then_some(s.peer[end as usize])
    };
    irq_restore(flags);
    out
}

/// A connection made by a name: end 0's peer is `of_zero`'s and end 1's is
/// `of_one`'s; and end 1 has asked to be told who sent what, if `passcred`.
pub fn connected(stream: usize, of_zero: Creds, of_one: Creds, passcred: bool) {
    if stream < MAX_STREAMS {
        let flags = irq_save();
        unsafe {
            let s = &mut streams()[stream];
            s.peer = [of_zero, of_one];
            s.passcred[1] = passcred;
        }
        irq_restore(flags);
    }
}

/// Whether `end` has asked to be told who sent what it receives; and, with
/// `set`, that it has or has not.
pub fn passcred(stream: usize, end: u8, set: Option<bool>) -> Option<bool> {
    if stream >= MAX_STREAMS || end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &mut streams()[stream];
        s.in_use.then(|| {
            let was = s.passcred[end as usize];
            if let Some(on) = set {
                s.passcred[end as usize] = on;
            }
            was
        })
    };
    irq_restore(flags);
    out
}

/// Take the descriptor at the head of this end's queue, if any.
///
/// Its in-flight reference comes with it and is the caller's to convert into
/// an owned one or to release.
pub fn pop_fd(stream: usize, end: u8) -> Option<FdKind> {
    if stream >= MAX_STREAMS || end > 1 {
        return None;
    }
    let me = end as usize;
    let flags = irq_save();
    let out = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.q_len[me] == 0 {
            None
        } else {
            let head = s.q[me][0];
            for i in 1..s.q_len[me] {
                s.q[me][i - 1] = s.q[me][i];
            }
            s.q_len[me] -= 1;
            Some(head)
        }
    };
    irq_restore(flags);
    out
}

/// Is there something for this end to read?
pub fn readable(stream: usize, end: u8) -> bool {
    match pipes_for(stream, end) {
        Some((rd, _)) => pipe::readable(rd),
        None => false,
    }
}

/// Is there room for this end to write?
pub fn writable(stream: usize, end: u8) -> bool {
    match pipes_for(stream, end) {
        Some((_, wr)) => pipe::writable(wr),
        None => false,
    }
}

/// Has the other end's last descriptor gone?
pub fn peer_gone(stream: usize, end: u8) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return true;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &streams()[stream];
        !s.in_use || s.refs[1 - end as usize] == 0
    };
    irq_restore(flags);
    out
}
