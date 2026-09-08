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
/// Thread-local storage for the main thread.
///
/// Static rather than allocated because this runs before the allocator has
/// been touched, and because the main thread's storage has to outlive
/// everything. Programs needing more than this declare it and find out at
/// startup rather than corrupting memory later — see the check below.
const MAIN_TLS_BYTES: usize = 4096;
static mut MAIN_TLS: [u8; MAIN_TLS_BYTES] = [0; MAIN_TLS_BYTES];

pub fn init() {
    // Set up thread-locals before anything else: std reaches for them early,
    // and reading one through an unset FS base faults.
    unsafe {
        let need = crate::tls::required_bytes();
        if need > MAIN_TLS_BYTES {
            // Nothing useful can run without its thread-locals, and carrying
            // on would write past this buffer.
            crate::syscall::sys_write(b"quark-rt: TLS template exceeds the main thread's buffer\n");
            crate::syscall::sys_exit_code(1);
        }
        let region = core::ptr::addr_of_mut!(MAIN_TLS) as *mut u8;
        if crate::tls::init_in(region, MAIN_TLS_BYTES).is_err() {
            crate::syscall::sys_write(b"quark-rt: failed to set up thread-local storage\n");
            crate::syscall::sys_exit_code(1);
        }
    }
}

// ---- Process lifecycle ----

pub fn exit(code: i32) -> ! {
    syscall::sys_exit_code(code);
}

pub fn abort() -> ! {
    syscall::sys_exit_code(-1);
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
        // stdout/stderr fall back to the raw kernel console so panics and
        // early boot output are never lost. This is reported as an error
        // anyway: claiming success here hid broken pipes and dead console
        // servers from every std::io caller.
        if fd == FD_STDOUT || fd == FD_STDERR {
            syscall::sys_write(buf);
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

    /// Wait on `futex` while it still holds `expected`.
    ///
    /// std's contract: return `false` if the wait timed out, `true` otherwise.
    /// This used to ignore `timeout` and always return `true`, so
    /// `Condvar::wait_timeout` and `thread::park_timeout` blocked forever
    /// instead of expiring.
    pub fn futex_wait(
        futex: &AtomicU32,
        expected: u32,
        timeout: Option<core::time::Duration>,
    ) -> bool {
        use core::sync::atomic::Ordering;

        let ptr = futex as *const AtomicU32 as *const u32;

        let Some(timeout) = timeout else {
            syscall::sys_futex_wait(ptr, expected);
            return true;
        };

        // This used to poll the word against the tick counter and yield, for
        // want of a timed wait in the kernel — which burned a core for the
        // length of the wait and could not see a wake before the next poll.
        let r = syscall::sys_futex_wait_timeout(ptr, expected, ticks_for(timeout));
        if r != syscall::FUTEX_TIMED_OUT {
            return true;
        }

        // Timed out. Report a wake anyway if the word moved in the meantime:
        // std reads `false` as "the deadline passed and nothing happened", and
        // a change that landed either side of the deadline did happen.
        futex.load(Ordering::Relaxed) != expected
    }

    /// Convert a duration to PIT ticks (100 Hz), rounding up so a sub-tick
    /// timeout still waits at least one tick.
    fn ticks_for(d: core::time::Duration) -> u64 {
        let ms = d.as_millis().min(u64::MAX as u128) as u64;
        ms.div_ceil(10).max(1)
    }

    /// Wake one waiter. Returns true if a task was actually woken.
    pub fn futex_wake(futex: &AtomicU32) -> bool {
        let ptr = futex as *const AtomicU32 as *const u32;
        syscall::sys_futex_wake(ptr, 1) > 0
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
