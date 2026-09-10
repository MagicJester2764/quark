/// Kernel pipes for the Quark microkernel.
///
/// Anonymous byte-stream channels with a fixed-size ring buffer.
/// Blocking semantics: reader blocks if empty, writer blocks if full.
///
/// All pipe operations disable interrupts to prevent data races:
/// the timer interrupt can preempt syscall handlers and context-switch
/// to another task that accesses the same pipe concurrently.

use crate::scheduler;
use crate::task::{FdKind, MAX_FDS};

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
}

impl Pipe {
    const fn new() -> Self {
        Pipe {
            in_use: false,
            creator: 0,
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
        }
    }
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
    let flags = irq_save();
    let result = unsafe {
        let mut found = None;
        for i in 1..MAX_PIPES {
            if !PIPES[i].in_use {
                PIPES[i] = Pipe::new();
                PIPES[i].in_use = true;
                PIPES[i].creator = creator;
                found = Some(i);
                break;
            }
        }
        found
    };
    irq_restore(flags);
    result
}

/// Is a read able to return now — with bytes, or with the end-of-file a
/// departed writer means?
pub fn readable(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        handle < MAX_PIPES
            && PIPES[handle].in_use
            && (PIPES[handle].len > 0 || PIPES[handle].writers == 0)
    };
    irq_restore(flags);
    out
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
        }
    }
    irq_restore(flags);
}

pub fn create() -> Option<usize> {
    let creator = scheduler::current_tid();
    let flags = irq_save();
    let result = unsafe {
        // Per-task cap: sys_pipe_create needs no capability (the shell needs
        // it for `|`), so bound it here rather than letting one task drain the
        // global table.
        let held = (1..MAX_PIPES)
            .filter(|&i| PIPES[i].in_use && PIPES[i].creator == creator)
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
pub fn read(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
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

            // Block until data is available. If the waiter table is full we
            // must NOT block -- an unregistered waiter is never woken, so the
            // 9th reader used to sleep forever.
            let tid = scheduler::current_tid();
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

/// Non-blocking pipe read. Returns bytes read, 0 if empty (with writers), u64::MAX on error.
/// Returns 0 with a special marker: if no writers remain, returns 0 (EOF).
/// To distinguish empty-with-writers from EOF, we use a convention:
/// 0 = EOF (no writers), 0xFFFF_FFFE = would block (empty but writers exist).
pub fn read_nonblock(handle: usize, buf: *mut u8, max_len: usize) -> u64 {
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
                0xFFFF_FFFE // would block
            }
        }
    };
    irq_restore(flags);
    result
}

/// Write to a pipe. Blocks if full and readers exist. Returns bytes written.
pub fn write(handle: usize, buf: *const u8, len: usize) -> u64 {
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
                let tid = scheduler::current_tid();
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

/// Clean up pipe references when a task dies.
/// Decrements refcounts and wakes blocked waiters.
pub fn cleanup_task_fds(fds: &[FdKind; MAX_FDS], owner: usize) {
    for fd in fds.iter() {
        release_fd(fd, owner);
    }
}

/// Drop one descriptor's reference to whatever it names.
///
/// `cleanup_task_fds` does this for a whole table when a task dies; a task
/// closing one descriptor needs exactly the same work for one entry.
///
/// `owner` is passed rather than read from the current task: the reaper runs
/// this over a *dead* task's table, and it is not that task.
pub fn release_fd(kind: &FdKind, owner: usize) {
    match kind {
        FdKind::PipeRead(handle) => drop_ref(*handle, false),
        FdKind::PipeWrite(handle) => drop_ref(*handle, true),
        FdKind::MemFd { handle } => crate::shmem::close_ref(*handle, owner),
        FdKind::StreamEnd { stream, end } => crate::stream::close_end(*stream, *end),
        _ => {}
    }
}

/// Take a reference on whatever a descriptor names, for a copy of it.
///
/// The mirror of `release_fd`, and deliberately beside it: `SYS_FD_DUP` and
/// `SYS_PIPE_FD_SET` both make a second descriptor for one object, and a kind
/// added to one of these and not the other leaks or double-frees.
/// Take a reference for a descriptor in flight, belonging to no task yet.
///
/// A queued descriptor has no owner — the receiver is not decided until it
/// calls recv — so this is the tid-free counterpart of `retain_fd`, and its
/// reference is released by `release_in_flight` whether the descriptor
/// arrives or is dropped with the stream carrying it.
pub fn retain_in_flight(kind: &FdKind) -> Result<(), ()> {
    match kind {
        FdKind::PipeRead(handle) => add_ref(*handle, false),
        FdKind::PipeWrite(handle) => add_ref(*handle, true),
        FdKind::MemFd { handle } => {
            if crate::shmem::hold_in_flight(*handle) { Ok(()) } else { Err(()) }
        }
        FdKind::StreamEnd { stream, end } => crate::stream::retain_end(*stream, *end),
        _ => Ok(()),
    }
}

/// Give back what `retain_in_flight` took.
pub fn release_in_flight(kind: &FdKind) {
    match kind {
        FdKind::PipeRead(handle) => drop_ref(*handle, false),
        FdKind::PipeWrite(handle) => drop_ref(*handle, true),
        FdKind::MemFd { handle } => crate::shmem::drop_in_flight(*handle),
        FdKind::StreamEnd { stream, end } => crate::stream::close_end(*stream, *end),
        _ => {}
    }
}

/// `owner` is the task the *new* descriptor will belong to, which for
/// `SYS_FD_DUP` is the target rather than the caller.
pub fn retain_fd(kind: &FdKind, owner: usize) -> Result<(), ()> {
    match kind {
        FdKind::PipeRead(handle) => add_ref(*handle, false),
        FdKind::PipeWrite(handle) => add_ref(*handle, true),
        FdKind::MemFd { handle } => {
            // Whoever ends up holding the copy may map it.
            if crate::shmem::add_access(*handle, owner) { Ok(()) } else { Err(()) }
        }
        FdKind::StreamEnd { stream, end } => crate::stream::retain_end(*stream, *end),
        _ => Ok(()),
    }
}

pub fn drop_ref(handle: usize, is_write: bool) {
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
        }
    }
    irq_restore(flags);
}
