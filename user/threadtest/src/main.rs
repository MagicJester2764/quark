#![no_std]
#![no_main]
#![feature(thread_local)]

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

/// A real thread-local: the compiler resolves this as an offset from FS, so
/// each thread reads its own copy without anything at the use site saying so.
#[thread_local]
static mut COLOUR: u32 = 0xDEAD;

/// Storage for each thread's copy. Static rather than allocated because there
/// is no allocator here; one region per thread, never shared.
static mut TLS_MAIN: [u8; 512] = [0; 512];
static mut TLS_WORKER: [u8; 512] = [0; 512];

extern "C" fn arg_worker(arg: usize) -> ! {
    // Proper thread-local storage of our own, laid out from the PT_TLS image.
    unsafe {
        let r = core::ptr::addr_of_mut!(TLS_WORKER) as *mut u8;
        let _ = quark_rt::tls::init_in(r, 512);
        COLOUR = 0xBBBB;          // writes this thread's copy
        TLS_SEEN.store(COLOUR, Ordering::SeqCst);
    }
    ARG_SEEN.store(arg as u32, Ordering::SeqCst);
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

    let t = match thread::spawn(worker) {
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

    // A thread's entry argument, and a genuine #[thread_local].
    unsafe {
        let r = core::ptr::addr_of_mut!(TLS_MAIN) as *mut u8;
        if quark_rt::tls::init_in(r, 512).is_err() {
            println!("  tls init -> FAILED");
        }
        COLOUR = 0xAAAA;
        println!("  tls template     = {} bytes", quark_rt::tls::template_size());
    }
    match thread::spawn_with_arg(arg_worker, 0x1234_5678, 4) {
        Ok(t) => {
            t.join();
            println!("  entry argument   = 0x{:x} (expected 0x12345678)",
                     ARG_SEEN.load(Ordering::SeqCst));
            println!("  worker COLOUR    = 0x{:x} (its own copy)",
                     TLS_SEEN.load(Ordering::SeqCst));
            println!("  main   COLOUR    = 0x{:x} (unchanged by the thread)",
                     unsafe { COLOUR });
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
