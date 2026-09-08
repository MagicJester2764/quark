#![no_std]
#![no_main]

//! Two tasks in one address space.
//!
//! Threads share memory by construction here, so the check that matters is
//! that a write by one is seen by the other, and that the address space
//! survives a thread exiting before its parent.

use core::sync::atomic::{AtomicU32, Ordering};
use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall, thread};

// A task to run the thread in, and pages for its stack.
quark_rt::manifest!([
    CapReq::task_mgmt(0),
    CapReq::phys_alloc(16),
]);

/// Shared with the thread: same address space, so simply a static.
static COUNTER: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);

/// Reads the FS-relative word the thread set up, which is what a thread-local
/// compiles down to. Two threads with different FS bases see different values
/// from the identical instruction.
fn read_fs_word() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mov {}, fs:[0]", out(reg) v, options(nostack, readonly)) };
    v
}

/// Per-thread storage. Same address in both threads; FS is what separates them.
static mut MAIN_TLS: u64 = 0;
static mut WORKER_TLS: u64 = 0;

extern "C" fn arg_worker(arg: usize) -> ! {
    // FS base of our own, so fs:[0] reads WORKER_TLS rather than MAIN_TLS.
    unsafe {
        WORKER_TLS = 0xBBBB_BBBB;
        let _ = syscall::sys_set_fs_base(core::ptr::addr_of!(WORKER_TLS) as usize);
    }
    ARG_SEEN.store(arg as u32, Ordering::SeqCst);
    TLS_SEEN.store(read_fs_word() as u32, Ordering::SeqCst);
    syscall::sys_exit_code(0);
}

static ARG_SEEN: AtomicU32 = AtomicU32::new(0);
static TLS_SEEN: AtomicU32 = AtomicU32::new(0);

extern "C" fn worker() -> ! {
    for _ in 0..1000 {
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }
    DONE.store(1, Ordering::SeqCst);
    syscall::sys_exit_code(7);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("threadtest: main tid {}", syscall::sys_getpid());

    let t = match thread::spawn(worker, 0) {
        Ok(t) => t,
        Err(()) => {
            println!("  spawn -> FAILED");
            syscall::sys_exit_code(1);
        }
    };
    println!("  spawned thread tid {}", t.tid());

    // Both add to the same counter. No lock: SeqCst on one word is enough to
    // show the memory is genuinely shared, which is the point being tested.
    for _ in 0..1000 {
        COUNTER.fetch_add(1, Ordering::SeqCst);
    }

    let status = t.join();
    println!("  joined, exit status {}", status);
    println!("  worker reached the end: {}", DONE.load(Ordering::SeqCst) == 1);
    println!("  counter = {} (expected 2000)", COUNTER.load(Ordering::SeqCst));

    // If the address space had been destroyed when the thread exited, this
    // would already have faulted rather than printed.
    println!("threadtest: address space survived the thread");

    // A thread's entry argument, and a per-thread FS base.
    unsafe {
        MAIN_TLS = 0xAAAA_AAAA;
        let _ = syscall::sys_set_fs_base(core::ptr::addr_of!(MAIN_TLS) as usize);
    }
    match thread::spawn_with_arg(arg_worker, 0x1234_5678, 1, 4) {
        Ok(t) => {
            t.join();
            println!("  entry argument   = 0x{:x} (expected 0x12345678)",
                     ARG_SEEN.load(Ordering::SeqCst));
            println!("  worker fs:[0]    = 0x{:x} (its own)",
                     TLS_SEEN.load(Ordering::SeqCst));
            println!("  main   fs:[0]    = 0x{:x} (unchanged by the thread)",
                     read_fs_word());
        }
        Err(()) => println!("  spawn_with_arg -> FAILED"),
    }

    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
