#![no_std]

#[cfg(not(feature = "rustc-dep-of-std"))]
extern crate alloc;

pub mod args;
pub mod console;
pub mod ipc;
pub mod signal;
pub mod stdio;
pub mod sync;
pub mod syscall;
pub mod net;
pub mod passwd;
pub mod vfs;

pub mod allocator;

/// Minimal libc compatibility shim for std's os::fd module.
pub mod libc;

/// Flat runtime API for the std PAL to call into.
/// When building as part of std (rustc-dep-of-std), these are the
/// entry points that library/std/src/sys/pal/quark uses.
pub mod rt;

#[cfg(not(feature = "rustc-dep-of-std"))]
use allocator::QuarkAllocator;

#[cfg(not(feature = "rustc-dep-of-std"))]
#[global_allocator]
static ALLOCATOR: QuarkAllocator = QuarkAllocator::new();
