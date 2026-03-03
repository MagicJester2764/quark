/// IRQ delegation to user-space tasks.
///
/// When a user task registers for an IRQ, the kernel enqueues a message
/// into a per-IRQ ring buffer and unblocks the handler task.

use crate::ipc::Message;
use crate::scheduler;

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

/// Registered handler TID for each IRQ (0 = no handler).
static mut IRQ_HANDLERS: [usize; MAX_IRQS] = [0; MAX_IRQS];
/// Whether each IRQ has a registered user-space handler.
static mut IRQ_HAS_HANDLER: [bool; MAX_IRQS] = [false; MAX_IRQS];
/// Per-IRQ pending message ring buffers.
static mut IRQ_RINGS: [IrqRing; MAX_IRQS] = {
    const INIT: IrqRing = IrqRing::new();
    [INIT; MAX_IRQS]
};

/// Register a user-space task to handle an IRQ.
pub fn register_irq_handler(irq: u8, tid: usize) {
    if (irq as usize) < MAX_IRQS {
        unsafe {
            IRQ_HANDLERS[irq as usize] = tid;
            IRQ_HAS_HANDLER[irq as usize] = true;
        }
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

    unsafe {
        if !IRQ_HAS_HANDLER[idx] {
            return false;
        }

        let tid = IRQ_HANDLERS[idx];
        let msg = Message {
            sender: 0, // kernel TID
            tag: irq as u64,
            data: [0; 6],
        };

        IRQ_RINGS[idx].push(msg);
        scheduler::unblock_task(tid);

        true
    }
}

/// Poll for a pending IRQ message destined for a given task.
/// Checks all IRQ ring buffers registered to this TID.
pub fn poll_irq_message(tid: usize) -> Option<Message> {
    unsafe {
        for irq in 0..MAX_IRQS {
            if IRQ_HAS_HANDLER[irq] && IRQ_HANDLERS[irq] == tid {
                if let Some(msg) = IRQ_RINGS[irq].pop() {
                    return Some(msg);
                }
            }
        }
    }
    None
}
