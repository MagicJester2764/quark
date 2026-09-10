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

const MEMFD_AT: usize = 0x95_0000_0000;

fn test_memfd() {
    println!("memory as a descriptor:");
    let fd = match syscall::sys_memfd_create(4) {
        Ok(f) => f,
        Err(()) => {
            check("create a four-page memory descriptor", false);
            return;
        }
    };
    check("create a four-page memory descriptor", fd >= 3);
    check("map it", syscall::sys_mmap_fd(fd, MEMFD_AT).is_ok());

    unsafe { core::ptr::write_volatile(MEMFD_AT as *mut u64, 0xFEED_FACE) };
    check(
        "what was written is there",
        unsafe { core::ptr::read_volatile(MEMFD_AT as *const u64) } == 0xFEED_FACE,
    );

    check("close it", syscall::sys_fd_close(fd).is_ok());
    check(
        "mapping a closed descriptor fails",
        syscall::sys_mmap_fd(fd, MEMFD_AT + 0x10000).is_err(),
    );
}

fn test_socketpair() {
    println!("socketpair:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => {
            check("create a pair", false);
            return;
        }
    };
    check("create a pair", a >= 3 && b >= 3 && a != b);

    let mut buf = [0u8; 16];
    check("a writes", syscall::sys_fd_write(a, b"ping") == 4);
    check(
        "b reads what a wrote",
        syscall::sys_fd_read(b, &mut buf) == 4 && &buf[..4] == b"ping",
    );
    // The direction a pipe cannot do.
    check("b writes", syscall::sys_fd_write(b, b"pong") == 4);
    check(
        "a reads what b wrote",
        syscall::sys_fd_read(a, &mut buf) == 4 && &buf[..4] == b"pong",
    );

    check("close a", syscall::sys_fd_close(a).is_ok());
    check("b now reads EOF", syscall::sys_fd_read(b, &mut buf) == 0);
    check("close b", syscall::sys_fd_close(b).is_ok());
}

const PASSED_AT: usize = 0x96_0000_0000;

fn test_fd_passing() {
    println!("descriptor passing:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair to pass over", false); return; }
    };
    let mem = match syscall::sys_memfd_create(2) {
        Ok(f) => f,
        Err(()) => { check("memory to pass", false); return; }
    };
    check("a pair and some memory", true);

    // Write a witness through the sender's own mapping first.
    check("map it here", syscall::sys_mmap_fd(mem, PASSED_AT).is_ok());
    unsafe { core::ptr::write_volatile(PASSED_AT as *mut u64, 0xC0FFEE) };

    check(
        "send the descriptor with a byte",
        syscall::sys_fd_send(a, b"m", Some(mem)) == Ok(1),
    );

    let mut buf = [0u8; 4];
    check(
        "receive says a descriptor came",
        syscall::sys_fd_recv(b, &mut buf, Some(20)) == Ok((1, true)),
    );

    // The received descriptor is a different number naming the same memory.
    check(
        "map the received descriptor",
        syscall::sys_mmap_fd(20, PASSED_AT + 0x8000).is_ok(),
    );
    check(
        "it is the same memory",
        unsafe { core::ptr::read_volatile((PASSED_AT + 0x8000) as *const u64) } == 0xC0FFEE,
    );

    // Receiving when nothing was attached must not invent one.
    check("send with no descriptor", syscall::sys_fd_send(a, b"x", None) == Ok(1));
    check(
        "receive says none came",
        syscall::sys_fd_recv(b, &mut buf, Some(21)) == Ok((1, false)),
    );

    let _ = syscall::sys_fd_close(20);
    let _ = syscall::sys_fd_close(mem);

    // In flight, with the sender's own copy gone. The queue has to hold a
    // reference of its own, or the region is freed under the descriptor
    // travelling towards the peer and the receiver maps freed memory.
    let orphan = match syscall::sys_memfd_create(1) {
        Ok(f) => f,
        Err(()) => { check("memory to orphan", false); return; }
    };
    check("map the orphan here", syscall::sys_mmap_fd(orphan, PASSED_AT + 0x20000).is_ok());
    unsafe { core::ptr::write_volatile((PASSED_AT + 0x20000) as *mut u64, 0xBEEF) };
    check("send it", syscall::sys_fd_send(a, b"o", Some(orphan)) == Ok(1));
    check("close the only other copy", syscall::sys_fd_close(orphan).is_ok());
    check(
        "receive it anyway",
        syscall::sys_fd_recv(b, &mut buf, Some(22)) == Ok((1, true)),
    );
    check("map what arrived", syscall::sys_mmap_fd(22, PASSED_AT + 0x28000).is_ok());
    check(
        "and it still holds what was written",
        unsafe { core::ptr::read_volatile((PASSED_AT + 0x28000) as *const u64) } == 0xBEEF,
    );

    let _ = syscall::sys_fd_close(22);
    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}

fn test_no_leak() {
    println!("abandoned descriptors are reclaimed:");
    // Send a descriptor and throw the connection away without receiving it,
    // forty times. There are thirty-two streams and the region table is
    // finite, so anything that fails to give back what it took runs out
    // before this loop does.
    let mut rounds = 0;
    for _ in 0..40 {
        let Ok((a, b)) = syscall::sys_socketpair() else { break };
        let Ok(mem) = syscall::sys_memfd_create(1) else {
            let _ = syscall::sys_fd_close(a);
            let _ = syscall::sys_fd_close(b);
            break;
        };
        if syscall::sys_fd_send(a, b"z", Some(mem)) != Ok(1) {
            break;
        }
        // Everybody drops it: the sender's copy, and both ends of the stream
        // that was carrying the one in flight.
        let _ = syscall::sys_fd_close(mem);
        let _ = syscall::sys_fd_close(a);
        let _ = syscall::sys_fd_close(b);
        rounds += 1;
    }
    check("forty rounds of send-and-abandon", rounds == 40);

    // And the tables still work afterwards.
    match syscall::sys_socketpair() {
        Ok((a, b)) => {
            check("a stream can still be made", true);
            let _ = syscall::sys_fd_close(a);
            let _ = syscall::sys_fd_close(b);
        }
        Err(()) => check("a stream can still be made", false),
    }
    match syscall::sys_memfd_create(1) {
        Ok(m) => {
            check("memory can still be made", true);
            let _ = syscall::sys_fd_close(m);
        }
        Err(()) => check("memory can still be made", false),
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[dtest] Phase 10 acceptance");
    test_close();
    test_fd_table();
    test_big_region();
    test_memfd();
    test_socketpair();
    test_fd_passing();
    test_no_leak();

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
