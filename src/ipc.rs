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

/// Per-task notification word (seL4-style). Bits are OR'd in by sys_notify().
/// Atomically read-and-cleared when consumed by sys_recv/sys_recv_timeout.
static mut TASK_NOTIFY: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Per-task signal kill deadline (PIT tick). 0 = no pending signal deadline.
/// When nonzero, the task will be force-killed after the deadline expires.
static mut SIGNAL_DEADLINE: [u64; MAX_TASKS] = [0; MAX_TASKS];

/// Tag for notification messages delivered to user space.
pub const TAG_NOTIFICATION: u64 = 0xFFFF_0002;

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
pub fn sys_notify(dest: usize, badge: u64) -> Result<(), IpcError> {
    if dest >= MAX_TASKS || badge == 0 {
        return Err(IpcError::InvalidTid);
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

    // Deliver signal bits via notification word and wake if RecvBlocked(0|TID_ANY)
    sys_notify(dest, sig)?;

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

    unsafe {
        // When we wake up, send was completed
        TASK_IPC[sender].state = IpcState::None;
    }
    Ok(())
}

/// Synchronous receive: blocks until a message arrives.
/// `from` is the expected sender TID, or TID_ANY for any sender.
pub fn sys_recv(from: usize) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();

    let flags = irq_save();
    unsafe {
        // Check if any sender is blocked waiting to send to us
        for tid in 0..MAX_TASKS {
            if tid == receiver {
                continue;
            }
            let dest = match TASK_IPC[tid].state {
                IpcState::SendBlocked(d) => d,
                IpcState::CallSendBlocked(d) => d,
                _ => continue,
            };
            if dest == receiver && (from == TID_ANY || from == tid) {
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
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let caller = scheduler::current_tid();

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
                scheduler::unblock_task(dest);
                scheduler::block_task(caller);
            }
            _ => {
                // Slow path: receiver not ready, block as CallSendBlocked.
                TASK_IPC[caller].pending_msg = Some(to_send);
                TASK_IPC[caller].state = IpcState::CallSendBlocked(dest);
                scheduler::block_task(caller);
            }
        };
    }
    irq_restore(flags);
    scheduler::yield_now();

    // Reply arrived
    let flags = irq_save();
    let result = unsafe {
        let reply = match TASK_IPC[caller].pending_msg.take() {
            Some(m) => m,
            None => {
                TASK_IPC[caller].state = IpcState::None;
                irq_restore(flags);
                return Err(IpcError::DeadTask);
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
                scheduler::unblock_task(dest);
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
        // Check if any sender is blocked waiting to send to us (same as sys_recv)
        for tid in 0..MAX_TASKS {
            if tid == receiver {
                continue;
            }
            let dest = match TASK_IPC[tid].state {
                IpcState::SendBlocked(d) => d,
                IpcState::CallSendBlocked(d) => d,
                _ => continue,
            };
            if dest == receiver && (from == TID_ANY || from == tid) {
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
                // Only unblock if still RecvBlocked (could have been woken by IPC already)
                if matches!(TASK_IPC[tid].state, IpcState::RecvBlocked(_)) {
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
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
        // Clear the dead task's own IPC state, timeout, notifications, and signal deadline
        TASK_IPC[dead_tid].state = IpcState::None;
        TASK_IPC[dead_tid].pending_msg = None;
        TASK_TIMEOUT[dead_tid] = 0;
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
