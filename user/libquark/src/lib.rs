#![no_std]

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

use allocator::QuarkAllocator;

#[global_allocator]
static ALLOCATOR: QuarkAllocator = QuarkAllocator::new();
