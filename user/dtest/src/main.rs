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

fn test_fd_table() {
    println!("descriptor table:");
    // Eight pipes is the per-task limit, which gives sixteen ends — enough to
    // prove the table is deeper than the eight entries it used to have.
    let mut wired = 0;
    for i in 0..8 {
        let r = 3 + i * 2;
        let w = 4 + i * 2;
        if r >= 32 || w >= 32 || own_pipe(r, w).is_err() {
            break;
        }
        wired += 1;
    }
    check("wired eight pipes into sixteen descriptors", wired == 8);

    // The highest of them must actually work, not merely be accepted.
    let mut buf = [0u8; 8];
    check("write to fd 18", syscall::sys_fd_write(18, b"deep") == 4);
    check(
        "read from fd 17",
        syscall::sys_fd_read(17, &mut buf) == 4 && &buf[..4] == b"deep",
    );

    for i in 0..wired {
        let _ = syscall::sys_fd_close(3 + i * 2);
        let _ = syscall::sys_fd_close(4 + i * 2);
    }
}

const SHM_AT: usize = 0x94_0000_0000;

fn test_big_region() {
    println!("shared memory:");
    // 2000 pages is two 1280x800 buffers: the case that could not be
    // expressed when a region was capped at 1024 pages.
    let handle = match syscall::sys_shmem_create(2000) {
        Ok(h) => h,
        Err(()) => {
            check("create a 2000-page region", false);
            return;
        }
    };
    check("create a 2000-page region", true);
    check("map it", syscall::sys_shmem_map(handle, SHM_AT).is_ok());

    // Write the page number into the first word of every page and read it
    // back. A run list that stitches its runs together wrongly shows up here
    // and nowhere else.
    let mut good = true;
    for p in 0..2000usize {
        let at = (SHM_AT + p * 4096) as *mut u64;
        unsafe { core::ptr::write_volatile(at, p as u64 ^ 0x5A5A_0000) };
    }
    for p in 0..2000usize {
        let at = (SHM_AT + p * 4096) as *const u64;
        if unsafe { core::ptr::read_volatile(at) } != p as u64 ^ 0x5A5A_0000 {
            good = false;
            break;
        }
    }
    check("every one of its 2000 pages is distinct and readable", good);

    check("unmap", syscall::sys_shmem_unmap(handle, SHM_AT).is_ok());
    check("destroy", syscall::sys_shmem_destroy(handle).is_ok());
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[dtest] Phase 10 acceptance");
    test_close();
    test_fd_table();
    test_big_region();

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
