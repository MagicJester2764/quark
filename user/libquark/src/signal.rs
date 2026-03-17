/// Signal handling for Quark userspace.
///
/// Signals are delivered as notification badges (high bits) via the kernel's
/// async notification system. Tasks receive them as `TAG_NOTIFICATION` messages
/// via `sys_recv`. Use `extract_signal()` to check if a received notification
/// contains signal bits, then handle gracefully before the kernel's 2-second
/// force-kill deadline expires.

use crate::ipc::Message;
use crate::syscall;

pub use crate::syscall::{SIG_INT, SIG_KILL, SIG_MASK, SIG_TERM};

/// Check if a notification message contains a signal.
/// Returns the signal bits (nonzero if signal present).
///
/// This is the preferred way to detect signals: check every `TAG_NOTIFICATION`
/// message in your event loop via `extract_signal(&msg)`.
pub fn extract_signal(msg: &Message) -> u64 {
    if msg.tag == syscall::TAG_NOTIFICATION {
        msg.data[0] & SIG_MASK
    } else {
        0
    }
}

/// Default signal handler: exit the process.
/// Call this when you receive a signal and don't need custom cleanup.
pub fn default_handler(_sig: u64) -> ! {
    syscall::sys_exit();
}
