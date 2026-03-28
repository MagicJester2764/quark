/// Flat runtime API for the Rust std PAL.
///
/// These functions are called by library/std/src/sys/ modules.
/// They provide a stable, simple interface between std and Quark's
/// syscall/IPC layer, following the moto-rt pattern from Motor OS.

use crate::syscall;

// ---- Error codes ----

pub const E_OK: i32 = 0;
pub const E_NOT_FOUND: i32 = 1;
pub const E_PERMISSION: i32 = 2;
pub const E_INVALID: i32 = 3;
pub const E_EXISTS: i32 = 4;
pub const E_IO: i32 = 5;
pub const E_NOT_DIR: i32 = 6;
pub const E_IS_DIR: i32 = 7;
pub const E_WOULD_BLOCK: i32 = 8;
pub const E_UNSUPPORTED: i32 = 9;

// ---- Well-known file descriptors ----

pub const FD_STDIN: usize = 0;
pub const FD_STDOUT: usize = 1;
pub const FD_STDERR: usize = 2;

// ---- Runtime initialization ----

/// Initialize the Quark runtime. Called once from _start before main.
pub fn init() {
    // Currently a no-op; the allocator self-initializes on first alloc.
    // Future: pre-discover VFS/net service TIDs, set up TLS, etc.
}

// ---- Process lifecycle ----

pub fn exit(code: i32) -> ! {
    // Quark's sys_exit doesn't take a code yet; just exit.
    let _ = code;
    syscall::sys_exit();
}

pub fn abort() -> ! {
    syscall::sys_exit();
}

// ---- I/O (file descriptor read/write) ----

pub fn fd_read(fd: usize, buf: &mut [u8]) -> Result<usize, i32> {
    let ret = syscall::sys_fd_read(fd, buf);
    if ret == u64::MAX {
        Err(E_IO)
    } else {
        Ok(ret as usize)
    }
}

pub fn fd_write(fd: usize, buf: &[u8]) -> Result<usize, i32> {
    let ret = syscall::sys_fd_write(fd, buf);
    if ret == u64::MAX {
        // Fall back to kernel console for stdout/stderr
        if fd == FD_STDOUT || fd == FD_STDERR {
            syscall::sys_write(buf);
            return Ok(buf.len());
        }
        Err(E_IO)
    } else {
        Ok(ret as usize)
    }
}

// ---- Futex (for std sync primitives) ----

pub mod futex {
    use crate::syscall;
    use core::sync::atomic::AtomicU32;

    /// An atomic for use as a futex that is at least 32-bits but may be larger.
    pub type Futex = AtomicU32;
    /// Must be the underlying type of Futex.
    pub type Primitive = u32;

    /// An atomic for use as a futex that is at least 8-bits but may be larger.
    pub type SmallFutex = AtomicU32;
    /// Must be the underlying type of SmallFutex.
    pub type SmallPrimitive = u32;

    pub fn futex_wait(futex: &AtomicU32, expected: u32, _timeout: Option<core::time::Duration>) -> bool {
        let ptr = futex as *const AtomicU32 as *const u32;
        syscall::sys_futex_wait(ptr, expected);
        // Quark futex doesn't return a meaningful error for "wrong value",
        // it just returns immediately. Return true (woken).
        true
    }

    pub fn futex_wake(futex: &AtomicU32) -> bool {
        let ptr = futex as *const AtomicU32 as *const u32;
        syscall::sys_futex_wake(ptr, 1);
        true
    }

    pub fn futex_wake_all(futex: &AtomicU32) {
        let ptr = futex as *const AtomicU32 as *const u32;
        syscall::sys_futex_wake(ptr, u32::MAX as usize);
    }
}

// ---- Time ----

/// Returns the kernel PIT tick count (100 Hz, 10 ms per tick).
pub fn ticks() -> u64 {
    syscall::sys_ticks()
}

/// Sleep for approximately `ms` milliseconds.
pub fn sleep_ms(ms: u64) {
    syscall::sleep_ms(ms);
}

// ---- Arguments ----

pub fn argc() -> usize {
    crate::args::argc()
}

pub fn argv(index: usize) -> Option<&'static [u8]> {
    crate::args::argv(index)
}

// ---- Memory (for std GlobalAlloc) ----

pub fn mmap(vaddr: usize, pages: usize) -> Result<(), i32> {
    syscall::sys_mmap(vaddr, pages).map_err(|_| E_IO)
}
