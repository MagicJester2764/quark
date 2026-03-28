#![allow(unsafe_op_in_unsafe_fn)]

pub mod os;

pub use quark_rt::rt::futex;

use crate::io;

pub(crate) fn map_quark_error(err: i32) -> io::Error {
    io::Error::from_raw_os_error(err)
}

#[cfg(not(test))]
#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Initialize the runtime.
    quark_rt::rt::init();

    // Call main.
    unsafe extern "C" {
        fn main(_: isize, _: *const *const u8, _: u8) -> i32;
    }
    let result = unsafe { main(0, core::ptr::null(), 0) };

    // Terminate the process.
    quark_rt::rt::exit(result)
}

// SAFETY: must be called only once during runtime initialization.
// NOTE: Quark uses quark_rt::rt::init() to initialize runtime (see above).
pub unsafe fn init(_argc: isize, _argv: *const *const u8, _sigpipe: u8) {}

// SAFETY: must be called only once during runtime cleanup.
pub unsafe fn cleanup() {}

pub fn unsupported<T>() -> io::Result<T> {
    Err(unsupported_err())
}

pub fn unsupported_err() -> io::Error {
    io::Error::UNSUPPORTED_PLATFORM
}

pub fn abort_internal() -> ! {
    core::intrinsics::abort();
}
