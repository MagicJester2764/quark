/// Minimal libc compatibility shim for std's os::fd module.
/// Provides only the symbols std needs to compile (STDIN/OUT/ERR_FILENO, close).

pub const STDIN_FILENO: i32 = 0;
pub const STDOUT_FILENO: i32 = 1;
pub const STDERR_FILENO: i32 = 2;

pub unsafe fn close(_fd: i32) -> i32 {
    // Quark doesn't have a close syscall yet; no-op.
    0
}
