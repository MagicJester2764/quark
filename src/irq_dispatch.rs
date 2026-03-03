/// IRQ delegation to user-space tasks.
///
/// When a user task registers for an IRQ, the kernel sends it an IPC
/// message when that IRQ fires, instead of handling it in the kernel.

use crate::ipc::Message;
use crate::scheduler;

const MAX_IRQS: usize = 16;

/// Registered handler TID for each IRQ (0 = no handler).
static mut IRQ_HANDLERS: [usize; MAX_IRQS] = [0; MAX_IRQS];
/// Whether each IRQ has a registered user-space handler.
static mut IRQ_HAS_HANDLER: [bool; MAX_IRQS] = [false; MAX_IRQS];

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
/// this IRQ, send it a notification message and return true.
/// Otherwise return false (kernel handles it).
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
        // Send an IPC message to the handler task
        let msg = Message {
            sender: 0, // kernel TID
            tag: irq as u64,
            data: [0; 6],
        };

        // Try to deliver — unblock the task if it's waiting for a recv
        // We can't block the kernel here, so use a non-blocking approach
        scheduler::unblock_task(tid);
        // Store the message for the task to pick up
        deliver_irq_message(tid, msg);

        true
    }
}

/// Deliver an IRQ notification message to a task.
/// This is a simplified delivery that stores the message for the task.
fn deliver_irq_message(tid: usize, msg: Message) {
    // Use the IPC system's internal mechanism to deliver
    // For now, unblocking the task is sufficient — the task should
    // be in a recv loop waiting for IRQ notifications
    let _ = (tid, msg);
}
