/// IRQ delegation to user-space tasks.
///
/// When a user task registers for an IRQ, the kernel enqueues a message
/// into a per-IRQ ring buffer and unblocks the handler task.

use crate::ipc::Message;
use crate::scheduler;
use crate::sync::IrqSpinLock;

const MAX_IRQS: usize = 16;
const IRQ_RING_SIZE: usize = 8;

/// Per-IRQ ring buffer for pending messages.
struct IrqRing {
    buf: [Message; IRQ_RING_SIZE],
    head: usize,
    tail: usize,
    count: usize,
}

impl IrqRing {
    const fn new() -> Self {
        IrqRing {
            buf: [Message::empty(); IRQ_RING_SIZE],
            head: 0,
            tail: 0,
            count: 0,
        }
    }

    fn push(&mut self, msg: Message) -> bool {
        if self.count >= IRQ_RING_SIZE {
            return false; // full, drop
        }
        self.buf[self.tail] = msg;
        self.tail = (self.tail + 1) % IRQ_RING_SIZE;
        self.count += 1;
        true
    }

    fn pop(&mut self) -> Option<Message> {
        if self.count == 0 {
            return None;
        }
        let msg = self.buf[self.head];
        self.head = (self.head + 1) % IRQ_RING_SIZE;
        self.count -= 1;
        Some(msg)
    }
}

struct IrqDispatchState {
    handlers: [usize; MAX_IRQS],
    has_handler: [bool; MAX_IRQS],
    rings: [IrqRing; MAX_IRQS],
}

static IRQ_STATE: IrqSpinLock<IrqDispatchState> = IrqSpinLock::new(IrqDispatchState {
    handlers: [0; MAX_IRQS],
    has_handler: [false; MAX_IRQS],
    rings: {
        const INIT: IrqRing = IrqRing::new();
        [INIT; MAX_IRQS]
    },
});

/// Register a user-space task to handle an IRQ.
pub fn register_irq_handler(irq: u8, tid: usize) {
    if (irq as usize) < MAX_IRQS {
        let mut state = IRQ_STATE.lock();
        state.handlers[irq as usize] = tid;
        state.has_handler[irq as usize] = true;
    }
}

/// Called from the kernel IRQ handler. If a user task is registered for
/// this IRQ, enqueue a notification message and unblock it. Returns true
/// if handled by a user task.
pub fn dispatch_irq(irq: u8) -> bool {
    let idx = irq as usize;
    if idx >= MAX_IRQS {
        return false;
    }

    let mut state = IRQ_STATE.lock();
    if !state.has_handler[idx] {
        return false;
    }

    let tid = state.handlers[idx];
    let msg = Message {
        sender: 0, // kernel TID
        tag: irq as u64,
        data: [0; 6],
    };

    state.rings[idx].push(msg);
    // Drop lock before calling scheduler (avoids potential ordering issues)
    drop(state);
    scheduler::unblock_task(tid);

    true
}

/// Unregister all IRQ handlers for a dead task and drain its ring buffers.
pub fn unregister_task_irqs(tid: usize) {
    let mut state = IRQ_STATE.lock();
    for irq in 0..MAX_IRQS {
        if state.has_handler[irq] && state.handlers[irq] == tid {
            state.has_handler[irq] = false;
            state.handlers[irq] = 0;
            state.rings[irq] = IrqRing::new();
        }
    }
}

/// Poll for a pending IRQ message destined for a given task.
/// Checks all IRQ ring buffers registered to this TID.
pub fn poll_irq_message(tid: usize) -> Option<Message> {
    let mut state = IRQ_STATE.lock();
    for irq in 0..MAX_IRQS {
        if state.has_handler[irq] && state.handlers[irq] == tid {
            if let Some(msg) = state.rings[irq].pop() {
                return Some(msg);
            }
        }
    }
    None
}
