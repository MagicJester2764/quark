#![no_std]
#![no_main]

//! Acceptance checks for the kernel and the runtime, from Phase 10 on:
//! descriptors, streams, waiting, the environment, memory and authority.
//!
//! There is no test framework here, so this is one: a program that asserts and
//! exits non-zero. Run it from the shell; `dtest NAME` runs one section, named
//! in the table in `_start`.

use quark_rt::manifest::CapReq;
use quark_rt::wl::wire;
use quark_rt::{nameserver, println, spawn, sync, syscall, thread, vfs};

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

/// A capability over more physical memory than this is not one device's: a
/// framebuffer is a few megabytes, a boot module one. The grants Phase 12
/// removes were four gigabytes.
const DEVICE_SPAN: u64 = 64 << 20;
/// Where the kernel is loaded, which no task may map.
const KERNEL_IMAGE: u64 = 0x10_0000;

fn test_physical_authority() {
    println!("physical memory authority:");
    let mut seen = 0;
    let mut broad = 0;
    let mut kernel = 0;
    for tid in 1..64 {
        if syscall::sys_task_info(tid).is_err() {
            continue;
        }
        for slot in 0.. {
            let Ok(cap) = syscall::sys_cap_read(tid, slot) else { break };
            if cap.cap_type != syscall::CAP_TYPE_PHYS_RANGE || !cap.valid {
                continue;
            }
            seen += 1;
            if cap.param1.saturating_sub(cap.param0) > DEVICE_SPAN {
                println!("    tid {} may map {:#x}..{:#x}", tid, cap.param0, cap.param1);
                broad += 1;
            }
            if cap.param0 <= KERNEL_IMAGE && KERNEL_IMAGE < cap.param1 {
                kernel += 1;
            }
        }
    }
    check("another task's capabilities can be read", seen > 0);
    check("no task may map more than one device's memory", broad == 0);
    check("no task may map the kernel", kernel == 0);
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
        syscall::sys_fd_recv(b, &mut buf, Some(20)) == Ok((1, Some(20))),
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
        syscall::sys_fd_recv(b, &mut buf, Some(21)) == Ok((1, None)),
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
        syscall::sys_fd_recv(b, &mut buf, Some(22)) == Ok((1, Some(22))),
    );
    check("map what arrived", syscall::sys_mmap_fd(22, PASSED_AT + 0x28000).is_ok());
    check(
        "and it still holds what was written",
        unsafe { core::ptr::read_volatile((PASSED_AT + 0x28000) as *const u64) } == 0xBEEF,
    );

    let _ = syscall::sys_fd_close(22);

    // Asking for any slot rather than naming one. A caller translating
    // `recvmsg` has no way to name one: Linux chooses the number, and the
    // alternative -- probing -- means reading, which is the one thing a
    // receive must do exactly once.
    let any = match syscall::sys_memfd_create(1) {
        Ok(f) => f,
        Err(()) => { check("memory to send anywhere", false); return; }
    };
    check("send it", syscall::sys_fd_send(a, b"x", Some(any)) == Ok(1));
    let landed = syscall::sys_fd_recv(b, &mut buf, Some(syscall::ANY_FD));
    check("receive into a slot of the kernel's choosing", matches!(landed, Ok((1, Some(_)))));
    let slot = match landed { Ok((_, Some(s))) => s, _ => 0 };
    check("the slot it named is above the standard three", slot >= 3);
    check("and it holds memory", syscall::sys_mmap_fd(slot, PASSED_AT + 0x30000).is_ok());
    let _ = syscall::sys_fd_close(slot);
    let _ = syscall::sys_fd_close(any);

    // Sizing memory after making it. This is `ftruncate`, and every Wayland
    // client's buffer pool is made that way: memfd_create, then ftruncate,
    // then mmap.
    let grow = match syscall::sys_memfd_create(1) {
        Ok(f) => f,
        Err(()) => { check("memory to grow", false); return; }
    };
    check("one page to start with", syscall::sys_memfd_truncate(grow, 0).is_err());
    check("grow it to ten", syscall::sys_memfd_truncate(grow, 10 * 4096) == Ok(10 * 4096));
    check(
        "a size that is not a whole page rounds up",
        syscall::sys_memfd_truncate(grow, 4097) == Ok(2 * 4096),
    );
    check(
        "and mapping it says how big it became",
        syscall::sys_mmap_fd(grow, PASSED_AT + 0x40000) == Ok(2 * 4096),
    );
    unsafe { core::ptr::write_volatile((PASSED_AT + 0x40000 + 4096) as *mut u64, 0x1234) };
    check(
        "the second page is really there",
        unsafe { core::ptr::read_volatile((PASSED_AT + 0x40000 + 4096) as *const u64) } == 0x1234,
    );
    // Not while somebody holds a mapping: growing would change what is behind
    // it, and nothing would tell them.
    check("no resizing what is mapped", syscall::sys_memfd_truncate(grow, 4 * 4096).is_err());
    let _ = syscall::sys_munmap(PASSED_AT + 0x40000, 2);

    // A second name for one of your own descriptors, which needs no authority.
    let copy = match syscall::sys_fd_dup_self(grow, 3) {
        Ok(f) => f,
        Err(()) => { check("duplicate a descriptor", false); return; }
    };
    check("the duplicate is a different number", copy != grow);
    check("and it names the same memory", syscall::sys_mmap_fd(copy, PASSED_AT + 0x48000).is_ok());
    check(
        "a floor is respected",
        matches!(syscall::sys_fd_dup_self(grow, 20), Ok(f) if f >= 20),
    );
    let _ = syscall::sys_munmap(PASSED_AT + 0x48000, 2);
    let _ = syscall::sys_fd_close(copy);
    let _ = syscall::sys_fd_close(grow);

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

fn test_pollset() {
    println!("waiting on a set:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair to watch", false); return; }
    };
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("create a set", false); return; }
    };
    check("create a set", set >= 3);
    check(
        "watch b for readable",
        syscall::sys_pollset_add(set, b, syscall::POLL_READABLE, 0xB).is_ok(),
    );
    check(
        "watch a for writable",
        syscall::sys_pollset_add(set, a, syscall::POLL_WRITABLE, 0xA).is_ok(),
    );

    let mut ready = [syscall::Ready::empty(); 4];

    // `a` is writable now and `b` is not readable, so exactly one fires.
    check("one is ready", syscall::sys_pollset_wait(set, &mut ready, 50) == Ok(1));
    check("and it is the writable one", ready[0].token == 0xA);

    // Stop watching `a`, then nothing is ready until something is written.
    check("stop watching a", syscall::sys_pollset_remove(set, a).is_ok());
    check(
        "nothing ready, and it timed out",
        syscall::sys_pollset_wait(set, &mut ready, 5) == Ok(0),
    );

    check("write to a", syscall::sys_fd_write(a, b"go") == 2);
    let n = syscall::sys_pollset_wait(set, &mut ready, 50);
    check("now b is ready", n == Ok(1) && ready[0].token == 0xB);
    check(
        "readable is what it reports",
        ready[0].events & syscall::POLL_READABLE != 0,
    );

    // A closed peer is a hangup rather than a silence.
    let mut buf = [0u8; 4];
    let _ = syscall::sys_fd_read(b, &mut buf);
    check("close a", syscall::sys_fd_close(a).is_ok());
    let n = syscall::sys_pollset_wait(set, &mut ready, 50);
    check(
        "b reports hangup",
        n == Ok(1) && ready[0].events & syscall::POLL_HANGUP != 0,
    );

    // A descriptor that can never become ready is refused, not accepted and
    // then silent. Note that stdout is *not* an example: init wires it to a
    // pipe, so watching it for writable is a reasonable thing to ask and the
    // kernel is right to allow it.
    check(
        "watching an empty descriptor is refused",
        syscall::sys_pollset_add(set, 30, syscall::POLL_READABLE, 0xC).is_err(),
    );
    check(
        "watching stdin, an IPC endpoint, is refused",
        syscall::sys_pollset_add(set, 0, syscall::POLL_READABLE, 0xD).is_err(),
    );
    check(
        "watching stdout, which really is a pipe, is allowed",
        syscall::sys_pollset_add(set, 1, syscall::POLL_WRITABLE, 0xE).is_ok(),
    );

    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(b);
}

/// The waking thread's end of the pair, handed to it as descriptor 3.
const WAKER_FD: usize = 3;

/// Sleep a little, then write. Run on a thread so that something can become
/// ready while the main task is blocked in a wait — which is the whole of what
/// Task 8 adds, and cannot be tested from one task.
extern "C" fn waker() -> ! {
    syscall::sleep_ticks(5);
    let _ = syscall::sys_fd_write(WAKER_FD, b"wake");
    syscall::sys_exit_code(0);
}

fn test_wake_latency() {
    println!("waiting wakes promptly:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("a set", false); return; }
    };
    let _ = syscall::sys_pollset_add(set, b, syscall::POLL_READABLE, 1);

    // Data already waiting: a correct wait returns without sleeping at all.
    let _ = syscall::sys_fd_write(a, b"now");
    let before = syscall::sys_ticks();
    let mut ready = [syscall::Ready::empty(); 2];
    let n = syscall::sys_pollset_wait(set, &mut ready, 100);
    let elapsed = syscall::sys_ticks() - before;
    check("data already waiting returns at once", n == Ok(1) && elapsed <= 1);

    let mut buf = [0u8; 8];
    let _ = syscall::sys_fd_read(b, &mut buf);

    // Nothing to read: this must run its full timeout and not return early.
    let before = syscall::sys_ticks();
    let n = syscall::sys_pollset_wait(set, &mut ready, 20);
    let elapsed = syscall::sys_ticks() - before;
    check("an empty wait runs its full timeout", n == Ok(0) && elapsed >= 20);

    // And the one that matters: something becomes ready *while* we are
    // blocked. Without a wake path the wait sleeps its whole timeout and only
    // then notices, so the check is on the clock and not on the answer.
    let Ok(t) = thread::spawn_with_stack(waker, 8) else {
        check("start a thread to wake us", false);
        return;
    };
    check("start a thread to wake us", true);
    if syscall::sys_fd_dup(t.tid(), WAKER_FD, a).is_err() {
        check("give it the other end", false);
        return;
    }
    check("give it the other end", true);

    let before = syscall::sys_ticks();
    let n = syscall::sys_pollset_wait(set, &mut ready, 300);
    let elapsed = syscall::sys_ticks() - before;
    check("woken by the write, not by the deadline", n == Ok(1) && elapsed < 100);

    let _ = syscall::sys_fd_read(b, &mut buf);
    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}

fn test_poll() {
    println!("one-shot poll:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };

    // `a` is writable and `b` is not readable, so exactly one fires — and it
    // has to land in the right entry, which is what revents is for.
    let mut fds = [
        syscall::PollFd::new(b, syscall::POLL_READABLE),
        syscall::PollFd::new(a, syscall::POLL_WRITABLE),
    ];
    check("the writable end fires", syscall::sys_poll(&mut fds, 50) == Ok(1));
    check(
        "and it is the second entry",
        fds[1].revents & syscall::POLL_WRITABLE != 0,
    );
    check("the first reports nothing", fds[0].revents == 0);

    // Nothing ready: run the timeout rather than returning early.
    let mut fds = [syscall::PollFd::new(b, syscall::POLL_READABLE)];
    let before = syscall::sys_ticks();
    let n = syscall::sys_poll(&mut fds, 15);
    let elapsed = syscall::sys_ticks() - before;
    check("nothing ready times out", n == Ok(0) && elapsed >= 15);

    let _ = syscall::sys_fd_write(a, b"z");
    let mut fds = [syscall::PollFd::new(b, syscall::POLL_READABLE)];
    check("after a write it is readable", syscall::sys_poll(&mut fds, 50) == Ok(1));

    // A descriptor that cannot be waited on is reported as invalid rather than
    // failing the whole call, which is what poll(2) does.
    let mut fds = [
        syscall::PollFd::new(30, syscall::POLL_READABLE),
        syscall::PollFd::new(b, syscall::POLL_READABLE),
    ];
    let n = syscall::sys_poll(&mut fds, 50);
    check("an unwaitable descriptor is reported, not fatal", n == Ok(2));
    check(
        "and it is marked invalid",
        fds[0].revents & syscall::POLL_INVALID != 0,
    );
    check("while the good one still reports", fds[1].revents & syscall::POLL_READABLE != 0);

    let mut buf = [0u8; 4];
    let _ = syscall::sys_fd_read(b, &mut buf);
    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}

fn test_environment() {
    println!("environment:");
    // What the shell puts in every program's environment.
    check("HOME is set", quark_rt::args::getenv(b"HOME").is_some());
    check(
        "and it is a path",
        quark_rt::args::getenv(b"HOME").map(|v| v.starts_with(b"/")) == Some(true),
    );
    check("PATH is set", quark_rt::args::getenv(b"PATH").is_some());
    check("a name nobody set is absent", quark_rt::args::getenv(b"NOPE").is_none());
    // A prefix of a real name must not match it: without checking the `=`,
    // HOM matches HOME=/home/root and returns E=/home/root.
    check("HOM does not match HOME", quark_rt::args::getenv(b"HOM").is_none());
    // The environment sits after the arguments on the same page, so reading it
    // must not have disturbed them.
    check("arguments still readable", quark_rt::args::argv(0).is_some());
    check("and argv[0] is this program", quark_rt::args::argv(0) == Some(&b"dtest"[..]));
}

const CHILD_IMAGE: usize = 0x98_0000_0000;
const THEIR_MEM: usize = 0x99_0000_0000;
const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;

static SPAWN_SCRATCH: spawn::Scratch = spawn::Scratch {
    elf: 0x9A_0000_0000,
    stack: 0x9B_0000_0000,
    args: 0x9C_0000_0000,
};

/// Read `/usr/bin/dchild` and load it. Modelled on how `wm` starts a session
/// program: the image comes through the VFS, `spawn::load` builds the address
/// space, and the manifest decides what it is granted.
fn load_child(args: &[&[u8]]) -> Option<spawn::Spawned> {
    let vfs_tid = nameserver::lookup_retry(b"vfs", 20)?;
    // Lowercase for ext2, uppercase with .ELF for FAT32 — the two spellings
    // the shell already tries.
    let grant = |image: &[u8], tid: usize| {
        quark_rt::manifest::grant_image(tid, image, 12);
    };
    let info = spawn::load_path(vfs_tid, b"/usr/bin/dchild", CHILD_IMAGE, &SPAWN_SCRATCH, grant)
        .or_else(|()| {
            spawn::load_path(vfs_tid, b"/usr/bin/DCHILD.ELF", CHILD_IMAGE, &SPAWN_SCRATCH, grant)
        })
        .ok()?;
    // Every program is started with an argument page; reading one that was
    // never mapped faults.
    spawn::set_args(&info, args, &SPAWN_SCRATCH).ok()?;
    // It needs to be able to reach the nameserver, and somewhere to print.
    let _ = syscall::sys_cap_grant(info.tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);
    Some(info)
}

static SPACE_VFS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static SPACE_HANDLE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(usize::MAX);
static SPACE_OF_THREAD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Open a file and leave it open. It belongs to the program, not to this
/// thread, so it has to outlive the thread.
extern "C" fn opener() -> ! {
    use core::sync::atomic::Ordering::SeqCst;
    let me = syscall::sys_getpid() as usize;
    SPACE_OF_THREAD.store(syscall::sys_task_space(me).unwrap_or(0), SeqCst);
    if let Ok((handle, _, _)) = vfs::open(SPACE_VFS.load(SeqCst), b"/etc/passwd") {
        SPACE_HANDLE.store(handle, SeqCst);
    }
    syscall::sys_exit_code(0);
}

/// A program is its address space: its threads are part of it, and what one
/// of them opens is the program's.
fn test_program_is_its_space() {
    use core::sync::atomic::Ordering::SeqCst;
    let me = syscall::sys_getpid() as usize;
    let space = syscall::sys_task_space(me);
    check("a task belongs to a program", space.is_ok_and(|s| s != 0));
    // Looked up first, so the thread starts holding the capability to call it.
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        check("find the VFS", false);
        return;
    };
    SPACE_VFS.store(vfs_tid, SeqCst);
    let Ok(t) = thread::spawn_with_stack(opener, 8) else {
        check("start a thread to open a file", false);
        return;
    };
    // Joined before any child is started, since a join reaps whatever exits.
    let _ = t.join();
    check("a thread is part of its program", space == Ok(SPACE_OF_THREAD.load(SeqCst)));
    let handle = SPACE_HANDLE.load(SeqCst);
    let mut got = [0u8; 4];
    check(
        "a file a thread opened outlives the thread",
        handle != usize::MAX && vfs::read(vfs_tid, handle, &mut got, 0) == Ok(4) && &got == b"root",
    );
    if handle != usize::MAX {
        let _ = vfs::close(vfs_tid, handle);
    }
}

fn test_across_address_spaces() {
    println!("across address spaces:");
    test_program_is_its_space();
    let (mine, theirs) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let Some(info) = load_child(&[b"dchild"]) else {
        check("load /usr/bin/dchild", false);
        return;
    };
    check("load /usr/bin/dchild", true);
    // The pages the program was read into are a copy the child no longer
    // needs. sys_mmap refuses to map over a page that is still mapped, which
    // makes "was it given back" something a test can ask.
    check(
        "loading a program gives its staging memory back",
        syscall::sys_mmap(CHILD_IMAGE, 1).is_ok(),
    );
    let _ = syscall::sys_munmap(CHILD_IMAGE, 1);

    // Hand the child its end, then drop ours. If an end were a flag rather
    // than a count, this would tell the child's peer the end had gone.
    check(
        "give the child descriptor 3",
        syscall::sys_fd_dup(info.tid, 3, theirs).is_ok(),
    );
    check("drop our copy of it", syscall::sys_fd_close(theirs).is_ok());
    check("the stream is still alive", syscall::sys_fd_write(mine, b"go!\n") == 4);

    if info.start().is_err() {
        check("the child runs", false);
        return;
    }
    check("the child runs", true);
    let me = syscall::sys_getpid() as usize;
    check(
        "a child is another program",
        matches!(
            (syscall::sys_task_space(me), syscall::sys_task_space(info.tid)),
            (Ok(a), Ok(b)) if a != b
        ),
    );

    // Wait for its answer with the set, which is what makes this the whole
    // phase rather than three quarters of it.
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("a set to wait on", false); return; }
    };
    let _ = syscall::sys_pollset_add(set, mine, syscall::POLL_READABLE, 7);
    let mut ready = [syscall::Ready::empty(); 2];
    let n = syscall::sys_pollset_wait(set, &mut ready, 500);
    check(
        "the set wakes for the child's reply",
        n == Ok(1) && ready[0].token == 7,
    );

    let mut buf = [0u8; 8];
    check(
        "bytes and a descriptor arrived",
        syscall::sys_fd_recv(mine, &mut buf, Some(25)) == Ok((4, Some(25))),
    );
    check(
        "map memory the other task allocated",
        syscall::sys_mmap_fd(25, THEIR_MEM).is_ok(),
    );
    check(
        "and read what it wrote there",
        unsafe { core::ptr::read_volatile(THEIR_MEM as *const u64) } == WITNESS,
    );
    // The child has no TaskMgmt over anybody and nobody is calling it, so the
    // kernel must have refused to let it put a capability into this task's
    // CSpace. Filling sixteen slots is a denial of service even though a grant
    // can never raise the authority of the task it lands in.
    check(
        "a task cannot fill another's CSpace",
        unsafe { core::ptr::read_volatile((THEIR_MEM + 128) as *const u64) } == 1,
    );

    // A lock living in memory the two processes share. The child blocks on it
    // in its own address space and is woken from this one, which works only
    // because the kernel keys its futex queue on the physical address.
    let shared = unsafe { &*((THEIR_MEM + 64) as *const sync::Mutex<u64>) };
    let held = shared.lock();
    // The child is now blocked on this. Give it long enough to get there.
    syscall::sleep_ticks(10);
    drop(held);

    // Wait for the child to finish with it.
    let mut waited = 0;
    loop {
        {
            let v = shared.lock();
            if *v == 1 {
                break;
            }
        }
        syscall::sleep_ticks(1);
        waited += 1;
        if waited > 300 {
            break;
        }
    }
    check("a lock in shared memory works between processes", waited <= 300 && *shared.lock() == 1);

    let _ = syscall::sys_fd_close(25);
    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(mine);
}

/// Pages this task gives away, clear of everything else here.
const GIFT: usize = 0x9D_0000_0000;
/// Somewhere the child has nothing.
const CHILD_SPARE: usize = 0x90_0000_0000;

/// True if nothing is mapped at `at`. sys_mmap refuses to map over a page that
/// is present, which is what makes this a question a program can ask.
fn nothing_at(at: usize) -> bool {
    let free = syscall::sys_mmap(at, 1).is_ok();
    if free {
        let _ = syscall::sys_munmap(at, 1);
    }
    free
}

/// Wait for one particular child, collecting any other on the way.
fn wait_for(tid: usize) -> Option<i32> {
    loop {
        match syscall::sys_wait() {
            Ok((t, code)) if t == tid => return Some(code),
            Ok(_) => continue,
            Err(()) => return None,
        }
    }
}

fn test_spawned_memory() {
    println!("a program's memory is its own:");
    let Some(info) = load_child(&[b"dchild", b"quit"]) else {
        check("load /usr/bin/dchild", false);
        return;
    };
    // The loader builds a program in this task's memory and moves it across.
    // None of it may stay mapped here: a spawner still holding a page could
    // read whatever the frame held next, once the child was gone.
    check("its stack is not left mapped in the parent", nothing_at(SPAWN_SCRATCH.stack));
    check("nor its code", nothing_at(SPAWN_SCRATCH.elf));
    check("nor its arguments", nothing_at(SPAWN_SCRATCH.args));

    // Only memory a task owns can be given. A frame from sys_phys_alloc is
    // mapped without the mapping owning it, and whoever allocated it still
    // answers for it.
    let frame = syscall::sys_phys_alloc(1);
    let lent = frame.is_ok_and(|f| syscall::sys_map_phys(f, GIFT, 1).is_ok());
    check(
        "a frame mapped from elsewhere cannot be given",
        lent && syscall::sys_addrspace_give(info.cr3, CHILD_SPARE, GIFT, 1, 1).is_err(),
    );
    let _ = syscall::sys_munmap(GIFT, 1);
    if let Ok(f) = frame {
        let _ = syscall::sys_phys_free(f, 1);
    }

    let made = syscall::sys_mmap(GIFT, 1).is_ok();
    check(
        "a gift cannot replace a page the child has",
        made && syscall::sys_addrspace_give(
            info.cr3,
            spawn::STACK_TOP - spawn::PAGE_SIZE,
            GIFT,
            1,
            1,
        )
        .is_err(),
    );
    let me = syscall::sys_addrspace_self().unwrap_or(0);
    check(
        "nor go to the address space it came from",
        syscall::sys_addrspace_give(me, CHILD_SPARE, GIFT, 1, 1).is_err(),
    );
    check("a refused gift stays with the giver", syscall::sys_munmap(GIFT, 1) == Ok(1));

    let made = syscall::sys_mmap(GIFT, 1).is_ok();
    check(
        "memory of one's own can be given",
        made && syscall::sys_addrspace_give(info.cr3, CHILD_SPARE, GIFT, 1, 1).is_ok(),
    );
    check("and it leaves the giver", nothing_at(GIFT));

    check("the child runs with its gift", info.start().is_ok() && wait_for(info.tid) == Some(0));

    // A program whose thread exited before it did. Collecting the program
    // orphans the thread, and a dead orphan goes with it: left behind, it
    // named a parent that was gone, whatever took that slot next adopted it,
    // and the address space the two shared stayed allocated until then.
    let orphan = load_child(&[b"dchild", b"orphan"])
        .filter(|child| child.start().is_ok())
        .and_then(|child| wait_for(child.tid))
        .filter(|&tid| tid > 0);
    check("a program leaves a dead thread behind", orphan.is_some());
    check(
        "which is reaped when the program is collected",
        orphan.is_some_and(|tid| syscall::sys_task_info(tid as usize).is_err()),
    );

    // Each run costs a megabyte of stack and the program. A parent that went
    // on holding what it gave its children — or a machine that only reaped
    // them when it next went idle, which a parent doing this never lets it
    // do — is out of memory long before this loop is.
    let mut runs = 0;
    for _ in 0..160 {
        let Some(child) = load_child(&[b"dchild", b"quit"]) else { break };
        if child.start().is_err() || wait_for(child.tid) != Some(0) {
            break;
        }
        runs += 1;
    }
    check("run a program 160 times over", runs == 160);
}

static mut LEND_BUF: [u8; 64] = [0; 64];
static LEND_SERVER: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static LEND_GO: sync::Semaphore = sync::Semaphore::new(0);
/// What the lending thread saw. Bit 0: its first call was answered. 1: its
/// second was. 2: an unwritable buffer could not be lent for writing. 3:
/// nothing was lent to it while nobody was calling it.
static LEND_RESULTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The client half of `test_lent_buffers`: lends this task's main thread a
/// buffer three ways.
extern "C" fn lender() -> ! {
    use quark_rt::ipc::Message;
    // Not until main has given this thread the right to call it.
    LEND_GO.acquire();
    let server = LEND_SERVER.load(core::sync::atomic::Ordering::SeqCst);
    let ask = |tag| Message { sender: 0, tag, data: [0; 6] };
    let mut reply = Message::empty();
    let mut results = 0;
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LEND_BUF) };
    buf[..8].copy_from_slice(b"lent-buf");
    if syscall::sys_call_lend_rw(server, &ask(1), &mut reply, buf).is_ok() {
        results |= 1;
    }
    if syscall::sys_call_lend(server, &ask(2), &mut reply, &buf[..]).is_ok() {
        results |= 2;
    }
    // The argument page is mapped read-only.
    let args = unsafe {
        core::slice::from_raw_parts_mut(quark_rt::args::ARGS_PAGE_ADDR as *mut u8, 16)
    };
    if syscall::sys_call_lend_mut(server, &ask(3), &mut reply, args).is_err() {
        results |= 4;
    }
    let mut probe = [0u8; 1];
    if syscall::sys_lent_read(server, 0, &mut probe).is_err() {
        results |= 8;
    }
    LEND_RESULTS.store(results, core::sync::atomic::Ordering::SeqCst);
    syscall::sys_exit_code(0);
}

fn test_lent_buffers() {
    use quark_rt::ipc::Message;
    println!("lent buffers:");
    let me = syscall::sys_getpid() as usize;
    LEND_SERVER.store(me, core::sync::atomic::Ordering::SeqCst);
    let Ok(t) = thread::spawn_with_stack(lender, 8) else {
        check("start a thread to lend us a buffer", false);
        return;
    };
    let t = t.tid();
    // The thread may call this task: an Endpoint to it, from its creator.
    let granted = syscall::sys_cap_mint(syscall::SLOT_SCRATCH, syscall::CAP_TYPE_ENDPOINT, me as u64, 0)
        .is_ok()
        && syscall::sys_cap_grant_any(t, syscall::SLOT_SCRATCH).is_ok();
    let _ = syscall::sys_cap_delete(syscall::SLOT_SCRATCH);
    check("let the thread call us", granted);
    LEND_GO.release();

    let mut msg = Message::empty();
    let mut got = [0u8; 8];
    check("the lending call arrives", syscall::sys_recv(t, &mut msg).is_ok() && msg.tag == 1);
    check(
        "read what was lent",
        syscall::sys_lent_read(t, 0, &mut got) == Ok(8) && &got == b"lent-buf",
    );
    check("write into what was lent", syscall::sys_lent_write(t, 4, b"XY") == Ok(2));
    check("not past its end", syscall::sys_lent_read(t, 60, &mut got).is_err());
    check(
        "not at an offset that wraps",
        syscall::sys_lent_read(t, usize::MAX, &mut got[..1]).is_err(),
    );
    let _ = syscall::sys_reply(t, &Message::empty());
    check(
        "the write landed where it was aimed",
        unsafe { (&*core::ptr::addr_of!(LEND_BUF))[..8] == *b"lentXYuf" },
    );
    // Whatever the thread does next, its second call cannot be further along
    // than waiting to be received.
    check(
        "nothing is lent once the call is answered",
        syscall::sys_lent_read(t, 0, &mut got).is_err(),
    );

    check("a read-only lend arrives", syscall::sys_recv(t, &mut msg).is_ok() && msg.tag == 2);
    check("it can be read", syscall::sys_lent_read(t, 0, &mut got) == Ok(8));
    check("but not written", syscall::sys_lent_write(t, 0, b"Z").is_err());
    let _ = syscall::sys_reply(t, &Message::empty());

    let _ = wait_for(t);
    let results = LEND_RESULTS.load(core::sync::atomic::Ordering::SeqCst);
    check("both lending calls were answered", results & 3 == 3);
    check("an unwritable buffer cannot be lent for writing", results & 4 != 0);
    check("nothing is lent to a task nobody is calling", results & 8 != 0);
}

/// Slots for the endpoint checks. In the range a capability given without a
/// slot lands in, clear of the fixed ones below 16.
const SELF_SLOT: usize = 40;
const STRANGER_SLOT: usize = 41;
const CHILD_SLOT: usize = 42;
const NEXT_CHILD_SLOT: usize = 43;
const THREAD_SLOT: usize = 44;
/// Never filled, so there is nothing in it to offer.
const EMPTY_SLOT: usize = 45;
/// For the child `test_call_storm` calls.
const STORM_SLOT: usize = 46;
/// A task nothing here made or holds a capability to. Not the nameserver:
/// every program is handed one to that.
const INIT_TID: usize = 1;
/// The capability type that named a set of TIDs, withdrawn at ABI 2.0.
const WITHDRAWN_ENDPOINT_SET: u64 = 7;
/// The offering thread's own slots.
const OFFER_SLOT: usize = 8;
const HOLDER_SLOT: usize = 9;
const FOREIGN_SLOT: usize = 10;

fn mint_endpoint(slot: usize, tid: usize) -> bool {
    syscall::sys_cap_mint(slot, syscall::CAP_TYPE_ENDPOINT, tid as u64, 0).is_ok()
}

/// Call `tid` and return the tag it answers with: None if the call cannot be
/// made, or nobody answers within half a second.
fn call_tag(tid: usize) -> Option<u64> {
    use quark_rt::ipc::Message;
    let mut reply = Message::empty();
    match syscall::sys_call_timeout(tid, &Message::empty(), &mut reply, 50) {
        syscall::CallOutcome::Replied => Some(reply.tag),
        _ => None,
    }
}

static OFFER_TO: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
static OFFER_GO: sync::Semaphore = sync::Semaphore::new(0);
/// What the offering thread saw. Bit 0: holding a capability to main, it could
/// mint another. 1: its offering call was answered. 2: it could not mint one
/// to init, which it neither is, made, nor holds one for.
static OFFER_RESULTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// The client half of the offer checks: offers main a capability naming
/// itself, with a call.
extern "C" fn offerer() -> ! {
    use quark_rt::ipc::Message;
    // Not until main has given this thread the right to call it.
    OFFER_GO.acquire();
    let main = OFFER_TO.load(core::sync::atomic::Ordering::SeqCst);
    let me = syscall::sys_getpid() as usize;
    let mut results = 0;
    if mint_endpoint(HOLDER_SLOT, main) {
        results |= 1;
    }
    let ask = Message { sender: 0, tag: 1, data: [0; 6] };
    let mut reply = Message::empty();
    if mint_endpoint(OFFER_SLOT, me)
        && syscall::sys_call_offer(main, &ask, &mut reply, OFFER_SLOT).is_ok()
    {
        results |= 2;
    }
    if !mint_endpoint(FOREIGN_SLOT, INIT_TID) {
        results |= 4;
    }
    OFFER_RESULTS.store(results, core::sync::atomic::Ordering::SeqCst);
    syscall::sys_exit_code(0);
}

fn test_endpoint_objects() {
    use quark_rt::ipc::Message;
    println!("endpoints:");
    let me = syscall::sys_getpid() as usize;
    for slot in SELF_SLOT..=EMPTY_SLOT {
        let _ = syscall::sys_cap_delete(slot);
    }
    // Minting for yourself is ownership; minting for a stranger is not.
    check("a task may mint a capability to itself", mint_endpoint(SELF_SLOT, me));
    check(
        "but not to a task it did not make and cannot call",
        !mint_endpoint(STRANGER_SLOT, INIT_TID),
    );
    check(
        "though it may to one it can call",
        mint_endpoint(STRANGER_SLOT, nameserver::NAMESERVER_TID),
    );
    check(
        "and it records a number, not the task",
        syscall::sys_cap_read(me, SELF_SLOT).is_ok_and(|c| {
            c.cap_type == syscall::CAP_TYPE_ENDPOINT && c.param0 != me as u64 && c.valid
        }),
    );
    let _ = syscall::sys_cap_delete(STRANGER_SLOT);
    // The sets of TIDs these replaced are gone, even the one naming only the
    // caller that anybody could once mint.
    check(
        "a set of task IDs cannot be minted",
        syscall::sys_cap_mint(STRANGER_SLOT, WITHDRAWN_ENDPOINT_SET, 1u64 << me, 0).is_err(),
    );
    let _ = syscall::sys_cap_delete(STRANGER_SLOT);

    // A capability names a task, not the slot it ran in.
    let Some(a) = load_child(&[b"dchild", b"serve"]) else {
        check("start a child to call", false);
        return;
    };
    let _ = a.start();
    check("a creator may mint a capability to its child", mint_endpoint(CHILD_SLOT, a.tid));
    let ask = Message::empty();
    let mut reply = Message::empty();
    check(
        "offering an empty slot is refused",
        syscall::sys_call_offer(a.tid, &ask, &mut reply, EMPTY_SLOT).is_err(),
    );
    check("the capability reaches the child", call_tag(a.tid) == Some(42));
    check("the child answered and exited", wait_for(a.tid) == Some(0));
    check("nobody can mint one to a task that is gone", !mint_endpoint(STRANGER_SLOT, a.tid));
    let Some(b) = load_child(&[b"dchild", b"serve"]) else {
        check("start a second child", false);
        return;
    };
    let _ = b.start();
    check("the next child takes the same slot", b.tid == a.tid);
    check("and the old capability does not reach it", call_tag(b.tid).is_none());
    check("a fresh one is minted", mint_endpoint(NEXT_CHILD_SLOT, b.tid));
    // Giving a task an endpoint it already has costs nothing.
    let first = syscall::sys_cap_grant_any(b.tid, NEXT_CHILD_SLOT);
    let second = syscall::sys_cap_grant_any(b.tid, NEXT_CHILD_SLOT);
    check("a grant to any slot lands at 16 or above", first.is_ok_and(|s| s >= 16));
    check("and the same endpoint again lands in the same slot", first.is_ok() && first == second);
    check("the fresh one reaches the child", call_tag(b.tid) == Some(42));
    let _ = wait_for(b.tid);

    // Offers: a capability travels with a call, and the task called takes it.
    OFFER_TO.store(me, core::sync::atomic::Ordering::SeqCst);
    let Ok(t) = thread::spawn_with_stack(offerer, 8) else {
        check("start a thread to offer us a capability", false);
        return;
    };
    let t = t.tid();
    check(
        "let the thread call us",
        syscall::sys_cap_grant_any(t, SELF_SLOT).is_ok(),
    );
    OFFER_GO.release();
    let mut msg = Message::empty();
    let arrived = syscall::sys_recv_timeout(t, &mut msg, 100).is_ok() && msg.tag == 1;
    check("the offering call arrives", arrived);
    if arrived {
        let taken = syscall::sys_cap_take_any(t);
        check("take what was offered", taken.is_ok_and(|s| s >= 16));
        let taken = taken.unwrap_or(0);
        check("a creator may mint a capability to its thread", mint_endpoint(THREAD_SLOT, t));
        let number = |slot| syscall::sys_cap_read(me, slot).map(|c| (c.cap_type, c.param0));
        check(
            "and what was taken names the same task",
            number(taken).is_ok() && number(taken) == number(THREAD_SLOT),
        );
        check("an offer is taken once", syscall::sys_cap_take_any(t).is_err());
        check(
            "and not from a task that is not calling",
            syscall::sys_cap_take_any(nameserver::NAMESERVER_TID).is_err(),
        );
        let _ = syscall::sys_cap_delete(taken);
        let _ = syscall::sys_reply(t, &Message::empty());
    }
    let _ = wait_for(t);
    let results = OFFER_RESULTS.load(core::sync::atomic::Ordering::SeqCst);
    check("a holder may mint another", results & 1 != 0);
    check("the offering call was answered", results & 2 != 0);
    check("a thread cannot mint one to a stranger", results & 4 != 0);

    for slot in SELF_SLOT..=EMPTY_SLOT {
        let _ = syscall::sys_cap_delete(slot);
    }
}

fn test_runtime_service() {
    println!("a service started at run time:");
    let Some(server) = load_child(&[b"dchild", b"register", b"dchild-svc"]) else {
        check("start a service", false);
        return;
    };
    let _ = server.start();
    // Its registration is what makes it reachable, so wait for that.
    let registered = (0..100).any(|_| {
        nameserver::lookup(b"dchild-svc") == Some(server.tid) || {
            syscall::sleep_ticks(1);
            false
        }
    });
    check("it registers", registered);
    check(
        "a second task cannot take its name",
        nameserver::register(b"dchild-svc").is_err(),
    );
    let Some(client) = load_child(&[b"dchild", b"lookup", b"dchild-svc"]) else {
        check("start a client", false);
        return;
    };
    let _ = client.start();
    // Whichever finishes first: collecting one must not throw the other away.
    let (mut served, mut reached) = (None, None);
    while served.is_none() || reached.is_none() {
        match syscall::sys_wait() {
            Ok((t, code)) if t == server.tid => served = Some(code),
            Ok((t, code)) if t == client.tid => reached = Some(code),
            Ok(_) => {}
            Err(()) => break,
        }
    }
    check("a program it was never introduced to reaches it by name", reached == Some(42));
    check("and the service answered", served == Some(0));
    let gone = (0..100).any(|_| {
        nameserver::lookup(b"dchild-svc").is_none() || {
            syscall::sleep_ticks(1);
            false
        }
    });
    check("its name goes with it", gone);
    check("and can be taken again", nameserver::register(b"dchild-svc").is_ok());
}

/// Start `dchild MODE PATH` with one end of a fresh pair as its descriptor 3,
/// and return it with this end.
fn lock_child(mode: &[u8], path: &[u8]) -> Option<(spawn::Spawned, usize)> {
    let (mine, theirs) = syscall::sys_socketpair().ok()?;
    let child = load_child(&[b"dchild", mode, path])?;
    let given = syscall::sys_fd_dup(child.tid, 3, theirs).is_ok();
    let _ = syscall::sys_fd_close(theirs);
    if !given || child.start().is_err() {
        let _ = syscall::sys_fd_close(mine);
        return None;
    }
    Some((child, mine))
}

/// One byte from `fd`, which a child writes when it has done something.
fn child_says(fd: usize) -> Option<u8> {
    let mut b = [0u8; 1];
    (syscall::sys_fd_read(fd, &mut b) == 1).then_some(b[0])
}

fn test_locks() {
    println!("locks across programs:");
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        check("find the VFS", false);
        return;
    };
    const FILE: &[u8] = b"/tmp/dtest-lock";
    if let Ok(o) = vfs::open_with(vfs_tid, FILE, vfs::OPEN_CREATE) {
        let _ = vfs::close(vfs_tid, o.handle);
    }
    let Ok((h, _, _)) = vfs::open(vfs_tid, FILE) else {
        check("open a file to lock", false);
        return;
    };
    let (ex, wait, query) = (vfs::LOCK_EXCLUSIVE, vfs::LOCK_WAIT, vfs::LOCK_QUERY);

    // Another program's lock keeps this one out, until that program has gone.
    match lock_child(b"lock", FILE) {
        Some((child, mine)) => {
            check("a child takes a lock", child_says(mine) == Some(b'L'));
            check(
                "which keeps this program out",
                vfs::lock(vfs_tid, h, ex, 0, 0, 0).err() == Some(vfs::ERR_WOULD_BLOCK),
            );
            let theirs = syscall::sys_task_space(child.tid).unwrap_or(0);
            check(
                "and a query names the child",
                vfs::lock(vfs_tid, h, ex, 0, 0, query).is_ok_and(|a| a[0] == ex && a[3] == theirs),
            );
            let _ = syscall::sys_fd_close(mine);
            check("the child lets go and exits", wait_for(child.tid) == Some(0));
            check("and its lock went with it", vfs::lock(vfs_tid, h, ex, 0, 0, 0).is_ok());
            let _ = vfs::lock(vfs_tid, h, vfs::LOCK_UNLOCK, 0, 0, 0);
        }
        None => check("a child takes a lock", false),
    }

    // Two programs each waiting for what the other holds.
    let _ = vfs::lock(vfs_tid, h, ex, 0, 1, 0);
    match lock_child(b"lock2", FILE) {
        Some((child, mine)) => {
            check("a child takes byte 1", child_says(mine) == Some(b'1'));
            // Its next call waits for byte 0, which this program holds.
            let blocked = (0..100).any(|_| {
                syscall::sys_task_info(child.tid).is_ok_and(|(state, _, _)| state == 2) || {
                    syscall::sleep_ticks(1);
                    false
                }
            });
            syscall::sleep_ticks(5);
            check("and waits for byte 0", blocked);
            check(
                "waiting for byte 1 would never end",
                vfs::lock(vfs_tid, h, ex, 1, 1, wait).err() == Some(vfs::ERR_DEADLOCK),
            );
            let _ = vfs::lock(vfs_tid, h, vfs::LOCK_UNLOCK, 0, 1, 0);
            check("letting go of byte 0 lets the child in", child_says(mine) == Some(b'2'));
            let _ = syscall::sys_fd_close(mine);
            let _ = wait_for(child.tid);
        }
        None => check("a child takes byte 1", false),
    }
    let _ = vfs::close(vfs_tid, h);
    let _ = vfs::unlink(vfs_tid, FILE);
}

/// Where the memory section reserves its gigabyte.
const LAZY: usize = 0xA0_0000_0000;
const LAZY_PAGES: usize = 262_144;

fn test_memory() {
    println!("memory on demand:");
    let (free0, charged0) = syscall::sys_mem_info();
    check("a gigabyte is reserved", syscall::sys_map_anon(LAZY, LAZY_PAGES, false).is_ok());
    let (free1, charged1) = syscall::sys_mem_info();
    check(
        "and costs a page table at most",
        charged1 == charged0 && free0.saturating_sub(free1) <= 2,
    );
    check("reserving it again is refused", syscall::sys_map_anon(LAZY, 1, false).is_err());
    check("and so is mapping over it", syscall::sys_mmap(LAZY + 4096, 1).is_err());
    // Sixteen pages, far apart, each in a reservation of its own until now.
    let page = |i: usize| LAZY + i * 16_000 * 4096;
    for i in 0..16 {
        unsafe { core::ptr::write_volatile(page(i) as *mut u8, i as u8 + 1) };
    }
    let (free2, charged2) = syscall::sys_mem_info();
    check("touching sixteen pages charges sixteen", charged2 == charged1 + 16);
    check("and takes at least sixteen frames", free1.saturating_sub(free2) >= 16);
    check(
        "each keeps what was written",
        (0..16).all(|i| unsafe { core::ptr::read_volatile(page(i) as *const u8) } == i as u8 + 1),
    );

    // The kernel copies out of a page nothing has touched.
    let untouched = unsafe { core::slice::from_raw_parts((LAZY + 1000 * 4096 + 7) as *const u8, 64) };
    let written = nameserver::lookup_retry(b"vfs", 20).and_then(|vfs_tid| {
        let o = vfs::open_with(vfs_tid, b"/dev/null", 0).ok()?;
        let n = vfs::write(vfs_tid, o.handle, untouched, 0);
        let _ = vfs::close(vfs_tid, o.handle);
        n.ok()
    });
    check("an untouched page can be lent", written == Some(64));

    for chunk in (0..LAZY_PAGES).step_by(256) {
        let _ = syscall::sys_munmap(LAZY + chunk * 4096, 256);
    }
    check("unmapping gives the charge back", syscall::sys_mem_info().1 == charged0);
    check("and the range is free again", syscall::sys_mmap(LAZY, 1).is_ok());
    let _ = syscall::sys_munmap(LAZY, 1);

    // A program that takes more than it may is stopped, and gives it all back.
    let hog = load_child(&[b"dchild", b"hog"]).map(|c| {
        let _ = syscall::sys_set_mem_limit(c.tid, 2048);
        let _ = c.start();
        wait_for(c.tid)
    });
    check("a program past its limit ends with SIGBUS", hog == Some(Some(-7)));
    let (free3, _) = syscall::sys_mem_info();
    check("and its memory comes back", free3 + 64 >= free0);
}

fn test_random() {
    println!("random numbers:");
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    check("the kernel fills a buffer", syscall::sys_getrandom(&mut a) == Ok(32));
    check("and another, differently", syscall::sys_getrandom(&mut b) == Ok(32) && a != b);
    check("with something other than zeroes", a.iter().any(|&x| x != 0));
    check("an empty request is answered", syscall::sys_getrandom(&mut []) == Ok(0));
    // A buffer the caller cannot write is refused, not written.
    let bad = unsafe { core::slice::from_raw_parts_mut(0x1000 as *mut u8, 16) };
    check("a buffer that is not the caller's is refused", syscall::sys_getrandom(bad).is_err());
    let mut big = [0u8; 5000];
    check(
        "the runtime fills more than a page",
        quark_rt::random::fill(&mut big).is_ok() && big[4096..].iter().any(|&x| x != 0),
    );
}

/// `dir`/entry-NN-nnn…, the name 100 bytes long. Returns the path's length.
fn listing_entry(buf: &mut [u8; 160], dir: &[u8], i: usize) -> usize {
    buf[..dir.len()].copy_from_slice(dir);
    let mut n = dir.len();
    buf[n..n + 7].copy_from_slice(b"/entry-");
    n += 7;
    buf[n] = b'0' + (i / 10) as u8;
    buf[n + 1] = b'0' + (i % 10) as u8;
    buf[n + 2] = b'-';
    n += 3;
    let end = dir.len() + 1 + 100;
    buf[n..end].fill(b'n');
    end
}

fn test_files() {
    println!("files:");
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        check("find the VFS", false);
        return;
    };
    // Paths are lent, so their length is the filesystem's business.
    let dir: &[u8] = b"/tmp/dtest-a-directory-whose-name-alone-is-past-the-old-limit";
    let file: &[u8] = b"/tmp/dtest-a-directory-whose-name-alone-is-past-the-old-limit/and-a-file";
    check(
        "make a directory with a long path",
        matches!(vfs::mkdir(vfs_tid, dir), Ok(()) | Err(vfs::ERR_EXISTS)),
    );
    if let Ok(o) = vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE) {
        let _ = vfs::write(vfs_tid, o.handle, b"long paths", 0);
        let _ = vfs::close(vfs_tid, o.handle);
    }
    let again = vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE);
    check("creating a file again opens it", again.as_ref().is_ok_and(|o| o.size == 10 && !o.is_dir));
    let other = vfs::open(vfs_tid, b"/etc/passwd");
    if let (Ok(a), Ok((b, _, _))) = (&again, &other) {
        let ids = (vfs::stat_full(vfs_tid, a.handle), vfs::stat_full(vfs_tid, *b));
        check(
            "stat names the inode, not the handle",
            matches!(ids, (Ok(x), Ok(y)) if x.id == a.id && x.id != y.id && x.links >= 1),
        );
    }
    for h in [again.map(|o| o.handle), other.map(|o| o.0)].into_iter().flatten() {
        let _ = vfs::close(vfs_tid, h);
    }
    check(
        "creating it exclusively fails",
        vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE | vfs::OPEN_EXCLUSIVE).err() == Some(vfs::ERR_EXISTS),
    );
    check(
        "a file is not a directory",
        vfs::open_with(vfs_tid, file, vfs::OPEN_DIRECTORY).err() == Some(vfs::ERR_NOT_DIR),
    );
    let mut long = [b'y'; 300];
    long[..5].copy_from_slice(b"/tmp/");
    check(
        "a name past 255 bytes is refused",
        vfs::open_with(vfs_tid, &long, vfs::OPEN_CREATE).err() == Some(vfs::ERR_NAME_TOO_LONG),
    );
    // Removing, renaming and shortening, and the directory made above goes.
    let moved: &[u8] = b"/tmp/dtest-a-directory-whose-name-alone-is-past-the-old-limit/renamed";
    let _ = vfs::unlink(vfs_tid, moved);
    check("rename a file", vfs::rename(vfs_tid, file, moved).is_ok());
    check("the old name is gone", vfs::open(vfs_tid, file).err() == Some(vfs::ERR_NOT_FOUND));
    if let Ok(o) = vfs::open_with(vfs_tid, moved, 0) {
        check(
            "shorten it through a handle",
            vfs::truncate(vfs_tid, o.handle, 4).is_ok()
                && vfs::stat_full(vfs_tid, o.handle).is_ok_and(|s| s.size == 4),
        );
        let _ = vfs::close(vfs_tid, o.handle);
    }
    if let Ok(o) = vfs::open_with(vfs_tid, file, vfs::OPEN_CREATE) {
        let _ = vfs::close(vfs_tid, o.handle);
    }
    let replaced = vfs::rename(vfs_tid, moved, file).is_ok()
        && vfs::open_with(vfs_tid, file, 0).is_ok_and(|o| {
            let _ = vfs::close(vfs_tid, o.handle);
            o.size == 4
        });
    check("rename onto a name replaces what had it", replaced);
    // A second name is the same file.
    let _ = vfs::unlink(vfs_tid, moved);
    check("link a second name", vfs::link(vfs_tid, file, moved).is_ok());
    let names = (vfs::open_with(vfs_tid, file, 0), vfs::open_with(vfs_tid, moved, 0));
    if let (Ok(a), Ok(b)) = &names {
        let stats = (vfs::stat_full(vfs_tid, a.handle), vfs::stat_full(vfs_tid, b.handle));
        check(
            "both names are one file with two links",
            matches!(stats, (Ok(x), Ok(y)) if x.id == y.id && x.links == 2 && y.links == 2),
        );
    } else {
        check("both names are one file with two links", false);
    }
    for o in [names.0, names.1].into_iter().flatten() {
        let _ = vfs::close(vfs_tid, o.handle);
    }
    check(
        "a directory has one name",
        vfs::link(vfs_tid, dir, b"/tmp/dtest-dir-link").err() == Some(vfs::ERR_IS_DIR),
    );
    check("and the second name goes", vfs::unlink(vfs_tid, moved).is_ok());
    check(
        "a directory with something in it stays",
        vfs::rmdir(vfs_tid, dir).err() == Some(vfs::ERR_NOT_EMPTY),
    );
    check("unlink a file", vfs::unlink(vfs_tid, file).is_ok());
    check(
        "then the directory can go",
        vfs::rmdir(vfs_tid, dir).is_ok() && vfs::open(vfs_tid, dir).err() == Some(vfs::ERR_NOT_FOUND),
    );

    // A working directory: relative names start there, and a child is given
    // it, or starts at the root.
    check("chdir to /etc", vfs::chdir(vfs_tid, b"/etc").is_ok());
    check(
        "a relative name opens from there",
        vfs::open(vfs_tid, b"passwd").map(|(h, _, _)| vfs::close(vfs_tid, h)).is_ok(),
    );
    let mut here = [0u8; 64];
    check(
        "getcwd says /etc",
        vfs::getcwd(vfs_tid, &mut here).is_ok_and(|n| &here[..n] == b"/etc"),
    );
    let given = load_child(&[b"dchild", b"cwd"]).map(|c| {
        let _ = vfs::give_cwd(vfs_tid, c.tid);
        let _ = c.start();
        wait_for(c.tid)
    });
    check("a child given the directory starts there", given == Some(Some(0)));
    let not_given = load_child(&[b"dchild", b"cwd"]).map(|c| {
        let _ = c.start();
        wait_for(c.tid)
    });
    check("one not given it starts at the root", not_given == Some(Some(1)));
    check(
        "nobody else's child can be given it",
        vfs::give_cwd(vfs_tid, nameserver::NAMESERVER_TID).err() == Some(vfs::ERR_PERMISSION),
    );
    check(
        "a file is not a directory to be in",
        vfs::chdir(vfs_tid, b"/etc/passwd").err() == Some(vfs::ERR_NOT_DIR),
    );
    check("and back to /", vfs::chdir(vfs_tid, b"/").is_ok());

    // A directory read a page at a time, with names longer than a page's
    // fixed entries used to hold.
    const LISTING: &[u8] = b"/tmp/dtest-listing";
    let _ = vfs::mkdir(vfs_tid, LISTING);
    let mut path = [0u8; 160];
    let mut made = 0;
    for i in 0..80 {
        let n = listing_entry(&mut path, LISTING, i);
        if let Ok(o) = vfs::open_with(vfs_tid, &path[..n], vfs::OPEN_CREATE) {
            let _ = vfs::close(vfs_tid, o.handle);
            made += 1;
        }
    }
    check("make 80 files with 100-byte names", made == 80);
    let mut seen = [false; 80];
    let mut listed = 0;
    if let Ok((h, _, _)) = vfs::open(vfs_tid, LISTING) {
        let mut out = [vfs::DirEntry::empty(); 16];
        let mut next = 0u64;
        loop {
            let Ok(page) = vfs::readdir_bulk(vfs_tid, h, next, &mut out) else {
                break;
            };
            for e in &out[..page.count] {
                let name = e.name_bytes();
                if name.len() == 100 && name.starts_with(b"entry-") {
                    let i = ((name[6] - b'0') * 10 + (name[7] - b'0')) as usize;
                    if i < 80 && !seen[i] {
                        seen[i] = true;
                        listed += 1;
                    }
                }
            }
            next = page.next;
            if page.end || page.count == 0 {
                break;
            }
        }
        let _ = vfs::close(vfs_tid, h);
    }
    check("list all of them, sixteen at a time", listed == 80);

    // Space: a 64 KiB file takes it, and gives it back.
    let big: &[u8] = b"/tmp/dtest-listing/big";
    let before = vfs::statfs(vfs_tid).map(|s| s.free_blocks);
    if let Ok(o) = vfs::open_with(vfs_tid, big, vfs::OPEN_CREATE | vfs::OPEN_TRUNCATE) {
        let chunk = [0x5Au8; 4096];
        for i in 0..16u32 {
            let _ = vfs::write(vfs_tid, o.handle, &chunk, i * 4096);
        }
        let _ = vfs::close(vfs_tid, o.handle);
    }
    let during = vfs::statfs(vfs_tid).map(|s| s.free_blocks);
    let _ = vfs::unlink(vfs_tid, big);
    let after = vfs::statfs(vfs_tid).map(|s| s.free_blocks);
    let block = vfs::statfs(vfs_tid).map_or(1024, |s| s.block_size);
    check(
        "a 64 KiB file takes 64 KiB",
        matches!((before, during), (Ok(b), Ok(d)) if b >= d + 65536 / block),
    );
    check("and gives it back when it goes", before.is_ok() && before == after);
    for i in 0..80 {
        let n = listing_entry(&mut path, LISTING, i);
        let _ = vfs::unlink(vfs_tid, &path[..n]);
    }
    check("and the directory empties and goes", vfs::rmdir(vfs_tid, LISTING).is_ok());

    // A program that exits holding files gives them back. Two of these hold
    // more handles between them than the table has room for.
    for _ in 0..2 {
        let Some(child) = load_child(&[b"dchild", b"hold", b"100"]) else {
            check("start a program that holds files", false);
            return;
        };
        let _ = child.start();
        check("it opened a hundred files", wait_for(child.tid) == Some(100));
    }
    let mut held = [0usize; 60];
    let mut n = 0;
    for slot in held.iter_mut() {
        if let Ok((h, _, _)) = vfs::open(vfs_tid, b"/etc/passwd") {
            *slot = h;
            n += 1;
        }
    }
    check("and their handles went when they did", n == 60);
    for &h in &held[..n] {
        let _ = vfs::close(vfs_tid, h);
    }
}

static LOCK: sync::Mutex<u32> = sync::Mutex::new(0);
static COND: sync::Condvar = sync::Condvar::new();
static ONCE: sync::Once = sync::Once::new();
static ONCE_RAN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static SEM: sync::Semaphore = sync::Semaphore::new(0);
static RW: sync::RwLock<u32> = sync::RwLock::new(7);

/// Take the lock, change the value, and say so — after a delay, so the main
/// task is genuinely blocked rather than arriving second.
extern "C" fn sync_worker() -> ! {
    syscall::sleep_ticks(5);
    {
        let mut held = LOCK.lock();
        *held = 99;
        COND.notify_one();
    }
    SEM.release();
    syscall::sys_exit_code(0);
}

fn test_sync() {
    println!("locks:");
    // Uncontended, which is the path that must cost no system call at all.
    {
        let mut held = LOCK.lock();
        *held = 1;
        check("lock and write through it", *held == 1);
        check("try_lock fails while it is held", LOCK.try_lock().is_none());
    }
    check("try_lock succeeds once it is free", LOCK.try_lock().is_some());
    {
        let mut held = LOCK.lock();
        *held = 0;
    }

    ONCE.call_once(|| {
        ONCE_RAN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    });
    ONCE.call_once(|| {
        ONCE_RAN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    });
    check(
        "once runs exactly once",
        ONCE_RAN.load(core::sync::atomic::Ordering::Relaxed) == 1 && ONCE.is_completed(),
    );

    {
        let a = RW.read();
        let b = RW.read();
        check("two readers at once", *a == 7 && *b == 7);
    }
    {
        let mut w = RW.write();
        *w = 8;
    }
    check("a writer changed it", *RW.read() == 8);

    // Contention, which needs a second task: one task cannot both hold a lock
    // and wait for it.
    let Ok(_t) = thread::spawn_with_stack(sync_worker, 8) else {
        check("start a thread to contend with", false);
        return;
    };
    check("start a thread to contend with", true);

    let before = syscall::sys_ticks();
    let mut held = LOCK.lock();
    while *held != 99 {
        held = COND.wait(held);
    }
    let elapsed = syscall::sys_ticks() - before;
    check("condvar woke with the value the other task set", *held == 99);
    // It has to have waited — arriving after the worker had already finished
    // would prove nothing about waiting — but not spun for a whole timeout.
    check("and it waited rather than spun", elapsed >= 3 && elapsed < 200);
    drop(held);

    SEM.acquire();
    check("semaphore permit arrived", true);
    check("and there is not a second one", !SEM.try_acquire());
}

// --- floating-point state across a context switch ---

/// The go-ahead for the worker, and its report that it ran.
static FPU_GO: sync::Semaphore = sync::Semaphore::new(0);
static FPU_RAN: sync::Semaphore = sync::Semaphore::new(0);

/// A different value in each of the sixteen SSE registers, derived from a seed
/// so that two tasks' patterns can never agree by accident.
fn fpu_pattern(seed: u64, reg: u64) -> u64 {
    seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (reg << 56) ^ reg
}

/// Load every SSE register and MXCSR with this task's pattern.
///
/// Assembly because this program, like everything built for
/// x86_64-unknown-none, is compiled soft-float: no Rust statement here touches
/// an SSE register, which is exactly what makes the test deterministic. The only
/// SSE state in the system is what this function and its twin in the worker put
/// there — so if one task sees the other's, the kernel handed it over.
unsafe fn fpu_load(seed: u64, mxcsr: u32) {
    let mut vals = [0u64; 16];
    for (i, v) in vals.iter_mut().enumerate() {
        *v = fpu_pattern(seed, i as u64);
    }
    let m = mxcsr;
    unsafe {
        core::arch::asm!(
            "movq xmm0,  [{v} + 0*8]",
            "movq xmm1,  [{v} + 1*8]",
            "movq xmm2,  [{v} + 2*8]",
            "movq xmm3,  [{v} + 3*8]",
            "movq xmm4,  [{v} + 4*8]",
            "movq xmm5,  [{v} + 5*8]",
            "movq xmm6,  [{v} + 6*8]",
            "movq xmm7,  [{v} + 7*8]",
            "movq xmm8,  [{v} + 8*8]",
            "movq xmm9,  [{v} + 9*8]",
            "movq xmm10, [{v} + 10*8]",
            "movq xmm11, [{v} + 11*8]",
            "movq xmm12, [{v} + 12*8]",
            "movq xmm13, [{v} + 13*8]",
            "movq xmm14, [{v} + 14*8]",
            "movq xmm15, [{v} + 15*8]",
            "ldmxcsr [{m}]",
            v = in(reg) vals.as_ptr(),
            m = in(reg) &m as *const u32,
            options(nostack, preserves_flags),
        );
    }
}

/// Read every SSE register and MXCSR back.
unsafe fn fpu_read() -> ([u64; 16], u32) {
    let mut vals = [0u64; 16];
    let mut m: u32 = 0;
    unsafe {
        core::arch::asm!(
            "movq [{v} + 0*8],  xmm0",
            "movq [{v} + 1*8],  xmm1",
            "movq [{v} + 2*8],  xmm2",
            "movq [{v} + 3*8],  xmm3",
            "movq [{v} + 4*8],  xmm4",
            "movq [{v} + 5*8],  xmm5",
            "movq [{v} + 6*8],  xmm6",
            "movq [{v} + 7*8],  xmm7",
            "movq [{v} + 8*8],  xmm8",
            "movq [{v} + 9*8],  xmm9",
            "movq [{v} + 10*8], xmm10",
            "movq [{v} + 11*8], xmm11",
            "movq [{v} + 12*8], xmm12",
            "movq [{v} + 13*8], xmm13",
            "movq [{v} + 14*8], xmm14",
            "movq [{v} + 15*8], xmm15",
            "stmxcsr [{m}]",
            v = in(reg) vals.as_mut_ptr(),
            m = in(reg) &mut m as *mut u32,
            options(nostack, preserves_flags),
        );
    }
    (vals, m)
}

/// Round toward zero, all exceptions masked — distinct from the default
/// (round to nearest) so that a lost MXCSR is visible too.
const FPU_MXCSR_MAIN: u32 = 0x7F80;
/// Round down, all exceptions masked.
const FPU_MXCSR_WORKER: u32 = 0x3F80;

extern "C" fn fpu_worker() -> ! {
    FPU_GO.acquire();
    // The main task's pattern is loaded by now and it is asleep. Overwrite
    // every register with a different one; without a per-task save area, this
    // is what the main task will find when it wakes.
    unsafe { fpu_load(0xB0B, FPU_MXCSR_WORKER) };
    FPU_RAN.release();
    syscall::sys_exit_code(0);
}

fn test_fpu() {
    println!("floating-point state:");
    // A new task starts from a clean state rather than whatever the last task
    // left: MXCSR is the power-on default. Anything else is one task reading
    // another's.
    let (_, fresh) = unsafe { fpu_read() };
    check("a task starts with the default MXCSR", fresh & 0xFFC0 == 0x1F80);

    let Ok(_t) = thread::spawn_with_stack(fpu_worker, 8) else {
        check("start a task to share the SSE registers with", false);
        return;
    };
    unsafe { fpu_load(0xA11CE, FPU_MXCSR_MAIN) };
    FPU_GO.release();
    // Blocks, so the worker runs and loads its own pattern.
    FPU_RAN.acquire();
    let (vals, m) = unsafe { fpu_read() };

    let mut intact = true;
    for (i, v) in vals.iter().enumerate() {
        if *v != fpu_pattern(0xA11CE, i as u64) {
            intact = false;
        }
    }
    check("every SSE register survives another task using them", intact);
    check("and so does MXCSR", m == FPU_MXCSR_MAIN);
    check(
        "none of them is the other task's",
        vals[0] != fpu_pattern(0xB0B, 0),
    );
}

fn test_wire() {
    println!("wayland wire format:");
    // wl_display.get_registry as libwayland actually sent it down a Quark
    // socketpair: object 1, opcode 1, size 12, one new_id argument of 2.
    // Twelve real bytes rather than twelve invented ones.
    let msg: [u8; 12] = [
        0x01, 0x00, 0x00, 0x00, // object 1
        0x01, 0x00, 0x0C, 0x00, // opcode 1, size 12
        0x02, 0x00, 0x00, 0x00, // new_id 2
    ];
    let h = wire::parse_header(&msg);
    check("a header parses", h.is_some());
    let Some(h) = h else { return };
    check("object is 1", h.object == 1);
    check("opcode is 1", h.opcode == 1);
    check("size is 12", h.size == 12);
    check("the argument is 2", wire::get_u32(&msg, 8) == Some(2));

    let mut out = [0u8; 12];
    wire::put_header(&mut out, wire::Header { object: 1, opcode: 1, size: 12 });
    wire::put_u32(&mut out, 8, 2);
    check("a header we write is the one libwayland wrote", out == msg);

    check("a truncated header is refused", wire::parse_header(&msg[..7]).is_none());
    // A size that does not cover its own header would advance a read cursor
    // by less than nothing.
    let bad: [u8; 8] = [1, 0, 0, 0, 1, 0, 4, 0];
    check("a size smaller than a header is refused", wire::parse_header(&bad).is_none());

    let mut sb = [0u8; 16];
    let n = wire::put_str(&mut sb, 0, b"wl_shm");
    check("a string is length, bytes, NUL, padding", n == 12);
    check("its length includes the NUL", wire::get_u32(&sb, 0) == Some(7));
    check(
        "and it reads back",
        wire::get_str(&sb, 0).map(|(s, _)| s) == Some(&b"wl_shm"[..]),
    );
    check("padding rounds up to four", wire::pad4(7) == 8 && wire::pad4(8) == 8);
}

/// Call after call for three seconds, each one handing the processor straight
/// to the task called.
///
/// The hand-over marks the callee runnable without queueing it, since it is
/// about to run, and then switches to it. Interrupts were back on in between,
/// and a tick there that preempted the caller -- already blocked on the callee
/// -- switched to something else and left the callee in no queue, runnable and
/// never run, with its caller waiting on it for ever. Any task busy with calls
/// could hit it; fontconfig scanning fonts did, once a minute or so. Every call
/// here has a deadline, so that shows up as a failure rather than a hang.
fn test_call_storm() {
    use quark_rt::ipc::Message;
    println!("calls:");
    let Some(child) = load_child(&[b"dchild", b"echo"]) else {
        check("start a child to call", false);
        return;
    };
    if child.start().is_err() || !mint_endpoint(STORM_SLOT, child.tid) {
        check("start a child to call", false);
        let _ = syscall::sys_task_kill(child.tid);
        let _ = wait_for(child.tid);
        return;
    }
    let start = syscall::sys_ticks();
    let mut calls = 0u64;
    let mut answered = true;
    let mut reply = Message::empty();
    while syscall::sys_ticks() - start < 300 {
        calls += 1;
        let ask = Message { sender: 0, tag: calls, data: [0; 6] };
        let outcome = syscall::sys_call_timeout(child.tid, &ask, &mut reply, 100);
        if !matches!(outcome, syscall::CallOutcome::Replied) || reply.tag != calls + 1 {
            answered = false;
            break;
        }
    }
    println!("        {} calls in {} ticks", calls, syscall::sys_ticks() - start);
    check("every call is answered, however many", answered && calls >= 1000);
    let stop = Message::empty();
    let stopped = matches!(
        syscall::sys_call_timeout(child.tid, &stop, &mut reply, 100),
        syscall::CallOutcome::Replied
    );
    if !stopped {
        let _ = syscall::sys_task_kill(child.tid);
    }
    check("and the child is still there to stop", stopped && wait_for(child.tid) == Some(0));
    let _ = syscall::sys_cap_delete(STORM_SLOT);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Every section by default; `dtest NAME` runs just that one, whose output
    // then fits on a screen.
    const SECTIONS: &[(&str, fn())] = &[
        ("physical", test_physical_authority),
        ("close", test_close),
        ("fds", test_fd_table),
        ("region", test_big_region),
        ("memfd", test_memfd),
        ("socketpair", test_socketpair),
        ("passing", test_fd_passing),
        ("leak", test_no_leak),
        ("pollset", test_pollset),
        ("wake", test_wake_latency),
        ("poll", test_poll),
        ("environment", test_environment),
        ("spaces", test_across_address_spaces),
        ("spawn", test_spawned_memory),
        ("lend", test_lent_buffers),
        ("endpoints", test_endpoint_objects),
        ("calls", test_call_storm),
        ("service", test_runtime_service),
        ("random", test_random),
        ("locks", test_locks),
        ("memory", test_memory),
        ("files", test_files),
        ("sync", test_sync),
        ("fpu", test_fpu),
        ("wire", test_wire),
    ];
    println!("[dtest] kernel and runtime checks");
    let only = quark_rt::args::argv(1);
    for (name, section) in SECTIONS {
        if only.is_none_or(|o| o == name.as_bytes()) {
            section();
        }
    }

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
