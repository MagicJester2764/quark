//! Connected pairs of byte streams.
//!
//! A pipe carries bytes one way. Anything that speaks a protocol — a display
//! server and its client, most obviously — needs both directions, and needs
//! them paired, so that one end closing is something the other can observe.
//!
//! The bytes are two pipes rather than a new buffer, because a pipe is already
//! a ring with readers, writers and tasks blocked on both. What a stream adds
//! is the pairing, and a queue of descriptors in flight: a message can carry a
//! handle to an object, which is what `SCM_RIGHTS` does on Unix and the reason
//! `wl_shm` works at all. The queue is filled in the commit that adds passing;
//! here it exists and stays empty.

use crate::pipe;
use crate::task::FdKind;

/// Streams in the system. Two pipes apiece, which `pipe::MAX_PIPES` accounts
/// for.
const MAX_STREAMS: usize = 32;

/// Descriptors in flight in one direction.
///
/// A protocol attaches at most one descriptor to a message and the peer reads
/// messages in order, so this only has to absorb a burst.
const FD_QUEUE: usize = 8;

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

/// Make a connected pair. Returns the stream index, or `None`.
pub fn create(tid: usize) -> Option<usize> {
    // The two pipes first: if either is refused there is nothing to unwind but
    // the other, and no stream slot has been claimed.
    let a = pipe::create_for_stream()?;
    let b = match pipe::create_for_stream() {
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
        // Anything still travelling towards the peer will never arrive: this
        // end is the one that would have delivered it. Take the queue out
        // under the lock and release it after, since releasing a descriptor
        // can reach back into this table.
        let mut orphans = [FdKind::Empty; FD_QUEUE * 2];
        let mut n = 0;
        for side in 0..2 {
            if side == 1 - end as usize || both {
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
        crate::pipe::release_in_flight(kind);
    }

    // Dropping the writer this end held is what gives the peer end-of-file,
    // and dropping the reader is what tells the peer nobody is listening. The
    // pipes free themselves when both ends have done this.
    let (rd, wr) = gone;
    pipe::drop_ref(rd, false);
    pipe::drop_ref(wr, true);
}

/// Queue a descriptor for the peer of `end`. False if the queue is full or
/// the peer has gone.
///
/// The caller has already taken an in-flight reference; on refusal it is the
/// caller's to give back.
pub fn push_fd(stream: usize, end: u8, kind: FdKind) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.refs[to] == 0 || s.q_len[to] == FD_QUEUE {
            false
        } else {
            s.q[to][s.q_len[to]] = kind;
            s.q_len[to] += 1;
            true
        }
    };
    irq_restore(flags);
    ok
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
