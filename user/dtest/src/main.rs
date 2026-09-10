#![no_std]
#![no_main]

//! Phase 10 acceptance: descriptors, streams, waiting and the environment.
//!
//! There is no test framework here, so this is one: a program that asserts and
//! exits non-zero. Run it from the shell, or read its output on the serial
//! line. Each section corresponds to one task of the Phase 10 plan.

use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall};

quark_rt::manifest!([CapReq::task_mgmt(0), CapReq::phys_alloc(64)]);

static mut PASSED: u32 = 0;
static mut FAILED: u32 = 0;

fn check(what: &str, ok: bool) {
    unsafe {
        if ok {
            PASSED += 1;
            println!("  ok    {}", what);
        } else {
            FAILED += 1;
            println!("  FAIL  {}", what);
        }
    }
}

/// A pipe wired to two of our own descriptors, for tests that need a
/// descriptor that behaves like something.
fn own_pipe(read_fd: usize, write_fd: usize) -> Result<(), ()> {
    let me = syscall::sys_getpid() as usize;
    let h = syscall::sys_pipe_create()?;
    syscall::sys_pipe_fd_set(me, read_fd, h, false)?;
    syscall::sys_pipe_fd_set(me, write_fd, h, true)?;
    Ok(())
}

fn test_close() {
    println!("close:");
    if own_pipe(3, 4).is_err() {
        check("pipe wired to fd 3 and 4", false);
        return;
    }
    check("pipe wired to fd 3 and 4", true);

    // Both return a count, or u64::MAX; there is no Result on this path.
    let mut buf = [0u8; 8];
    check("write to the write end", syscall::sys_fd_write(4, b"hi") == 2);
    check(
        "read gets the bytes back",
        syscall::sys_fd_read(3, &mut buf) == 2 && &buf[..2] == b"hi",
    );

    // Closing the last writer is what turns a read into EOF. Without a close
    // call there is no way to say so.
    check("close the write end", syscall::sys_fd_close(4).is_ok());
    check("read now reports EOF", syscall::sys_fd_read(3, &mut buf) == 0);
    check("close the read end", syscall::sys_fd_close(3).is_ok());
    check(
        "closing an empty descriptor fails",
        syscall::sys_fd_close(3).is_err(),
    );
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[dtest] Phase 10 acceptance");
    test_close();

    unsafe {
        println!("[dtest] {} passed, {} failed", PASSED, FAILED);
        syscall::sys_exit_code(if FAILED == 0 { 0 } else { 1 });
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dtest] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
