/// IRQ delegation to user-space tasks.
///
/// When a user task registers for an IRQ, the kernel enqueues a message
/// into a per-IRQ ring buffer and unblocks the handler task.

use crate::ipc::Message;
use crate::scheduler;
use crate::sync::IrqSpinLock;

/// The sixteen ISA interrupts, and after them the ones a device sends for
/// itself as a message (MSI): numbers the kernel gives out, one driver each.
const MAX_IRQS: usize = 48;
/// The first of the second kind.
pub const FIRST_MESSAGE: usize = 16;
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

/// Register a user-space task to handle one of the sixteen ISA interrupts.
pub fn register_irq_handler(irq: u8, tid: usize) {
    if (irq as usize) < FIRST_MESSAGE {
        let mut state = IRQ_STATE.lock();
        state.handlers[irq as usize] = tid;
        state.has_handler[irq as usize] = true;
    }
}

/// Give `tid` an interrupt number of its own, for a device to send as a
/// message: the lowest nobody has. `None` when all thirty-two are taken.
///
/// It is the task's until the task is gone (`unregister_task_irqs`). A
/// device that goes on sending after that is told to nobody, and one given
/// the number later hears a stray: a driver looks at its device to see
/// whether it has anything to say, as it does for a line it shares.
pub fn allocate_message(tid: usize) -> Option<u8> {
    let mut state = IRQ_STATE.lock();
    let n = (FIRST_MESSAGE..MAX_IRQS).find(|&n| !state.has_handler[n])?;
    state.handlers[n] = tid;
    state.has_handler[n] = true;
    state.rings[n] = IrqRing::new();
    Some(n as u8)
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

    let queued = state.rings[idx].push(msg);
    // Drop lock before calling scheduler (avoids potential ordering issues)
    drop(state);
    if queued {
        crate::intc::held(irq);
    } else {
        // The ring was full, so this notification is gone — and with it the
        // `sys_irq_ack` that would have answered it. An interrupt nobody is
        // told about is one an 8259 never hears the end of: its in-service
        // bit stays set and the line delivers nothing again, ever. A
        // keyboard that stops mid-sentence and never comes back is what
        // that looks like.
        //
        // Losing the notification is survivable — a driver that drains its
        // device collects the work on the next one. Losing the end of the
        // interrupt is not, so it is said here.
        crate::intc::dropped(irq);
    }
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
            // Nobody is left to deal with the device, and one that holds
            // its line until it is answered would interrupt for ever. The
            // clock and the keyboard are the kernel's when they are
            // nobody's, and stay on.
            if irq > 1 && irq < FIRST_MESSAGE {
                crate::intc::disable(irq as u8);
            }
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
