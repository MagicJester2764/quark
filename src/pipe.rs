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

/// Pipes in the system.
///
/// Every stream is two of these, so a compositor holding a connection per
/// client spends them quickly: ninety-six is thirty-two ordinary pipes plus
/// two apiece for the thirty-two streams. Each carries a 4 KiB buffer inline,
/// so the number is 384 KiB of kernel memory and is spent up front.
const MAX_PIPES: usize = 96;
const PIPE_BUF_SIZE: usize = 4096;
const MAX_WAITERS: usize = 8;

struct Pipe {
    in_use: bool,
    /// Task that created this pipe, so an orphan (created but never wired to
    /// any fd, hence refcount 0) can be reclaimed when its creator dies.
    /// Without this, sys_pipe_create leaked a slot permanently on every
    /// failed spawn and the 31 slots could be exhausted for good.
    creator: usize,
    /// The *program* that created it, for the cap below.
    ///
    /// A TID is recycled and a space id is not, which matters because a pipe
    /// outlives its creator: its ends are descriptors other tasks hold. Counted
    /// by TID, a fresh task inherited the pipe budget of whatever had its
    /// number before — and a program that had spent its eight left the next
    /// task to take that number unable to make any. dtest found it by asking
    /// for eight pipes after a compositor session had ended.
    owner_space: u64,
    buf: [u8; PIPE_BUF_SIZE],
    read_pos: usize,
    write_pos: usize,
    len: usize,
    readers: usize,
    writers: usize,
    read_waiters: [usize; MAX_WAITERS],
    read_waiter_count: usize,
    write_waiters: [usize; MAX_WAITERS],
    write_waiter_count: usize,
    /// A pipe a server's key names (see [`open_named`]): a FIFO.
    named: bool,
    /// How many times each end of a named pipe has been opened: the reading
    /// end, then the writing. They only go up. Somebody waiting for the
    /// other end to be opened is waiting for one of these to move, not for
    /// the end to be held — a writer that opened, wrote and closed before
    /// the reader ran again has still been, and the reader's wait is over.
    opens: [u32; 2],
    /// Tasks waiting for the other end to be opened by somebody.
    peer_waiters: [usize; MAX_WAITERS],
    peer_waiter_count: usize,
}

impl Pipe {
    const fn new() -> Self {
        Pipe {
            in_use: false,
            creator: 0,
            owner_space: 0,
            buf: [0; PIPE_BUF_SIZE],
            read_pos: 0,
            write_pos: 0,
            len: 0,
            readers: 0,
            writers: 0,
            read_waiters: [0; MAX_WAITERS],
            read_waiter_count: 0,
            write_waiters: [0; MAX_WAITERS],
            write_waiter_count: 0,
            named: false,
            opens: [0; 2],
            peer_waiters: [0; MAX_WAITERS],
            peer_waiter_count: 0,
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
    /// The server's endpoint number; 0 is a free entry.
    server: u64,
    key: u64,
    pipe: usize,
}

/// More than a system has names for at once, and a table small enough to
/// search.
const MAX_NAMED: usize = 32;
static mut NAMED: [Named; MAX_NAMED] = [Named { server: 0, key: 0, pipe: 0 }; MAX_NAMED];

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
/// that had already been.
///
/// Not counted against any program's pipes: the server did not ask for a
/// pipe, a client of it opened a name.
pub fn open_named(server: u64, key: u64, write: bool, only_with_peer: bool) -> Opened {
    if server == 0 {
        return Opened::Full;
    }
    let creator = scheduler::current_tid();
    let flags = irq_save();
    let out = unsafe {
        let table = &mut *core::ptr::addr_of_mut!(NAMED);
        let known = table.iter().position(|n| n.server == server && n.key == key);
        let held = |i: usize| if write { PIPES[i].readers > 0 } else { PIPES[i].writers > 0 };
        if only_with_peer && !known.is_some_and(|slot| held(table[slot].pipe)) {
            Opened::NoPeer
        } else {
            let handle = match known {
                Some(slot) => Some(table[slot].pipe),
                None => {
                    let free = table.iter().position(|n| n.server == 0);
                    let pipe = (1..MAX_PIPES).find(|&i| !PIPES[i].in_use);
                    match (free, pipe) {
                        (Some(slot), Some(i)) => {
                            PIPES[i] = Pipe::new();
                            PIPES[i].in_use = true;
                            PIPES[i].creator = creator;
                            PIPES[i].named = true;
                            table[slot] = Named { server, key, pipe: i };
                            Some(i)
                        }
                        _ => None,
                    }
                }
            };
            match handle {
                None => Opened::Full,
                Some(i) => {
                    let pipe = &mut PIPES[i];
                    let (mine, theirs) = if write { (1, 0) } else { (0, 1) };
                    if write {
                        pipe.writers += 1;
                    } else {
                        pipe.readers += 1;
                    }
                    pipe.opens[mine] = pipe.opens[mine].wrapping_add(1);
                    // Whoever was waiting for this end to be opened has what
                    // it was waiting for.
                    for w in 0..pipe.peer_waiter_count {
                        scheduler::unblock_task(pipe.peer_waiters[w]);
                    }
                    pipe.peer_waiter_count = 0;
                    let others = if write { pipe.readers } else { pipe.writers };
                    let wait = if others > 0 { 0 } else { since(pipe.opens[theirs]) };
                    Opened::End(i, wait)
                }
            }
        }
    };
    irq_restore(flags);
    out
}

/// A pipe has gone: whatever key named it names nothing. Interrupts are off.
unsafe fn forget_name(handle: usize) {
    unsafe {
        for n in (*core::ptr::addr_of_mut!(NAMED)).iter_mut() {
            if n.server != 0 && n.pipe == handle {
                n.server = 0;
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
    /// Not a pipe, or too many are waiting on this one already.
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
        let flags = irq_save();
        unsafe {
            if handle >= MAX_PIPES || !PIPES[handle].in_use {
                irq_restore(flags);
                return Peer::Failed;
            }
            let pipe = &mut PIPES[handle];
            let (others, opens) = if is_write {
                (pipe.readers, pipe.opens[0])
            } else {
                (pipe.writers, pipe.opens[1])
            };
            if others > 0 || self::since(opens) != since {
                irq_restore(flags);
                return Peer::There;
            }
            // Asking whether to wait and parking are one step, as for every
            // wait a signal ends.
            if crate::signal::interrupted(tid) {
                irq_restore(flags);
                return Peer::Interrupted;
            }
            if pipe.peer_waiter_count >= MAX_WAITERS {
                irq_restore(flags);
                return Peer::Failed;
            }
            pipe.peer_waiters[pipe.peer_waiter_count] = tid;
            pipe.peer_waiter_count += 1;
            scheduler::block_task(tid);
        }
        irq_restore(flags);
        scheduler::yield_now();
    }
}

/// A signal has arrived for a task that may be waiting for the other end of
/// a pipe: if it is, it stops waiting and goes to see.
pub fn interrupt(tid: usize) -> bool {
    let mut found = false;
    let flags = irq_save();
    unsafe {
        for i in 1..MAX_PIPES {
            let pipe = &mut PIPES[i];
            if pipe.in_use && pipe.peer_waiter_count > 0 {
                let before = pipe.peer_waiter_count;
                forget_in(&mut pipe.peer_waiters, &mut pipe.peer_waiter_count, tid);
                found |= pipe.peer_waiter_count != before;
            }
        }
    }
    irq_restore(flags);
    if found {
        scheduler::unblock_task(tid);
    }
    found
}

static mut PIPES: [Pipe; MAX_PIPES] = {
    const P: Pipe = Pipe::new();
    [P; MAX_PIPES]
};

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

/// Maximum pipes a single task may hold open at once.
const MAX_PIPES_PER_TASK: usize = 8;

/// Create a new pipe. Returns the pipe handle index.
/// Handles start at 1 (slot 0 is reserved so that 0 can mean "no pipe").
/// A pipe for a stream, exempt from the per-task cap.
///
/// That cap exists because `sys_pipe_create` needs no capability, so one task
/// could otherwise drain the table. A stream is bounded by its own table
/// instead, and charging its two pipes against a task's eight would have meant
/// four connections per program.
pub fn create_for_stream() -> Option<usize> {
    let creator = scheduler::current_tid();
    let space = scheduler::space_of_task(creator);
    let flags = irq_save();
    let result = unsafe {
        let mut found = None;
        for i in 1..MAX_PIPES {
            if !PIPES[i].in_use {
                PIPES[i] = Pipe::new();
                PIPES[i].in_use = true;
                PIPES[i].creator = creator;
                PIPES[i].owner_space = space;
                found = Some(i);
                break;
            }
        }
        found
    };
    irq_restore(flags);
    result
}

/// No writer remains, so a read will never block again.
pub fn no_writers(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe { handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].writers == 0 };
    irq_restore(flags);
    out
}

/// No reader remains, so a write has nowhere to go.
pub fn no_readers(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe { handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].readers == 0 };
    irq_restore(flags);
    out
}

/// The writers have gone. Not the same as there being none: a named pipe
/// opened to read by somebody who would not wait has no writer *yet*, and
/// that is not the end of anything. A program polling it for the first
/// writer's first word would otherwise be told there was something to read,
/// read nothing, and ask again as fast as it could.
pub fn ended(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        handle < MAX_PIPES && PIPES[handle].in_use && {
            let pipe = &PIPES[handle];
            pipe.writers == 0 && (pipe.opens[1] > 0 || !pipe.named)
        }
    };
    irq_restore(flags);
    out
}

/// Is a read able to return now — with bytes, or with the end-of-file a
/// departed writer means?
pub fn readable(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe { handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].len > 0 };
    irq_restore(flags);
    out || ended(handle)
}

/// Is there room to write?
pub fn writable(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].len < PIPE_BUF_SIZE
    };
    irq_restore(flags);
    out
}

/// Free a pipe that was created and never wired to anything.
pub fn drop_unreferenced(handle: usize) {
    let flags = irq_save();
    unsafe {
        if handle < MAX_PIPES
            && PIPES[handle].in_use
            && PIPES[handle].readers == 0
            && PIPES[handle].writers == 0
        {
            PIPES[handle].in_use = false;
            forget_name(handle);
        }
    }
    irq_restore(flags);
}

pub fn create() -> Option<usize> {
    let creator = scheduler::current_tid();
    let space = scheduler::space_of_task(creator);
    let flags = irq_save();
    let result = unsafe {
        // Per-program cap: sys_pipe_create needs no capability (the shell needs
        // it for `|`), so bound it here rather than letting one program drain
        // the global table. By program rather than by task, because a space id
        // is never reused and a TID is: see `owner_space`.
        let held = (1..MAX_PIPES)
            .filter(|&i| PIPES[i].in_use && PIPES[i].owner_space == space)
            .count();
        if held >= MAX_PIPES_PER_TASK {
            None
        } else {
            let mut found = None;
            for i in 1..MAX_PIPES {
                if !PIPES[i].in_use {
                    PIPES[i] = Pipe::new();
                    PIPES[i].in_use = true;
                    PIPES[i].creator = creator;
                    PIPES[i].owner_space = space;
                    found = Some(i);
                    break;
                }
            }
            found
        }
    };
    irq_restore(flags);
    result
}

/// Release pipes created by a dying task that were never wired to an fd.
///
/// Pipes with live endpoints are refcounted through `cleanup_task_fds`; this
/// only reclaims the ones that never got a reference at all.
pub fn cleanup_orphans(tid: usize) {
    let flags = irq_save();
    unsafe {
        for i in 1..MAX_PIPES {
            if PIPES[i].in_use
                && PIPES[i].creator == tid
                && PIPES[i].readers == 0
                && PIPES[i].writers == 0
            {
                PIPES[i].in_use = false;
                forget_name(i);
            }
        }
    }
    irq_restore(flags);
}

/// Increment the reader or writer refcount for a pipe.
pub fn add_ref(handle: usize, is_write: bool) -> Result<(), ()> {
    let flags = irq_save();
    let result = unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            Err(())
        } else {
            if is_write {
                PIPES[handle].writers += 1;
            } else {
                PIPES[handle].readers += 1;
            }
            Ok(())
        }
    };
    irq_restore(flags);
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
            let flags = irq_save();

            if handle >= MAX_PIPES || !PIPES[handle].in_use {
                irq_restore(flags);
                return u64::MAX;
            }

            let pipe = &mut PIPES[handle];

            if pipe.len > 0 {
                // Copy data out of ring buffer
                let to_copy = pipe.len.min(max_len);
                {
                    let _ua = crate::cpu::UserAccess::begin();
                    for i in 0..to_copy {
                        let pos = (pipe.read_pos + i) % PIPE_BUF_SIZE;
                        buf.add(i).write(pipe.buf[pos]);
                    }
                }
                pipe.read_pos = (pipe.read_pos + to_copy) % PIPE_BUF_SIZE;
                pipe.len -= to_copy;

                // Wake one blocked writer if any
                if pipe.write_waiter_count > 0 {
                    let tid = pipe.write_waiters[0];
                    pipe.write_waiter_count -= 1;
                    for j in 0..pipe.write_waiter_count {
                        pipe.write_waiters[j] = pipe.write_waiters[j + 1];
                    }
                    scheduler::unblock_task(tid);
                }

                irq_restore(flags);
                return to_copy as u64;
            }

            // Buffer empty
            if pipe.writers == 0 {
                irq_restore(flags);
                return 0; // EOF
            }

            // A signal with a handler for the kernel to run ends the wait,
            // or stops it beginning: looked for here, with interrupts off.
            let tid = scheduler::current_tid();
            if crate::signal::ends_wait(tid) {
                irq_restore(flags);
                return crate::signal::INTERRUPTED;
            }

            // Block until data is available. If the waiter table is full we
            // must NOT block -- an unregistered waiter is never woken, so the
            // 9th reader used to sleep forever.
            if pipe.read_waiter_count >= MAX_WAITERS {
                irq_restore(flags);
                return u64::MAX;
            }
            pipe.read_waiters[pipe.read_waiter_count] = tid;
            pipe.read_waiter_count += 1;
            scheduler::block_task(tid);

            irq_restore(flags);
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
    let flags = irq_save();
    let result = unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            u64::MAX
        } else {
            let pipe = &mut PIPES[handle];

            if pipe.len > 0 {
                let to_copy = pipe.len.min(max_len);
                {
                    let _ua = crate::cpu::UserAccess::begin();
                    for i in 0..to_copy {
                        let pos = (pipe.read_pos + i) % PIPE_BUF_SIZE;
                        buf.add(i).write(pipe.buf[pos]);
                    }
                }
                pipe.read_pos = (pipe.read_pos + to_copy) % PIPE_BUF_SIZE;
                pipe.len -= to_copy;

                if pipe.write_waiter_count > 0 {
                    let tid = pipe.write_waiters[0];
                    pipe.write_waiter_count -= 1;
                    for j in 0..pipe.write_waiter_count {
                        pipe.write_waiters[j] = pipe.write_waiters[j + 1];
                    }
                    scheduler::unblock_task(tid);
                }

                to_copy as u64
            } else if pipe.writers == 0 {
                0 // EOF
            } else {
                WOULD_BLOCK
            }
        }
    };
    irq_restore(flags);
    result
}

/// Write without parking. Returns what fitted, [`WOULD_BLOCK`] if nothing did,
/// and `u64::MAX` on a broken pipe.
///
/// A short write is not a failure here: the caller asked not to wait, and the
/// bytes that fitted are as much progress as waiting would have made in the
/// same instant.
pub fn write_nonblock(handle: usize, buf: *const u8, len: usize) -> u64 {
    let flags = irq_save();
    let result = unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            u64::MAX
        } else {
            let pipe = &mut PIPES[handle];
            if pipe.readers == 0 {
                u64::MAX
            } else {
                let space = PIPE_BUF_SIZE - pipe.len;
                let to_copy = space.min(len);
                if to_copy == 0 {
                    WOULD_BLOCK
                } else {
                    {
                        let _ua = crate::cpu::UserAccess::begin();
                        for i in 0..to_copy {
                            let pos = (pipe.write_pos + i) % PIPE_BUF_SIZE;
                            pipe.buf[pos] = buf.add(i).read();
                        }
                    }
                    pipe.write_pos = (pipe.write_pos + to_copy) % PIPE_BUF_SIZE;
                    pipe.len += to_copy;

                    if pipe.read_waiter_count > 0 {
                        let tid = pipe.read_waiters[0];
                        pipe.read_waiter_count -= 1;
                        for j in 0..pipe.read_waiter_count {
                            pipe.read_waiters[j] = pipe.read_waiters[j + 1];
                        }
                        scheduler::unblock_task(tid);
                    }
                    to_copy as u64
                }
            }
        }
    };
    irq_restore(flags);
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
            let flags = irq_save();

            if handle >= MAX_PIPES || !PIPES[handle].in_use {
                irq_restore(flags);
                return u64::MAX;
            }

            let pipe = &mut PIPES[handle];

            // Broken pipe — no readers
            if pipe.readers == 0 {
                irq_restore(flags);
                return if offset > 0 { offset as u64 } else { u64::MAX };
            }

            let space = PIPE_BUF_SIZE - pipe.len;
            if space > 0 {
                let to_copy = space.min(len - offset);
                {
                    let _ua = crate::cpu::UserAccess::begin();
                    for i in 0..to_copy {
                        let pos = (pipe.write_pos + i) % PIPE_BUF_SIZE;
                        pipe.buf[pos] = buf.add(offset + i).read();
                    }
                }
                pipe.write_pos = (pipe.write_pos + to_copy) % PIPE_BUF_SIZE;
                pipe.len += to_copy;
                offset += to_copy;

                // Wake one blocked reader if any
                if pipe.read_waiter_count > 0 {
                    let tid = pipe.read_waiters[0];
                    pipe.read_waiter_count -= 1;
                    for j in 0..pipe.read_waiter_count {
                        pipe.read_waiters[j] = pipe.read_waiters[j + 1];
                    }
                    scheduler::unblock_task(tid);
                }

                irq_restore(flags);
            } else {
                // Buffer full — block until space available. Same rule as the
                // read path: no waiter slot means no wakeup, so fail instead.
                // And a signal ends the wait as it ends a read's, unless some
                // of it went: then that is the answer.
                let tid = scheduler::current_tid();
                if crate::signal::ends_wait(tid) {
                    irq_restore(flags);
                    return if offset > 0 { offset as u64 } else { crate::signal::INTERRUPTED };
                }
                if pipe.write_waiter_count >= MAX_WAITERS {
                    irq_restore(flags);
                    return if offset > 0 { offset as u64 } else { u64::MAX };
                }
                pipe.write_waiters[pipe.write_waiter_count] = tid;
                pipe.write_waiter_count += 1;
                scheduler::block_task(tid);

                irq_restore(flags);
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
        _ => {}
    }
}

/// Take `tid` off the list of tasks parked on what a descriptor names.
///
/// For a task that died where it waited. Every list here is of task ids, and
/// an id is given to the next task made: a wake meant for the dead one would
/// reach whatever has its number now, and if that is blocked on something
/// else — a call to a server — it is woken with nothing, and the call fails.
/// A kind that parks tasks and is missing here leaves that open.
pub fn forget_waiter(kind: &FdKind, tid: usize) -> bool {
    match kind {
        FdKind::PipeRead(handle) | FdKind::PipeWrite(handle) => forget_on_pipe(*handle, tid),
        FdKind::StreamEnd { stream, end } => match crate::stream::pipes_for(*stream, *end) {
            Some((rd, wr)) => forget_on_pipe(rd, tid) | forget_on_pipe(wr, tid),
            None => false,
        },
        FdKind::PtyEnd { pty, .. } => crate::pty::forget_waiter(*pty, tid),
        FdKind::Timer { timer } => crate::timerfd::forget_waiter(*timer, tid),
        FdKind::Event { ev } => crate::eventfd::forget_waiter(*ev, tid),
        _ => false,
    }
}

/// `tid` out of a list of `count` waiters, the rest closed up. True if it
/// was on it.
pub fn forget_in(list: &mut [usize], count: &mut usize, tid: usize) -> bool {
    let mut kept = 0;
    for i in 0..*count {
        if list[i] != tid {
            list[kept] = list[i];
            kept += 1;
        }
    }
    let found = kept != *count;
    *count = kept;
    found
}

fn forget_on_pipe(handle: usize, tid: usize) -> bool {
    if handle >= MAX_PIPES {
        return false;
    }
    let flags = irq_save();
    let found = unsafe {
        let pipe = &mut PIPES[handle];
        pipe.in_use
            && (forget_in(&mut pipe.read_waiters, &mut pipe.read_waiter_count, tid)
                | forget_in(&mut pipe.write_waiters, &mut pipe.write_waiter_count, tid)
                | forget_in(&mut pipe.peer_waiters, &mut pipe.peer_waiter_count, tid))
    };
    irq_restore(flags);
    found
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
    let flags = irq_save();
    unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            irq_restore(flags);
            return;
        }
        let pipe = &mut PIPES[handle];
        if is_write {
            pipe.writers = pipe.writers.saturating_sub(1);
            if pipe.writers == 0 {
                // Wake all blocked readers — they'll get EOF
                for i in 0..pipe.read_waiter_count {
                    scheduler::unblock_task(pipe.read_waiters[i]);
                }
                pipe.read_waiter_count = 0;
            }
        } else {
            pipe.readers = pipe.readers.saturating_sub(1);
            if pipe.readers == 0 {
                // Wake all blocked writers — they'll get broken pipe
                for i in 0..pipe.write_waiter_count {
                    scheduler::unblock_task(pipe.write_waiters[i]);
                }
                pipe.write_waiter_count = 0;
            }
        }
        // Free pipe if both sides closed
        if pipe.readers == 0 && pipe.writers == 0 {
            pipe.in_use = false;
            forget_name(handle);
        }
    }
    irq_restore(flags);
}
