/// Synchronous IPC (message passing) for the Quark microkernel.
///
/// Tasks communicate by sending/receiving fixed-size messages.
/// Messages fit in registers for zero-copy small transfers.
///
/// All IPC operations disable interrupts around critical sections to prevent
/// races: the timer interrupt can preempt syscall handlers and context-switch
/// to another task that accesses the same IPC state.

use crate::scheduler;

pub const TID_ANY: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    InvalidTid,
    DeadTask,
    WouldBlock,
    NotWaiting,
    Timeout,
}

/// Fixed-size IPC message: sender TID, tag, and 6 payload words.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    pub sender: usize,
    pub tag: u64,
    pub data: [u64; 6],
}

impl Message {
    pub const fn empty() -> Self {
        Message {
            sender: 0,
            tag: 0,
            data: [0; 6],
        }
    }
}

/// Per-task IPC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcState {
    None,
    /// Blocked waiting to send to a specific TID.
    SendBlocked(usize),
    /// Blocked waiting to receive from a specific TID (or TID_ANY).
    RecvBlocked(usize),
    /// Blocked in sys_call send phase: message not yet picked up by receiver.
    /// When sys_recv picks this up, it transitions to CallBlocked (stays blocked).
    CallSendBlocked(usize),
    /// Blocked in sys_call (send+recv): waiting for reply from dest.
    CallBlocked(usize),
}

/// IPC state and pending message for each task.
struct TaskIpc {
    state: IpcState,
    pending_msg: Option<Message>,
}

const MAX_TASKS: usize = 64;
static mut TASK_IPC: [TaskIpc; MAX_TASKS] = {
    const INIT: TaskIpc = TaskIpc {
        state: IpcState::None,
        pending_msg: None,
    };
    [INIT; MAX_TASKS]
};

/// Per-task timeout deadline (PIT tick count). 0 = no timeout.
static mut TASK_TIMEOUT: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Set by `check_timeouts` when it abandons a task's blocking call, so the
/// caller can tell "nobody answered in time" from "the target died".
static mut TASK_TIMED_OUT: [bool; MAX_TASKS] = [false; MAX_TASKS];

/// Where each receiver's next scan for a waiting sender begins.
///
/// The scan used to start at TID 0 every time, which is not a queue but a
/// priority order: the lowest-numbered sender blocked on a service is served,
/// and if it blocks again before that service scans once more, it is served
/// again. A higher-numbered sender behind it never runs. Two clients polling
/// one server is enough to reproduce it — a compositor with two windows had
/// the second one wait forever for a reply to its first message.
///
/// Starting one past whoever was served last makes it a round robin: every
/// waiting sender is reached within one turn of the table.
static mut RECV_ROTOR: [usize; MAX_TASKS] = [0; MAX_TASKS];

/// Per-task notification word (seL4-style). Bits are OR'd in by sys_notify().
/// Atomically read-and-cleared when consumed by sys_recv/sys_recv_timeout.
static mut TASK_NOTIFY: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Who has asked to be told when each task dies.
///
/// `WATCHERS[t]` is a bitmask of tasks wanting to hear about `t`, the same
/// shape as an `Endpoint` set and for the same reason: TIDs are small and
/// there are only 64 of them.
static mut WATCHERS: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Deaths a watcher has been told about and has not yet collected.
///
/// Shallow on purpose. A watcher exists to reclaim something the dead task
/// held — a display, a window, a keyboard — and one that has let eight deaths
/// pile up unread is not doing that. Dropping the ninth loses a reclaim; a
/// deeper queue would only lose the twenty-fifth.
const DEATH_QUEUE: usize = 8;
static mut DEATHS: [[u8; DEATH_QUEUE]; MAX_TASKS] = [[0; DEATH_QUEUE]; MAX_TASKS];
static mut DEATHS_LEN: [usize; MAX_TASKS] = [0; MAX_TASKS];

/// Per-task signal kill deadline (PIT tick). 0 = no pending signal deadline.
/// When nonzero, the task will be force-killed after the deadline expires.
static mut SIGNAL_DEADLINE: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Tag for notification messages delivered to user space.
pub const TAG_NOTIFICATION: u64 = 0xFFFF_0002;

/// Tag for a death notification: `data[0]` is the task that died.
pub const TAG_TASK_DIED: u64 = 0xFFFF_0003;

// Signal badge bits (use high bits to avoid collision with app badges)
pub const SIG_INT: u64 = 1 << 16;
pub const SIG_TERM: u64 = 1 << 17;
pub const SIG_KILL: u64 = 1 << 18;
pub const SIG_MASK: u64 = SIG_INT | SIG_TERM | SIG_KILL;

/// Ticks before a signaled task is force-killed (5 seconds at 100 Hz).
const SIGNAL_KILL_TIMEOUT: u64 = 500;

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

/// Asynchronous notification: OR `badge` into dest's notification word.
/// Non-blocking. Wakes the dest task if it is RecvBlocked(0) or RecvBlocked(TID_ANY).
/// Ask to be told when `target` dies.
///
/// No capability is required, and deliberately: `SYS_TASK_INFO` already tells
/// anyone whether a given task is alive, so a watch reveals nothing that was
/// not already there for the asking. What it removes is the polling — and the
/// window between polls, which is where a display stays claimed by a task that
/// no longer exists.
///
/// The registration is dropped when either task dies, so a watcher is never
/// told about the next occupant of a recycled TID.
pub fn sys_task_watch(watcher: usize, target: usize) -> Result<(), IpcError> {
    if target >= MAX_TASKS || watcher >= MAX_TASKS || target == watcher {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(target) {
        // Already gone. Say so now rather than promising news that will never
        // come: a caller told "no such task" can reclaim immediately, and one
        // left waiting for a notification cannot.
        return Err(IpcError::DeadTask);
    }
    let flags = irq_save();
    unsafe { WATCHERS[target] |= 1u64 << watcher };
    irq_restore(flags);
    Ok(())
}

/// Tell everyone watching `dead` that it has gone.
///
/// Called when the task is marked Dead, not when it is reaped: reaping waits
/// on a parent that may never call `sys_wait`, and a compositor holding the
/// screen for a program that exited ten minutes ago is the thing this exists
/// to prevent.
pub fn notify_watchers(dead: usize) {
    if dead >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let mut mask = WATCHERS[dead];
        WATCHERS[dead] = 0;
        while mask != 0 {
            let w = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            if DEATHS_LEN[w] < DEATH_QUEUE {
                DEATHS[w][DEATHS_LEN[w]] = dead as u8;
                DEATHS_LEN[w] += 1;
            }
            // Wake it if it is sitting in a receive that would take this.
            match TASK_IPC[w].state {
                IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                    TASK_IPC[w].state = IpcState::None;
                    scheduler::unblock_task(w);
                }
                _ => {}
            }
        }
    }
    irq_restore(flags);
}

/// Take one pending death notification, if there is one.
///
/// # Safety
/// The caller holds interrupts off.
unsafe fn take_death(receiver: usize) -> Option<Message> {
    unsafe {
        if DEATHS_LEN[receiver] == 0 {
            return None;
        }
        let dead = DEATHS[receiver][0] as u64;
        for i in 1..DEATHS_LEN[receiver] {
            DEATHS[receiver][i - 1] = DEATHS[receiver][i];
        }
        DEATHS_LEN[receiver] -= 1;
        Some(Message { sender: 0, tag: TAG_TASK_DIED, data: [dead, 0, 0, 0, 0, 0] })
    }
}

pub fn sys_notify(dest: usize, badge: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || badge == 0 {
        return Err(IpcError::InvalidTid);
    }
    // Reserved signal bits may only be raised through sys_signal, which checks
    // the caller's authority over the target. Allowing them here let any task
    // forge a signal into any other task's notification word.
    if badge & SIG_MASK != 0 {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }

    let flags = irq_save();
    unsafe {
        TASK_NOTIFY[dest] |= badge;

        // Wake the task if it's recv-blocked and would accept a notification
        match TASK_IPC[dest].state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {}
        }
    }
    irq_restore(flags);

    Ok(())
}

/// Raise `badge` in `dest`'s notification word without filtering reserved
/// bits. Only `sys_signal` may use this, after its own permission check.
fn notify_raw(dest: usize, badge: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || badge == 0 {
        return Err(IpcError::InvalidTid);
    }

    let flags = irq_save();
    unsafe {
        TASK_NOTIFY[dest] |= badge;

        match TASK_IPC[dest].state {
            IpcState::RecvBlocked(from) if from == 0 || from == TID_ANY => {
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {}
        }
    }
    irq_restore(flags);

    Ok(())
}

/// Send a signal to a task. SIG_KILL immediately kills; other signals are
/// delivered as notification badges with a force-kill deadline.
/// Permission checking is done in syscall_dispatch (same as sys_task_kill).
///
/// Unlike plain `sys_notify`, this also interrupts tasks blocked in IPC calls
/// (CallBlocked, CallSendBlocked, SendBlocked, RecvBlocked with specific sender).
/// The interrupted syscall returns an error, allowing the task to check its
/// notification word and handle the signal.
pub fn sys_signal(dest: usize, sig: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || dest <= 1 {
        return Err(IpcError::InvalidTid);
    }
    if sig == 0 {
        return Err(IpcError::InvalidTid);
    }

    // SIG_KILL: immediate termination, no grace period
    if sig & SIG_KILL != 0 {
        let _ = scheduler::kill_task(dest);
        return Ok(());
    }

    // Deliver signal bits via notification word and wake if RecvBlocked(0|TID_ANY).
    // Uses the privileged form: sys_notify refuses reserved signal bits.
    notify_raw(dest, sig)?;

    // Force-unblock from IPC states that sys_notify doesn't handle.
    let flags = irq_save();
    unsafe {
        match TASK_IPC[dest].state {
            IpcState::CallBlocked(_) | IpcState::CallSendBlocked(_) | IpcState::SendBlocked(_) => {
                TASK_IPC[dest].pending_msg = None;
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            IpcState::RecvBlocked(from) if from != 0 && from != TID_ANY => {
                // sys_notify only handles RecvBlocked(0|TID_ANY); interrupt specific waits too
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
            }
            _ => {} // None or RecvBlocked(0|TID_ANY) already handled by sys_notify
        }
    }
    irq_restore(flags);

    // Set force-kill deadline (only if not already set — don't extend)
    unsafe {
        if SIGNAL_DEADLINE[dest] == 0 {
            SIGNAL_DEADLINE[dest] = crate::pit::ticks() + SIGNAL_KILL_TIMEOUT;
        }
    }

    Ok(())
}

/// Check signal deadlines and force-kill unresponsive tasks.
/// Called from `pit::tick()` on every timer interrupt.
pub fn check_signal_deadlines() {
    // Already in interrupt context (IRQ handler), so no need for irq_save.
    let now = crate::pit::ticks();
    unsafe {
        for tid in 2..MAX_TASKS {
            let deadline = SIGNAL_DEADLINE[tid];
            if deadline != 0 && now >= deadline {
                SIGNAL_DEADLINE[tid] = 0;
                let _ = scheduler::kill_task(tid);
            }
        }
    }
}

/// Clear signal deadline for a task (called when task exits or is killed).
pub fn clear_signal_deadline(tid: usize) {
    if tid < MAX_TASKS {
        unsafe {
            SIGNAL_DEADLINE[tid] = 0;
        }
    }
}

/// Synchronous send: blocks until receiver calls recv.
pub fn sys_send(dest: usize, msg: &Message) -> Result<(), IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }
    let sender = scheduler::current_tid();

    let flags = irq_save();
    unsafe {
        // Check if dest is blocked waiting to receive from us (or from ANY)
        let dest_state = TASK_IPC[dest].state;
        match dest_state {
            IpcState::RecvBlocked(from) if from == sender || from == TID_ANY => {
                // Receiver is waiting — deliver directly
                let mut delivered = *msg;
                delivered.sender = sender;
                TASK_IPC[dest].pending_msg = Some(delivered);
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
                irq_restore(flags);
                return Ok(());
            }
            _ => {}
        }

        // Receiver not ready — block sender
        let mut to_send = *msg;
        to_send.sender = sender;
        TASK_IPC[sender].pending_msg = Some(to_send);
        TASK_IPC[sender].state = IpcState::SendBlocked(dest);
        scheduler::block_task(sender);
    }
    irq_restore(flags);
    scheduler::yield_now();

    let flags = irq_save();
    let result = unsafe {
        // Woken. If our message is still queued, nobody took it — the receiver
        // died or we were interrupted by a signal, so report failure rather
        // than pretending the send landed.
        let undelivered = TASK_IPC[sender].pending_msg.take().is_some();
        TASK_IPC[sender].state = IpcState::None;
        if undelivered {
            Err(IpcError::DeadTask)
        } else {
            Ok(())
        }
    };
    irq_restore(flags);
    result
}

/// Synchronous receive: blocks until a message arrives.
/// `from` is the expected sender TID, or TID_ANY for any sender.
pub fn sys_recv(from: usize) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();

    let flags = irq_save();
    unsafe {
        // Check if any sender is blocked waiting to send to us, starting one
        // past the last one served so that no sender can monopolise us.
        for step in 0..MAX_TASKS {
            let tid = (RECV_ROTOR[receiver] + step) % MAX_TASKS;
            if tid == receiver {
                continue;
            }
            let dest = match TASK_IPC[tid].state {
                IpcState::SendBlocked(d) => d,
                IpcState::CallSendBlocked(d) => d,
                _ => continue,
            };
            if dest == receiver && (from == TID_ANY || from == tid) {
                RECV_ROTOR[receiver] = (tid + 1) % MAX_TASKS;
                let was_call = matches!(TASK_IPC[tid].state, IpcState::CallSendBlocked(_));
                let msg = match TASK_IPC[tid].pending_msg.take() {
                    Some(m) => m,
                    None => {
                        // Inconsistent state: reset sender and skip
                        TASK_IPC[tid].state = IpcState::None;
                        scheduler::unblock_task(tid);
                        continue;
                    }
                };
                if was_call {
                    // Transition to CallBlocked — keep blocked, waiting for reply
                    TASK_IPC[tid].state = IpcState::CallBlocked(receiver);
                } else {
                    // Plain send — unblock sender
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Before blocking, check for pending IRQ messages
        // (from=0 means kernel, TID_ANY matches any)
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // A task this one was watching has died.
        if from == 0 || from == TID_ANY {
            if let Some(msg) = take_death(receiver) {
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check for pending notifications (from=0 or TID_ANY)
        if from == 0 || from == TID_ANY {
            let word = TASK_NOTIFY[receiver];
            if word != 0 {
                TASK_NOTIFY[receiver] = 0;
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }

        // No sender ready — block receiver
        TASK_IPC[receiver].state = IpcState::RecvBlocked(from);
        scheduler::block_task(receiver);
    }
    irq_restore(flags);
    scheduler::yield_now();

    let flags = irq_save();
    let result = unsafe {
        // When we wake up, check if an IPC message was delivered first.
        // This must come before IRQ polling — otherwise an IRQ arriving
        // between the IPC delivery and our resume would cause us to
        // return the IRQ message and orphan the IPC message.
        if let Some(msg) = TASK_IPC[receiver].pending_msg.take() {
            TASK_IPC[receiver].state = IpcState::None;
            irq_restore(flags);
            return Ok(msg);
        }

        // No IPC message — check IRQ
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // A task this one was watching has died.
        if from == 0 || from == TID_ANY {
            if let Some(msg) = take_death(receiver) {
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check notification word
        if from == 0 || from == TID_ANY {
            let word = TASK_NOTIFY[receiver];
            if word != 0 {
                TASK_NOTIFY[receiver] = 0;
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }

        // Should not reach here — either IPC, IRQ, or notification should have woken us
        TASK_IPC[receiver].state = IpcState::None;
        Err(IpcError::WouldBlock)
    };
    irq_restore(flags);
    result
}

/// Synchronous RPC: send a message and wait for a reply.
pub fn sys_call(dest: usize, msg: &Message) -> Result<Message, IpcError> {
    call_inner(dest, msg, 0)
}

/// Synchronous call that gives up after `timeout_ticks`.
///
/// A call has two blocking points and a target that is alive but not serving
/// can hang either one: it may never reach sys_recv, leaving us in
/// CallSendBlocked with the message still undelivered, or it may receive and
/// never reply, leaving us in CallBlocked. Both are abandoned on expiry.
///
/// Dropping out of CallBlocked is safe because sys_reply only delivers to a
/// task still in CallBlocked on that replier; a reply that lands after we have
/// given up is discarded rather than written into a caller that moved on.
pub fn sys_call_timeout(
    dest: usize,
    msg: &Message,
    timeout_ticks: u64,
) -> Result<Message, IpcError> {
    call_inner(dest, msg, timeout_ticks)
}

/// `timeout_ticks` of 0 means block indefinitely.
fn call_inner(dest: usize, msg: &Message, timeout_ticks: u64) -> Result<Message, IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    if !scheduler::task_is_live(dest) {
        return Err(IpcError::DeadTask);
    }
    let caller = scheduler::current_tid();

    // Set when the receiver was waiting for this and can take over directly.
    let mut hand_over_to: Option<usize> = None;

    let flags = irq_save();
    unsafe {
        let mut to_send = *msg;
        to_send.sender = caller;

        // Check if dest is recv-blocked
        let dest_state = TASK_IPC[dest].state;
        match dest_state {
            IpcState::RecvBlocked(from) if from == caller || from == TID_ANY => {
                // Fast path: deliver message directly to receiver.
                TASK_IPC[caller].state = IpcState::CallBlocked(dest);
                TASK_IPC[caller].pending_msg = None;
                TASK_IPC[dest].pending_msg = Some(to_send);
                TASK_IPC[dest].state = IpcState::None;
                // Runnable, but deliberately not queued: it is about to be
                // switched to, and an entry left behind is a turn it has
                // already had.
                scheduler::make_ready(dest);
                scheduler::block_task(caller);
                hand_over_to = Some(dest);
            }
            _ => {
                // Slow path: receiver not ready, block as CallSendBlocked.
                TASK_IPC[caller].pending_msg = Some(to_send);
                TASK_IPC[caller].state = IpcState::CallSendBlocked(dest);
                scheduler::block_task(caller);
            }
        };

        TASK_TIMED_OUT[caller] = false;
        TASK_TIMEOUT[caller] = if timeout_ticks == 0 {
            0
        } else {
            crate::pit::ticks() + timeout_ticks
        };
    }
    irq_restore(flags);
    match hand_over_to {
        // Straight across, on what is left of this task's slice.
        Some(dest) => scheduler::donate_to(dest),
        // Nobody was waiting; the message is queued and somebody will come
        // for it. Ordinary scheduling.
        None => scheduler::yield_now(),
    }

    // Reply arrived
    let flags = irq_save();
    let result = unsafe {
        TASK_TIMEOUT[caller] = 0;
        let reply = match TASK_IPC[caller].pending_msg.take() {
            Some(m) => m,
            None => {
                TASK_IPC[caller].state = IpcState::None;
                let timed_out = TASK_TIMED_OUT[caller];
                TASK_TIMED_OUT[caller] = false;
                irq_restore(flags);
                return Err(if timed_out { IpcError::Timeout } else { IpcError::DeadTask });
            }
        };
        TASK_IPC[caller].state = IpcState::None;
        Ok(reply)
    };
    irq_restore(flags);
    result
}

/// Reply to a caller that is blocked in sys_call.
pub fn sys_reply(dest: usize, msg: &Message) -> Result<(), IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let replier = scheduler::current_tid();

    let flags = irq_save();
    let result = unsafe {
        match TASK_IPC[dest].state {
            IpcState::CallBlocked(expected_replier) if expected_replier == replier => {
                let mut reply = *msg;
                reply.sender = replier;
                TASK_IPC[dest].pending_msg = Some(reply);
                TASK_IPC[dest].state = IpcState::None;
                // The caller has been stopped waiting for exactly this. It
                // runs as soon as this server blocks again, rather than after
                // everything else that became ready in the meantime.
                scheduler::unblock_task_next(dest);
                Ok(())
            }
            _ => {
                Err(IpcError::NotWaiting)
            }
        }
    };
    irq_restore(flags);
    result
}

/// Synchronous receive with timeout: blocks until a message arrives or deadline expires.
/// `from` is the expected sender TID, or TID_ANY for any sender.
/// `timeout_ticks` is the number of PIT ticks to wait (0 = non-blocking poll).
pub fn sys_recv_timeout(from: usize, timeout_ticks: u64) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();

    let flags = irq_save();
    unsafe {
        // Check if any sender is blocked waiting to send to us (same as
        // sys_recv, round robin included).
        for step in 0..MAX_TASKS {
            let tid = (RECV_ROTOR[receiver] + step) % MAX_TASKS;
            if tid == receiver {
                continue;
            }
            let dest = match TASK_IPC[tid].state {
                IpcState::SendBlocked(d) => d,
                IpcState::CallSendBlocked(d) => d,
                _ => continue,
            };
            if dest == receiver && (from == TID_ANY || from == tid) {
                RECV_ROTOR[receiver] = (tid + 1) % MAX_TASKS;
                let was_call = matches!(TASK_IPC[tid].state, IpcState::CallSendBlocked(_));
                let msg = match TASK_IPC[tid].pending_msg.take() {
                    Some(m) => m,
                    None => {
                        TASK_IPC[tid].state = IpcState::None;
                        scheduler::unblock_task(tid);
                        continue;
                    }
                };
                if was_call {
                    TASK_IPC[tid].state = IpcState::CallBlocked(receiver);
                } else {
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check for pending IRQ messages
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // A task this one was watching has died.
        if from == 0 || from == TID_ANY {
            if let Some(msg) = take_death(receiver) {
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check for pending notifications
        if from == 0 || from == TID_ANY {
            let word = TASK_NOTIFY[receiver];
            if word != 0 {
                TASK_NOTIFY[receiver] = 0;
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }

        // Non-blocking poll: return immediately if timeout is 0
        if timeout_ticks == 0 {
            irq_restore(flags);
            return Err(IpcError::Timeout);
        }

        // Set deadline and block
        TASK_TIMEOUT[receiver] = crate::pit::ticks() + timeout_ticks;
        TASK_IPC[receiver].state = IpcState::RecvBlocked(from);
        scheduler::block_task(receiver);
    }
    irq_restore(flags);
    scheduler::yield_now();

    let flags = irq_save();
    unsafe {
        // Clear timeout (may already be 0 if expired)
        TASK_TIMEOUT[receiver] = 0;

        // Check if an IPC message was delivered
        if let Some(msg) = TASK_IPC[receiver].pending_msg.take() {
            TASK_IPC[receiver].state = IpcState::None;
            irq_restore(flags);
            return Ok(msg);
        }

        // Check IRQ messages
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // A task this one was watching has died.
        if from == 0 || from == TID_ANY {
            if let Some(msg) = take_death(receiver) {
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(msg);
            }
        }

        // Check notification word
        if from == 0 || from == TID_ANY {
            let word = TASK_NOTIFY[receiver];
            if word != 0 {
                TASK_NOTIFY[receiver] = 0;
                TASK_IPC[receiver].state = IpcState::None;
                irq_restore(flags);
                return Ok(Message {
                    sender: 0,
                    tag: TAG_NOTIFICATION,
                    data: [word, 0, 0, 0, 0, 0],
                });
            }
        }

        // No message — must have been a timeout
        TASK_IPC[receiver].state = IpcState::None;
    }
    irq_restore(flags);
    Err(IpcError::Timeout)
}

/// Kernel-initiated IPC call on behalf of a faulting task.
/// Used by the exception handler to forward page faults to a pager task.
/// The faulting task is blocked until the pager replies via sys_reply.
///
/// Must be called with the faulting task as the current task.
/// After this returns, the pager has replied and the faulting task can resume.
pub fn fault_call(faulting_tid: usize, pager_tid: usize, msg: Message) {
    if faulting_tid >= MAX_TASKS || pager_tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        // Check if pager is recv-blocked waiting for us (or TID_ANY)
        let pager_state = TASK_IPC[pager_tid].state;
        match pager_state {
            IpcState::RecvBlocked(from) if from == faulting_tid || from == TID_ANY => {
                // Fast path: deliver directly to pager.
                // Set CallBlocked BEFORE unblocking pager to prevent race.
                TASK_IPC[faulting_tid].state = IpcState::CallBlocked(pager_tid);
                TASK_IPC[faulting_tid].pending_msg = None;
                TASK_IPC[pager_tid].pending_msg = Some(msg);
                TASK_IPC[pager_tid].state = IpcState::None;
                scheduler::unblock_task(pager_tid);
            }
            _ => {
                // Slow path: pager not waiting — queue as CallSendBlocked.
                // When pager calls sys_recv, it picks this up.
                TASK_IPC[faulting_tid].pending_msg = Some(msg);
                TASK_IPC[faulting_tid].state = IpcState::CallSendBlocked(pager_tid);
            }
        }
        scheduler::block_task(faulting_tid);
    }
    irq_restore(flags);
    scheduler::yield_now();

    // Resumed — pager replied. Clean up.
    let flags = irq_save();
    unsafe {
        TASK_IPC[faulting_tid].pending_msg = None;
        TASK_IPC[faulting_tid].state = IpcState::None;
    }
    irq_restore(flags);
}

/// Check all task timeouts and unblock expired ones.
/// Called from `pit::tick()` on every timer interrupt.
pub fn check_timeouts() {
    // Already in interrupt context (IRQ handler), interrupts are implicitly off.
    let now = crate::pit::ticks();
    unsafe {
        for tid in 0..MAX_TASKS {
            let deadline = TASK_TIMEOUT[tid];
            if deadline != 0 && now >= deadline {
                TASK_TIMEOUT[tid] = 0;
                // Only unblock if still blocked on the thing we timed (it could
                // have been woken by IPC already, between deadline and now).
                match TASK_IPC[tid].state {
                    IpcState::RecvBlocked(_) => {
                        TASK_IPC[tid].state = IpcState::None;
                        scheduler::unblock_task(tid);
                    }
                    IpcState::CallSendBlocked(_) | IpcState::CallBlocked(_) => {
                        // Drop the undelivered message so no receiver can pick
                        // it up after we have stopped waiting for the reply.
                        TASK_IPC[tid].pending_msg = None;
                        TASK_IPC[tid].state = IpcState::None;
                        TASK_TIMED_OUT[tid] = true;
                        scheduler::unblock_task(tid);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Clean up IPC state when a task dies.
/// Unblocks any tasks that were blocked waiting on the dead task.
pub fn cleanup_task_ipc(dead_tid: usize) {
    if dead_tid >= MAX_TASKS {
        return;
    }

    let flags = irq_save();
    unsafe {
        // Withdraw its watches and drop what it never collected. TIDs are
        // recycled, so a registration left behind would fire for whoever
        // lands in the slot next.
        WATCHERS[dead_tid] = 0;
        DEATHS_LEN[dead_tid] = 0;
        let bit = !(1u64 << dead_tid);
        for t in 0..MAX_TASKS {
            WATCHERS[t] &= bit;
        }
        // Clear the dead task's own IPC state, timeout, notifications, and signal deadline
        TASK_IPC[dead_tid].state = IpcState::None;
        TASK_IPC[dead_tid].pending_msg = None;
        TASK_TIMEOUT[dead_tid] = 0;
        TASK_TIMED_OUT[dead_tid] = false;
        TASK_NOTIFY[dead_tid] = 0;
        SIGNAL_DEADLINE[dead_tid] = 0;

        // Scan all tasks for those blocked on the dead task
        let error_msg = Message {
            sender: dead_tid,
            tag: u64::MAX,
            data: [0; 6],
        };

        for tid in 0..MAX_TASKS {
            if tid == dead_tid {
                continue;
            }
            match TASK_IPC[tid].state {
                IpcState::SendBlocked(dest) if dest == dead_tid => {
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                IpcState::CallSendBlocked(dest) if dest == dead_tid => {
                    TASK_IPC[tid].pending_msg = Some(error_msg);
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                IpcState::CallBlocked(dest) if dest == dead_tid => {
                    TASK_IPC[tid].pending_msg = Some(error_msg);
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                IpcState::RecvBlocked(from) if from == dead_tid => {
                    TASK_IPC[tid].pending_msg = Some(error_msg);
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                _ => {}
            }
        }
    }
    irq_restore(flags);
}
