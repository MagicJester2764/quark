/// Synchronous IPC (message passing) for the Quark microkernel.
///
/// Tasks communicate by sending/receiving fixed-size messages.
/// Messages fit in registers for zero-copy small transfers.

use crate::scheduler;

pub const TID_ANY: usize = usize::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcError {
    InvalidTid,
    DeadTask,
    WouldBlock,
    NotWaiting,
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

/// Synchronous send: blocks until receiver calls recv.
pub fn sys_send(dest: usize, msg: &Message) -> Result<(), IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let sender = scheduler::current_tid();

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
        scheduler::yield_now();

        // When we wake up, send was completed
        TASK_IPC[sender].state = IpcState::None;
        Ok(())
    }
}

/// Synchronous receive: blocks until a message arrives.
/// `from` is the expected sender TID, or TID_ANY for any sender.
pub fn sys_recv(from: usize) -> Result<Message, IpcError> {
    let receiver = scheduler::current_tid();

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
                let msg = TASK_IPC[tid].pending_msg.take().unwrap();
                if was_call {
                    // Transition to CallBlocked — keep blocked, waiting for reply
                    TASK_IPC[tid].state = IpcState::CallBlocked(receiver);
                } else {
                    // Plain send — unblock sender
                    TASK_IPC[tid].state = IpcState::None;
                    scheduler::unblock_task(tid);
                }
                return Ok(msg);
            }
        }

        // Before blocking, check for pending IRQ messages
        // (from=0 means kernel, TID_ANY matches any)
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                return Ok(msg);
            }
        }

        // No sender ready — block receiver
        TASK_IPC[receiver].state = IpcState::RecvBlocked(from);
        scheduler::block_task(receiver);
        scheduler::yield_now();

        // When we wake up, check IRQ messages first (may have been unblocked by IRQ)
        if from == 0 || from == TID_ANY {
            if let Some(msg) = crate::irq_dispatch::poll_irq_message(receiver) {
                TASK_IPC[receiver].state = IpcState::None;
                return Ok(msg);
            }
        }

        // When we wake up, message was delivered to our pending slot
        let msg = TASK_IPC[receiver].pending_msg.take().unwrap();
        TASK_IPC[receiver].state = IpcState::None;
        Ok(msg)
    }
}

/// Synchronous RPC: send a message and wait for a reply.
pub fn sys_call(dest: usize, msg: &Message) -> Result<Message, IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let caller = scheduler::current_tid();

    unsafe {
        let mut to_send = *msg;
        to_send.sender = caller;

        // Check if dest is recv-blocked
        let dest_state = TASK_IPC[dest].state;
        let need_wait = match dest_state {
            IpcState::RecvBlocked(from) if from == caller || from == TID_ANY => {
                // Fast path: deliver message directly to receiver
                TASK_IPC[dest].pending_msg = Some(to_send);
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
                true // Receiver hasn't processed yet, need to wait for reply
            }
            _ => {
                // Slow path: receiver not ready, block as CallSendBlocked.
                // When sys_recv picks this up, it will transition us to
                // CallBlocked (keeping us blocked). sys_reply then delivers
                // the reply and unblocks us — we resume here with reply ready.
                TASK_IPC[caller].pending_msg = Some(to_send);
                TASK_IPC[caller].state = IpcState::CallSendBlocked(dest);
                scheduler::block_task(caller);
                scheduler::yield_now();
                false // Reply already delivered by the time we resume
            }
        };

        if need_wait {
            // Wait for reply (fast path only — receiver was unblocked but
            // hasn't replied yet; interrupts are off so no race here)
            TASK_IPC[caller].state = IpcState::CallBlocked(dest);
            TASK_IPC[caller].pending_msg = None;
            scheduler::block_task(caller);
            scheduler::yield_now();
        }

        // Reply arrived
        let reply = TASK_IPC[caller].pending_msg.take().unwrap();
        TASK_IPC[caller].state = IpcState::None;
        Ok(reply)
    }
}

/// Reply to a caller that is blocked in sys_call.
pub fn sys_reply(dest: usize, msg: &Message) -> Result<(), IpcError> {
    if dest >= MAX_TASKS {
        return Err(IpcError::InvalidTid);
    }
    let replier = scheduler::current_tid();

    unsafe {
        match TASK_IPC[dest].state {
            IpcState::CallBlocked(expected_replier) if expected_replier == replier => {
                let mut reply = *msg;
                reply.sender = replier;
                TASK_IPC[dest].pending_msg = Some(reply);
                TASK_IPC[dest].state = IpcState::None;
                scheduler::unblock_task(dest);
                Ok(())
            }
            _ => Err(IpcError::NotWaiting),
        }
    }
}
