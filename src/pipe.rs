/// Kernel pipes for the Quark microkernel.
///
/// Anonymous byte-stream channels with a fixed-size ring buffer.
/// Blocking semantics: reader blocks if empty, writer blocks if full.
///
/// All pipe operations disable interrupts to prevent data races:
/// the timer interrupt can preempt syscall handlers and context-switch
/// to another task that accesses the same pipe concurrently.

use crate::scheduler;
use crate::task::FdKind;
use crate::waitlist::{self, On, Waiters};

/// A pipe's buffer: a frame of its own.
const PIPE_BUF_SIZE: usize = 4096;

struct Pipe {
    /// Task that created this pipe, so an orphan (created but never wired to
    /// any fd, hence refcount 0) can be reclaimed when its creator dies.
    /// Without this, sys_pipe_create leaked a slot permanently on every
    /// failed spawn and the 31 slots could be exhausted for good.
    creator: usize,
    /// The ring the bytes go round: a frame, given back with the pipe.
    buf: *mut u8,
    read_pos: usize,
    write_pos: usize,
    len: usize,
    readers: usize,
    writers: usize,
    /// Who is waiting to read, and to write (`waitlist.rs`).
    read_waiters: Waiters,
    write_waiters: Waiters,
    /// A pipe a server's key names (see [`open_named`]): a FIFO.
    named: bool,
    /// How many times each end of a named pipe has been opened: the reading
    /// end, then the writing. They only go up. Somebody waiting for the
    /// other end to be opened is waiting for one of these to move, not for
    /// the end to be held — a writer that opened, wrote and closed before
    /// the reader ran again has still been, and the reader's wait is over.
    opens: [u32; 2],
    /// Tasks waiting for the other end to be opened by somebody.
    peer_waiters: Waiters,
    /// Each write is a record kept whole — two bytes of length and its
    /// bytes — and each read takes one record, what does not fit the reader
    /// dropped: a stream made to keep messages whole (`SOCK_SEQPACKET`).
    /// What is in the buffer is always whole records, so what it holds and
    /// whether it is empty mean what they mean for bytes.
    packets: bool,
}

impl Pipe {
    const fn new(creator: usize, buf: *mut u8) -> Self {
        Pipe {
            creator,
            buf,
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            read_waiters: Waiters::NONE,
            write_waiters: Waiters::NONE,
            named: false,
            opens: [0; 2],
            peer_waiters: Waiters::NONE,
            packets: false,
        }
    }

    /// Byte `i` of the ring.
    ///
    /// # Safety
    /// `i` is below `PIPE_BUF_SIZE`.
    unsafe fn at(&self, i: usize) -> *mut u8 {
        unsafe { self.buf.add(i) }
    }

    /// Take what is next for a reader with room for `max_len`: bytes up to
    /// that, or one record, of which what does not fit is dropped. How much
    /// the reader was given.
    ///
    /// # Safety
    /// [`LOCK`] held, something to take, and `buf` a user range the call
    /// has checked.
    unsafe fn take(&mut self, buf: *mut u8, max_len: usize) -> usize {
        unsafe {
            let (from, n, used) = if self.packets {
                let n = *self.at(self.read_pos) as usize | (*self.at((self.read_pos + 1) % PIPE_BUF_SIZE) as usize) << 8;
                ((self.read_pos + 2) % PIPE_BUF_SIZE, n.min(max_len), 2 + n)
            } else {
                let n = self.len.min(max_len);
                (self.read_pos, n, n)
            };
            {
                let _ua = crate::cpu::UserAccess::begin();
                for i in 0..n {
                    buf.add(i).write(*self.at((from + i) % PIPE_BUF_SIZE));
                }
            }
            self.read_pos = (self.read_pos + used) % PIPE_BUF_SIZE;
            self.len -= used;
            n
        }
    }

    /// Room for a write of `len`: any for bytes, and for a record the whole
    /// of it with its length.
    fn room_for(&self, len: usize) -> usize {
        let space = PIPE_BUF_SIZE - self.len;
        if !self.packets {
            space
        } else if space >= len + 2 {
            len
        } else {
            0
        }
    }

    /// Put `n` bytes of `buf` in — as one record, with its length, for a
    /// stream of records.
    ///
    /// # Safety
    /// [`LOCK`] held, room for them ([`Pipe::room_for`]), and `buf` a user
    /// range the call has checked.
    unsafe fn put(&mut self, buf: *const u8, n: usize) {
        unsafe {
            if self.packets {
                *self.at(self.write_pos) = n as u8;
                *self.at((self.write_pos + 1) % PIPE_BUF_SIZE) = (n >> 8) as u8;
                self.write_pos = (self.write_pos + 2) % PIPE_BUF_SIZE;
                self.len += 2;
            }
            {
                let _ua = crate::cpu::UserAccess::begin();
                for i in 0..n {
                    *self.at((self.write_pos + i) % PIPE_BUF_SIZE) = buf.add(i).read();
                }
            }
            self.write_pos = (self.write_pos + n) % PIPE_BUF_SIZE;
            self.len += n;
        }
    }
}

/// Named pipes: FIFOs.
///
/// A pipe two programs find by a name rather than by being handed its ends.
/// The name is a server's business — a file server has an inode for it, with
/// an owner and a mode — and the pipe is the kernel's, like any other, since
/// a program waits on one with `poll`. What joins them is here: a server
/// names a pipe by a key of its own choosing, and for as long as anybody
/// holds an end of the pipe that key names, the key names that pipe.
///
/// The key is the server's, so the table is keyed by the server too — by its
/// endpoint number, which is never given out again. A server that dies leaves
/// keys nobody can ask for, and they go with the last end like any other.
#[derive(Clone, Copy)]
struct Named {
    /// The server's endpoint number.
    server: u64,
    key: u64,
    pipe: usize,
}

/// Every name, made when a named pipe is first opened and given back with
/// the pipe (`table.rs`). There were thirty-two.
static mut NAMED: crate::table::Table<Named> = crate::table::Table::new(crate::table::MOST);

/// The pipes' lock (`sync::RANK_PIPE`): both tables — the pipes and their
/// names — every pipe and name in them, and the lists of those waiting on
/// them.
static LOCK: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(crate::sync::RANK_PIPE, "the pipes", ());

/// # Safety
/// [`LOCK`] held.
#[inline(always)]
unsafe fn named() -> &'static mut crate::table::Table<Named> {
    unsafe { &mut *core::ptr::addr_of_mut!(NAMED) }
}

/// The pipe `key` names for `server`, if it names one.
///
/// # Safety
/// [`LOCK`] held.
unsafe fn named_pipe(server: u64, key: u64) -> Option<usize> {
    unsafe {
        let mut at = 0;
        while let Some(i) = named().next_used(at) {
            at = i + 1;
            if let Some(n) = named().get(i).filter(|n| n.server == server && n.key == key) {
                return Some(n.pipe);
            }
        }
        None
    }
}

/// What opening a named pipe came to.
pub enum Opened {
    /// The pipe, with the end counted, and what its opener should wait for:
    /// 0 if somebody holds the other end, and otherwise a number to hand to
    /// [`wait_peer`], which says how things stood at this moment.
    End(usize, u64),
    /// Asked for an end only if the other is held, and it is not.
    NoPeer,
    /// No room for another pipe, or for another name.
    Full,
}

/// Open one end of the pipe that `key` names for `server`, making the pipe
/// if nobody has it open.
///
/// Finding it, counting the end and looking at the other are one step. In
/// two, a writer could come and go between a reader being given its end and
/// the reader asking whether to wait, and the reader would wait for a writer
/// that had already been. The pipe it may have to make is made before that
/// step — making one asks for a frame — and given back if it was not needed.
pub fn open_named(server: u64, key: u64, write: bool, only_with_peer: bool) -> Opened {
    if server == 0 {
        return Opened::Full;
    }
    let mut spare = make(scheduler::current_tid(), false);
    let held = LOCK.lock();
    let out = unsafe {
        let known = named_pipe(server, key);
        let held = |i: usize| pipes().get(i).is_some_and(|p| if write { p.readers > 0 } else { p.writers > 0 });
        if only_with_peer && !known.is_some_and(held) {
            Opened::NoPeer
        } else {
            let handle = match known {
                Some(i) => Some(i),
                None => match (spare, named().lowest_free(0)) {
                    (Some(i), Some(n)) if named().fill_at(n, Named { server, key, pipe: i }).is_ok() => {
                        spare = None;
                        if let Some(p) = pipes().get(i) {
                            p.named = true;
                        }
                        Some(i)
                    }
                    _ => None,
                },
            };
            match handle.and_then(|i| pipes().get(i).map(|p| (i, p))) {
                None => Opened::Full,
                Some((i, pipe)) => {
                    let (mine, theirs) = if write { (1, 0) } else { (0, 1) };
                    if write {
                        pipe.writers += 1;
                    } else {
                        pipe.readers += 1;
                    }
                    pipe.opens[mine] = pipe.opens[mine].wrapping_add(1);
                    // Whoever was waiting for this end to be opened has what
                    // it was waiting for.
                    waitlist::wake_all(&mut pipe.peer_waiters);
                    let others = if write { pipe.readers } else { pipe.writers };
                    let wait = if others > 0 { 0 } else { since(pipe.opens[theirs]) };
                    Opened::End(i, wait)
                }
            }
        }
    };
    if let Some(i) = spare {
        unsafe { gone(i) };
    }
    drop(held);
    out
}

/// A pipe has gone: whatever key named it names nothing. [`LOCK`] held.
unsafe fn forget_name(handle: usize) {
    unsafe {
        let mut at = 0;
        while let Some(i) = named().next_used(at) {
            at = i + 1;
            if named().get(i).is_some_and(|n| n.pipe == handle) {
                named().empty(i);
            }
        }
    }
}

/// A count of openings as an opener is given it to wait on: never 0, which
/// is what says there is nothing to wait for, and small enough to travel
/// above a descriptor number in one word.
fn since(opens: u32) -> u64 {
    1 + (opens & 0x3FFF_FFFF) as u64
}

/// What waiting for the other end of a pipe came to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// Somebody has it, or has had it since the wait was asked for.
    There,
    /// A signal the program handles arrived first.
    Interrupted,
    /// Not a pipe.
    Failed,
}

/// Wait until somebody opens the other end of a pipe from the one a task
/// has: the write end if it has the read end, and the read end if it has the
/// write. `since` is what [`open_named`] gave with the end. What makes
/// opening a named pipe wait, as it must — a reader that did not would find
/// no writer, which is how a pipe says it has ended.
pub fn wait_peer(handle: usize, is_write: bool, since: u64) -> Peer {
    if since == 0 {
        return Peer::There;
    }
    let tid = scheduler::current_tid();
    loop {
        let held = LOCK.lock();
        unsafe {
            let Some(pipe) = pipes().get(handle) else {
                drop(held);
                return Peer::Failed;
            };
            let (others, opens) = if is_write {
                (pipe.readers, pipe.opens[0])
            } else {
                (pipe.writers, pipe.opens[1])
            };
            if others > 0 || self::since(opens) != since {
                drop(held);
                return Peer::There;
            }
            // Asking whether to wait and parking are one step, as for every
            // wait a signal ends.
            if crate::signal::interrupted(tid) {
                drop(held);
                return Peer::Interrupted;
            }
            waitlist::add(&mut pipe.peer_waiters, tid, On::PipePeer(handle as u32));
            scheduler::block_task(tid);
        }
        drop(held);
        scheduler::yield_now();
    }
}

/// A signal has arrived for a task that may be waiting for the other end of
/// a pipe: if it is, it stops waiting and goes to see.
pub fn interrupt(tid: usize) -> bool {
    // Not under the pipes' lock: `forget` takes it.
    let flags = irq_save();
    let found = unsafe { matches!(waitlist::on(tid), On::PipePeer(_)) && waitlist::forget(tid) };
    if found {
        scheduler::unblock_task(tid);
    }
    irq_restore(flags);
    found
}

/// Every pipe, by its number: made when somebody makes one, its buffer a
/// frame of its own, and given back with its last end (`table.rs`). Number
/// 0 is never one, so that 0 can mean "no pipe". There were 256, each with
/// its 4 KiB inline — a megabyte, spent up front — and 64 a program.
static mut PIPES: crate::table::Table<Pipe> = crate::table::Table::new(crate::table::MOST);

/// # Safety
/// [`LOCK`] held.
#[inline(always)]
unsafe fn pipes() -> &'static mut crate::table::Table<Pipe> {
    unsafe { &mut *core::ptr::addr_of_mut!(PIPES) }
}

/// Save RFLAGS and disable interrupts. Returns saved flags.
#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

/// Restore RFLAGS (re-enabling interrupts if they were enabled before).
#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// A new pipe, made by `creator`: its number, or `None` if there is no room
/// for it — no frame for its buffer, as `reclaim` gives a program frames,
/// or no number left.
fn make(creator: usize, packets: bool) -> Option<usize> {
    if !crate::reclaim::may_make() {
        return None;
    }
    let frame = crate::reclaim::frame()?;
    let held = LOCK.lock();
    let made = unsafe {
        pipes().lowest_free(1).filter(|&i| {
            let mut pipe = Pipe::new(creator, frame as *mut u8);
            pipe.packets = packets;
            pipes().fill_at(i, pipe).is_ok()
        })
    };
    drop(held);
    if made.is_none() {
        crate::pmm::free(crate::pmm::PhysFrame::from_address(frame));
    }
    made
}

/// Pipe `handle` goes: whoever is still on one of its lists looks again and
/// finds nothing, its name names nothing, and its buffer goes back.
///
/// # Safety
/// [`LOCK`] held.
unsafe fn gone(handle: usize) {
    unsafe {
        let Some(pipe) = pipes().get(handle) else { return };
        waitlist::wake_all(&mut pipe.read_waiters);
        waitlist::wake_all(&mut pipe.write_waiters);
        waitlist::wake_all(&mut pipe.peer_waiters);
        let frame = pipe.buf as usize;
        pipes().empty(handle);
        forget_name(handle);
        crate::pmm::free(crate::pmm::PhysFrame::from_address(frame));
    }
}

/// A pipe for a stream, bounded as every pipe is by what the machine has.
pub fn create_for_stream(packets: bool) -> Option<usize> {
    make(scheduler::current_tid(), packets)
}

/// No writer remains, so a read will never block again.
pub fn no_writers(handle: usize) -> bool {
    let held = LOCK.lock();
    let out = unsafe { pipes().get(handle).is_some_and(|p| p.writers == 0) };
    drop(held);
    out
}

/// No reader remains, so a write has nowhere to go.
pub fn no_readers(handle: usize) -> bool {
    let held = LOCK.lock();
    let out = unsafe { pipes().get(handle).is_some_and(|p| p.readers == 0) };
    drop(held);
    out
}

/// The writers have gone. Not the same as there being none: a named pipe
/// opened to read by somebody who would not wait has no writer *yet*, and
/// that is not the end of anything. A program polling it for the first
/// writer's first word would otherwise be told there was something to read,
/// read nothing, and ask again as fast as it could.
pub fn ended(handle: usize) -> bool {
    let held = LOCK.lock();
    let out = unsafe { pipes().get(handle).is_some_and(|p| p.writers == 0 && (p.opens[1] > 0 || !p.named)) };
    drop(held);
    out
}

/// Is a read able to return now — with bytes, or with the end-of-file a
/// departed writer means?
pub fn readable(handle: usize) -> bool {
    let held = LOCK.lock();
    let out = unsafe { pipes().get(handle).is_some_and(|p| p.len > 0) };
    drop(held);
    out || ended(handle)
}

/// Is there room to write?
pub fn writable(handle: usize) -> bool {
    let held = LOCK.lock();
    let out = unsafe { pipes().get(handle).is_some_and(|p| p.room_for(1) > 0) };
    drop(held);
    out
}

/// Free a pipe that was created and never wired to anything.
pub fn drop_unreferenced(handle: usize) {
    let held = LOCK.lock();
    unsafe {
        if pipes().get(handle).is_some_and(|p| p.readers == 0 && p.writers == 0) {
            gone(handle);
        }
    }
    drop(held);
}

/// Create a new pipe. Returns the pipe handle index, from 1.
///
/// `sys_pipe_create` needs no capability — the shell needs it for `|` — and
/// a program is bounded by what `reclaim` lets it have, and how many it can
/// keep by its descriptors. It was 64 a program, counted by space id, and a
/// build running four jobs at once came close to it.
pub fn create() -> Option<usize> {
    make(scheduler::current_tid(), false)
}

/// Release pipes created by a dying task that were never wired to an fd.
///
/// Pipes with live endpoints are refcounted through `cleanup_task_fds`; this
/// only reclaims the ones that never got a reference at all.
pub fn cleanup_orphans(tid: usize) {
    let held = LOCK.lock();
    unsafe {
        let mut at = 1;
        while let Some(i) = pipes().next_used(at) {
            at = i + 1;
            if pipes().get(i).is_some_and(|p| p.creator == tid && p.readers == 0 && p.writers == 0) {
                gone(i);
            }
        }
    }
    drop(held);
}

/// Increment the reader or writer refcount for a pipe.
pub fn add_ref(handle: usize, is_write: bool) -> Result<(), ()> {
    let held = LOCK.lock();
    let result = unsafe {
        match pipes().get(handle) {
            Some(p) => {
                if is_write {
                    p.writers += 1;
                } else {
                    p.readers += 1;
                }
                Ok(())
            }
            None => Err(()),
        }
    };
    drop(held);
    result
}

/// Read from a pipe. Blocks if empty and writers exist. Returns bytes read (0 = EOF).
/// Read from a pipe, then say so.
///
/// The notification is outside the critical section deliberately: waking a set
/// reaches into the task table and the stream table, and doing that with this
/// module's interrupts-off window open would nest two of them.
pub fn read(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
    let n = read_inner(handle, buf, max_len);
    crate::pollset::note_pipe(handle);
    n
}

fn read_inner(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
    unsafe {
        loop {
            let held = LOCK.lock();

            let Some(pipe) = pipes().get(handle) else {
                drop(held);
                return u64::MAX;
            };

            if pipe.len > 0 {
                // Copy data out of ring buffer
                let to_copy = pipe.take(buf, max_len);
                // Wake one blocked writer if any
                waitlist::wake_one(&mut pipe.write_waiters);
                drop(held);
                return to_copy as u64;
            }

            // Buffer empty
            if pipe.writers == 0 {
                drop(held);
                return 0; // EOF
            }

            // A signal with a handler for the kernel to run ends the wait,
            // or stops it beginning: looked for here, with interrupts off.
            let tid = scheduler::current_tid();
            if crate::signal::ends_wait(tid) {
                drop(held);
                return crate::signal::INTERRUPTED;
            }

            // Block until data is available.
            waitlist::add(&mut pipe.read_waiters, tid, On::PipeRead(handle as u32));
            scheduler::block_task(tid);

            drop(held);
            scheduler::yield_now();
            // Loop back to retry (will re-acquire irq_save at top)
        }
    }
}

/// What a non-blocking pipe call returns instead of parking.
///
/// Distinct from both 0 and `u64::MAX`, because all three are different
/// answers: 0 is end of file, `u64::MAX` is a broken pipe, and this is "ask
/// again later". A caller that folded the last two together would report a
/// closed connection every time a buffer happened to be empty.
pub const WOULD_BLOCK: u64 = 0xFFFF_FFFE;

/// Read without parking. Returns bytes read, 0 at end of file,
/// [`WOULD_BLOCK`] if the buffer is empty but a writer still holds the pipe,
/// and `u64::MAX` on error.
pub fn read_nonblock(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
    let n = read_nonblock_inner(handle, buf, max_len);
    if n != WOULD_BLOCK && n != u64::MAX {
        crate::pollset::note_pipe(handle);
    }
    n
}

fn read_nonblock_inner(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
    let held = LOCK.lock();
    let result = unsafe {
        match pipes().get(handle) {
            None => u64::MAX,
            Some(pipe) if pipe.len > 0 => {
                let to_copy = pipe.take(buf, max_len);
                waitlist::wake_one(&mut pipe.write_waiters);
                to_copy as u64
            }
            Some(pipe) if pipe.writers == 0 => 0, // EOF
            Some(_) => WOULD_BLOCK,
        }
    };
    drop(held);
    result
}

/// Write without parking. Returns what fitted, [`WOULD_BLOCK`] if nothing did,
/// and `u64::MAX` on a broken pipe.
///
/// A short write is not a failure here: the caller asked not to wait, and the
/// bytes that fitted are as much progress as waiting would have made in the
/// same instant.
pub fn write_nonblock(handle: usize, buf: *const u8, len: usize) -> u64 {
    let held = LOCK.lock();
    let result = unsafe {
        match pipes().get(handle) {
            None => u64::MAX,
            Some(pipe) if pipe.readers == 0 => u64::MAX,
            // A record bigger than the buffer never fits.
            Some(pipe) if pipe.packets && len + 2 > PIPE_BUF_SIZE => u64::MAX,
            Some(pipe) => {
                let to_copy = pipe.room_for(len).min(len);
                if to_copy == 0 {
                    WOULD_BLOCK
                } else {
                    pipe.put(buf, to_copy);
                    waitlist::wake_one(&mut pipe.read_waiters);
                    to_copy as u64
                }
            }
        }
    };
    drop(held);
    if result != WOULD_BLOCK && result != u64::MAX {
        crate::pollset::note_pipe(handle);
    }
    result
}

/// Write to a pipe. Blocks if full and readers exist. Returns bytes written.
pub fn write(handle: usize, buf: *const u8, len: usize) -> u64 {
    let n = write_inner(handle, buf, len);
    crate::pollset::note_pipe(handle);
    n
}

fn write_inner(handle: usize, buf: *const u8, len: usize) -> u64 {
    unsafe {
        if len == 0 {
            return 0;
        }

        let mut offset = 0usize;

        while offset < len {
            let held = LOCK.lock();

            let Some(pipe) = pipes().get(handle) else {
                drop(held);
                return u64::MAX;
            };

            // Broken pipe — no readers
            if pipe.readers == 0 {
                drop(held);
                return if offset > 0 { offset as u64 } else { u64::MAX };
            }
            // A record bigger than the buffer never fits.
            if pipe.packets && len + 2 > PIPE_BUF_SIZE {
                drop(held);
                return u64::MAX;
            }

            // Bytes as there is room; a record when there is room for all of
            // it, and then all of it at once.
            let space = pipe.room_for(len - offset);
            if space > 0 {
                let to_copy = space.min(len - offset);
                pipe.put(buf.add(offset), to_copy);
                offset += to_copy;

                // Wake one blocked reader if any
                waitlist::wake_one(&mut pipe.read_waiters);

                drop(held);
                // And whoever is polling for it, now, if there is more to
                // write: what is left may wait for room, and room comes only
                // from a reader who knows there is something to read. Told
                // once the whole write was over, a reader that polls — cargo,
                // for what rustc prints — was told nothing while a write of
                // more than the buffer waited for it, and the two waited for
                // each other for good.
                if offset < len {
                    crate::pollset::note_pipe(handle);
                }
            } else {
                // Buffer full — block until space available. A signal ends
                // the wait as it ends a read's, unless some of it went: then
                // that is the answer.
                let tid = scheduler::current_tid();
                if crate::signal::ends_wait(tid) {
                    drop(held);
                    return if offset > 0 { offset as u64 } else { crate::signal::INTERRUPTED };
                }
                waitlist::add(&mut pipe.write_waiters, tid, On::PipeWrite(handle as u32));
                scheduler::block_task(tid);

                drop(held);
                scheduler::yield_now();
                // Loop back to retry (will re-acquire irq_save at top)
            }
        }

        offset as u64
    }
}

/// Drop one descriptor's reference to whatever it names.
///
/// A descriptor belongs to a table and not to a task, so there is nobody to
/// name here: closing one, a program's last task dying and a descriptor
/// dropped with the stream that was carrying it are all this.
pub fn release_fd(kind: &FdKind) {
    match kind {
        FdKind::PipeRead(handle) => drop_ref(*handle, false),
        FdKind::PipeWrite(handle) => drop_ref(*handle, true),
        FdKind::PtyEnd { pty, end } => crate::pty::release(*pty, *end),
        FdKind::Timer { timer } => crate::timerfd::release(*timer),
        FdKind::Event { ev } => crate::eventfd::release(*ev),
        FdKind::MemFd { handle } => crate::shmem::fd_release(*handle),
        FdKind::StreamEnd { stream, end } => crate::stream::close_end(*stream, *end),
        FdKind::PollSet { set } => crate::pollset::destroy(*set),
        FdKind::Served { obj } => crate::served::release(*obj),
        FdKind::Signals { sfd } => crate::sigfd::release(*sfd),
        FdKind::Local { l } => crate::local::release(*l),
        _ => {}
    }
}

/// Take `tid` off the list of tasks parked on what a descriptor names.
///
/// For a task that died where it waited. Every list here is of task ids, and
/// an id is given to the next task made: a wake meant for the dead one would
/// reach whatever has its number now, and if that is blocked on something
/// else — a call to a server — it is woken with nothing, and the call fails.
/// A kind that parks tasks and is missing here leaves that open. A pipe's
/// waiters, a stream's pipes', a counter's, a timer's, a terminal's and a
/// listener's are lists through the waiters' records (`waitlist.rs`), and a
/// task comes off by its own link.
pub fn forget_waiter(kind: &FdKind, tid: usize) -> bool {
    match kind {
        FdKind::PipeRead(_)
        | FdKind::PipeWrite(_)
        | FdKind::StreamEnd { .. }
        | FdKind::Timer { .. }
        | FdKind::Event { .. }
        | FdKind::PtyEnd { .. }
        | FdKind::Local { .. } => {
            // Not under any kind's lock: `forget` takes the one it needs.
            let flags = irq_save();
            let found = unsafe { waitlist::forget(tid) };
            irq_restore(flags);
            found
        }
        _ => false,
    }
}

/// `tid` off the list of the pipe it waits on, if it waits on one — its
/// readers, its writers, or those waiting for its other end: under the
/// pipes' lock (`waitlist::forget`).
///
/// # Safety
/// Interrupts off, and [`LOCK`] not held.
pub unsafe fn forget(tid: usize) -> bool {
    let _held = LOCK.lock();
    unsafe {
        waitlist::forget_held(tid, |on| {
            let (handle, list) = match on {
                On::PipeRead(h) => (h, 0),
                On::PipeWrite(h) => (h, 1),
                On::PipePeer(h) => (h, 2),
                _ => return None,
            };
            Some(pipes().get(handle as usize).map(|p| match list {
                0 => &mut p.read_waiters,
                1 => &mut p.write_waiters,
                _ => &mut p.peer_waiters,
            }))
        })
    }
}

/// Take a reference on whatever a descriptor names, for a copy of it.
///
/// The mirror of `release_fd`, and deliberately beside it: every way of
/// making a second descriptor for one object comes here — `dup`, `fork`, a
/// descriptor put on a stream, a task holding what it is about to wait on —
/// and a kind added to one of these and not the other leaks or double-frees.
///
/// A descriptor on its way down a stream is held by this too. It used to have
/// a count of its own, because a reference was kept per task and a descriptor
/// in flight belongs to no task; with nothing kept per task there is nothing
/// to tell apart, and what the receiver installs is the reference the sender
/// put on the queue.
pub fn retain_fd(kind: &FdKind) -> Result<(), ()> {
    match kind {
        FdKind::PipeRead(handle) => add_ref(*handle, false),
        FdKind::PipeWrite(handle) => add_ref(*handle, true),
        FdKind::MemFd { handle } => {
            if crate::shmem::fd_retain(*handle) { Ok(()) } else { Err(()) }
        }
        FdKind::StreamEnd { stream, end } => crate::stream::retain_end(*stream, *end),
        FdKind::PtyEnd { pty, end } => {
            crate::pty::retain(*pty, *end);
            Ok(())
        }
        FdKind::Timer { timer } => {
            crate::timerfd::retain(*timer);
            Ok(())
        }
        FdKind::Event { ev } => {
            crate::eventfd::retain(*ev);
            Ok(())
        }
        FdKind::Served { obj } => {
            if crate::served::retain(*obj) { Ok(()) } else { Err(()) }
        }
        FdKind::Signals { sfd } => {
            crate::sigfd::retain(*sfd);
            Ok(())
        }
        FdKind::Local { l } => {
            crate::local::retain(*l);
            Ok(())
        }
        // A set counts no holders, and closing any copy destroys it, so it
        // has exactly one.
        FdKind::PollSet { .. } => Err(()),
        _ => Ok(()),
    }
}

pub fn drop_ref(handle: usize, is_write: bool) {
    drop_ref_inner(handle, is_write);
    // A departed writer is end-of-file and a departed reader is a hangup, both
    // of which somebody may be waiting on.
    crate::pollset::note_pipe(handle);
}

fn drop_ref_inner(handle: usize, is_write: bool) {
    let held = LOCK.lock();
    unsafe {
        let Some(pipe) = pipes().get(handle) else {
            drop(held);
            return;
        };
        if is_write {
            pipe.writers = pipe.writers.saturating_sub(1);
            if pipe.writers == 0 {
                // Wake all blocked readers — they'll get EOF
                waitlist::wake_all(&mut pipe.read_waiters);
            }
        } else {
            pipe.readers = pipe.readers.saturating_sub(1);
            if pipe.readers == 0 {
                // Wake all blocked writers — they'll get broken pipe
                waitlist::wake_all(&mut pipe.write_waiters);
            }
        }
        // Free pipe if both sides closed
        if pipe.readers == 0 && pipe.writers == 0 {
            gone(handle);
        }
    }
    drop(held);
}
