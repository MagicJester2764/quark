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

use crate::grow::Grow;
use crate::pipe;
use crate::task::FdKind;

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

/// Descriptors travelling towards one end, oldest first: room made as they
/// come. A send that would overfill it is refused whole, and "full" is as
/// many as the sender may hold — Linux's bound, its sender's
/// `RLIMIT_NOFILE`. There was room for thirty-two.
struct Queue {
    kinds: Grow<FdKind>,
    /// The oldest is at `head`, and there are `len` from there.
    head: usize,
    len: usize,
}

impl Queue {
    const fn new() -> Self {
        Queue { kinds: Grow::new(FdKind::Empty), head: 0, len: 0 }
    }

    /// Room for `more` behind what is there, made now: moved to the front,
    /// or grown. False if there is no memory for it.
    fn room_for(&mut self, more: usize) -> bool {
        if more == 0 {
            return true;
        }
        if self.head > 0 && self.head + self.len + more > self.kinds.len() {
            for i in 0..self.len {
                let kind = self.kinds.get(self.head + i).copied().unwrap_or(FdKind::Empty);
                if let Some(at) = self.kinds.get_mut(i) {
                    *at = kind;
                }
            }
            self.head = 0;
        }
        self.kinds.ensure(self.head + self.len + more - 1, 8, crate::task::FD_MOST).is_ok()
    }

    /// Put `kind` behind the rest. Room was made for it.
    fn push(&mut self, kind: FdKind) {
        if let Some(at) = self.kinds.get_mut(self.head + self.len) {
            *at = kind;
            self.len += 1;
        }
    }

    fn pop(&mut self) -> Option<FdKind> {
        if self.len == 0 {
            return None;
        }
        let kind = self.kinds.get(self.head).copied();
        self.head += 1;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        kind
    }

    /// The last `kinds.len()` are `kinds`: taken off, and true.
    fn take_back(&mut self, kinds: &[FdKind]) -> bool {
        let n = kinds.len();
        if n > self.len {
            return false;
        }
        let from = self.head + self.len - n;
        let same = kinds.iter().enumerate().all(|(i, k)| self.kinds.get(from + i) == Some(k));
        if same {
            self.len -= n;
        }
        same
    }

    /// Everything, taken out: this is left empty.
    fn take(&mut self) -> Queue {
        core::mem::replace(self, Queue::new())
    }

    fn iter(&self) -> impl Iterator<Item = &FdKind> + '_ {
        self.kinds.iter().skip(self.head).take(self.len)
    }
}

struct Stream {
    /// Written by end 0, read by end 1.
    zero_to_one: usize,
    /// Written by end 1, read by end 0.
    one_to_zero: usize,
    /// Descriptors travelling towards end 0, then towards end 1.
    q: [Queue; 2],
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

/// Every stream, from its making until both its ends have gone
/// (`table.rs`): a compositor's clients, and the pairs a program starting
/// others makes to hear how each start went. There were sixty-four.
static mut STREAMS: crate::table::Table<Stream> = crate::table::Table::new(crate::table::MOST);

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

/// Stream `stream`, if there is one.
///
/// # Safety
/// Interrupts off.
#[inline(always)]
unsafe fn stream_at(stream: usize) -> Option<&'static mut Stream> {
    unsafe { (*core::ptr::addr_of_mut!(STREAMS)).get(stream) }
}

/// The pipe an end reads from, and the one it writes to.
pub fn pipes_for(stream: usize, end: u8) -> Option<(usize, usize)> {
    if end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        stream_at(stream).map(|s| if end == 0 { (s.one_to_zero, s.zero_to_one) } else { (s.zero_to_one, s.one_to_zero) })
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

    let me = Creds::of(tid);
    let flags = irq_save();
    let made = unsafe {
        let streams = &mut *core::ptr::addr_of_mut!(STREAMS);
        streams.lowest_free(0).filter(|&i| {
            let s = Stream {
                zero_to_one: a,
                one_to_zero: b,
                q: [Queue::new(), Queue::new()],
                refs: [1, 1],
                peer: [me; 2],
                passcred: [false; 2],
            };
            streams.fill_at(i, s).is_ok()
        })
    };
    irq_restore(flags);
    if made.is_none() {
        pipe::drop_ref(a, false);
        pipe::drop_ref(a, true);
        pipe::drop_ref(b, false);
        pipe::drop_ref(b, true);
    }
    made
}

/// Take a reference on an end, for a second descriptor naming it.
pub fn retain_end(stream: usize, end: u8) -> Result<(), ()> {
    if end > 1 {
        return Err(());
    }
    let flags = irq_save();
    let ok = unsafe {
        match stream_at(stream) {
            Some(s) if s.refs[end as usize] > 0 => {
                s.refs[end as usize] += 1;
                true
            }
            _ => false,
        }
    };
    irq_restore(flags);
    if ok { Ok(()) } else { Err(()) }
}

/// One descriptor naming this end has gone. The end goes with the last of them.
pub fn close_end(stream: usize, end: u8) {
    if end > 1 {
        return;
    }
    let flags = irq_save();
    let gone = unsafe {
        let Some(s) = stream_at(stream).filter(|s| s.refs[end as usize] > 0) else {
            irq_restore(flags);
            return;
        };
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
        let mut orphans = [Queue::new(), Queue::new()];
        for side in 0..2 {
            if side == end as usize || both {
                orphans[side] = s.q[side].take();
            }
        }
        if both {
            (*core::ptr::addr_of_mut!(STREAMS)).empty(stream);
        }
        (pipes, orphans)
    };
    irq_restore(flags);

    let (gone, orphans) = gone;
    for q in &orphans {
        for kind in q.iter() {
            crate::pipe::release_fd(kind);
        }
    }

    // Dropping the writer this end held is what gives the peer end-of-file,
    // and dropping the reader is what tells the peer nobody is listening. The
    // pipes free themselves when both ends have done this.
    let (rd, wr) = gone;
    pipe::drop_ref(rd, false);
    pipe::drop_ref(wr, true);
}

/// Queue descriptors for the peer of `end`, all of them or none: no more
/// than `most` waiting there with them. False if there is not room for them
/// all, or the peer has gone.
///
/// The caller has already taken an in-flight reference on each; on refusal
/// they are the caller's to give back.
pub fn push_fds(stream: usize, end: u8, kinds: &[FdKind], most: usize) -> bool {
    if end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let ok = unsafe {
        match stream_at(stream) {
            Some(s) if s.refs[to] > 0 && s.q[to].len + kinds.len() <= most => {
                let q = &mut s.q[to];
                let room = q.room_for(kinds.len());
                if room {
                    for &kind in kinds {
                        q.push(kind);
                    }
                }
                room
            }
            _ => false,
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
    if end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let taken = unsafe { stream_at(stream).is_some_and(|s| s.q[to].take_back(kinds)) };
    irq_restore(flags);
    taken
}

/// Who is at the other end of `end`.
pub fn peer_of(stream: usize, end: u8) -> Option<Creds> {
    if end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe { stream_at(stream).map(|s| s.peer[end as usize]) };
    irq_restore(flags);
    out
}

/// A connection made by a name: end 0's peer is `of_zero`'s and end 1's is
/// `of_one`'s; and end 1 has asked to be told who sent what, if `passcred`.
pub fn connected(stream: usize, of_zero: Creds, of_one: Creds, passcred: bool) {
    let flags = irq_save();
    unsafe {
        if let Some(s) = stream_at(stream) {
            s.peer = [of_zero, of_one];
            s.passcred[1] = passcred;
        }
    }
    irq_restore(flags);
}

/// Whether `end` has asked to be told who sent what it receives; and, with
/// `set`, that it has or has not.
pub fn passcred(stream: usize, end: u8, set: Option<bool>) -> Option<bool> {
    if end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        stream_at(stream).map(|s| {
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
    if end > 1 {
        return None;
    }
    let flags = irq_save();
    let out = unsafe { stream_at(stream).and_then(|s| s.q[end as usize].pop()) };
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
    if end > 1 {
        return true;
    }
    let flags = irq_save();
    let out = unsafe { stream_at(stream).is_none_or(|s| s.refs[1 - end as usize] == 0) };
    irq_restore(flags);
    out
}
