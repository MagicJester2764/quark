#![no_std]
#![no_main]

//! Phase 10 acceptance: descriptors, streams, waiting and the environment.
//!
//! There is no test framework here, so this is one: a program that asserts and
//! exits non-zero. Run it from the shell, or read its output on the serial
//! line. Each section corresponds to one task of the Phase 10 plan.

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
fn load_child() -> Option<spawn::Spawned> {
    let vfs_tid = nameserver::lookup_retry(b"vfs", 20)?;
    // Lowercase for ext2, uppercase with .ELF for FAT32 — the two spellings
    // the shell already tries.
    let (handle, size, _) = match vfs::open(vfs_tid, b"/usr/bin/dchild") {
        Ok(h) => h,
        Err(_) => vfs::open(vfs_tid, b"/usr/bin/DCHILD.ELF").ok()?,
    };
    let size = size as usize;
    let pages = (size + 4095) / 4096;
    for p in 0..pages {
        let frame = syscall::sys_phys_alloc(1).ok()?;
        syscall::sys_map_phys(frame, CHILD_IMAGE + p * 4096, 1).ok()?;
        let want = 4096.min(size - p * 4096) as u32;
        vfs::read(vfs_tid, handle, frame, (p * 4096) as u32, want).ok()?;
    }
    let _ = vfs::close(vfs_tid, handle);

    let image = unsafe { core::slice::from_raw_parts(CHILD_IMAGE as *const u8, size) };
    let info = spawn::load(image, &SPAWN_SCRATCH).ok()?;
    quark_rt::manifest::grant_image(info.tid, image, 12);
    // It needs to be able to reach the nameserver, and somewhere to print.
    let _ = syscall::sys_cap_grant(info.tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);
    Some(info)
}

fn test_across_address_spaces() {
    println!("across address spaces:");
    let (mine, theirs) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let Some(info) = load_child() else {
        check("load /usr/bin/dchild", false);
        return;
    };
    check("load /usr/bin/dchild", true);

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
    test_pollset();
    test_wake_latency();
    test_poll();
    test_environment();
    test_across_address_spaces();
    test_sync();
    test_wire();

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
