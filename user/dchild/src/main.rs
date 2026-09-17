#![no_std]
#![no_main]

//! The other half of `dtest`'s cross-process check.
//!
//! Started by `dtest` with one end of a socketpair already at descriptor 3.
//! Allocates memory, writes a witness into it, and sends the descriptor back —
//! which is what a Wayland client does with `wl_shm`, minus the drawing.
//!
//! Given `quit` it only exits, for counting how many programs a parent can
//! run; given `orphan` it leaves a dead thread behind for the parent to check
//! on; given `serve` it answers one call with 42, and given `register NAME` it
//! does that under a name. `lookup NAME` calls whatever has that name and exits
//! with the answer. `hold N` opens a file N times and exits without closing
//! any of them, saying how many it got. `echo` answers every call with its
//! tag plus one, until a call whose tag is 0. `cwd` exits 0 if the relative
//! name `passwd` opens: whoever started it gave it `/etc` as its directory.
//! `lock PATH` locks the whole file, says so on descriptor 3, and holds it
//! until that closes; `lock2 PATH` locks byte 1, says so, waits for byte 0,
//! and says so again once it has it. `unlinked PATH` makes a file, removes it
//! while holding it open, and waits for ever: stopping the machine then is a
//! crash with an orphan on the disk. `hog` reserves four gigabytes and
//! touches them until something stops it.

use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::manifest::CapReq;
use quark_rt::{nameserver, println, sync, syscall, thread, vfs};

quark_rt::manifest!([CapReq::phys_alloc(16)]);

const CONN: usize = 3;
const MINE: usize = 0x97_0000_0000;
const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;
/// Where the verdict on the capability test goes, in the memory both halves
/// share. Reported this way rather than over the stream because the stream's
/// message boundaries are what the other half is asserting about.
const VERDICT: usize = MINE + 128;
/// A CSpace slot of this child's own, well clear of anything the manifest
/// filled, to mint into.
const SCRATCH: usize = 8;
/// A slot in somebody else's CSpace to try to fill.
const VICTIM_SLOT: usize = 14;
/// `sys_task_info`'s state for a task that has exited.
const DEAD: u8 = 3;

extern "C" fn quit() -> ! {
    syscall::sys_exit_code(0);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Asked only to run: the parent is counting how many programs it can
    // start, not talking to this one.
    if quark_rt::args::argv(1) == Some(&b"quit"[..]) {
        syscall::sys_exit_code(0);
    }
    // Leave a thread behind, dead but never collected, and say which one: the
    // parent checks that collecting this program takes the thread with it.
    if quark_rt::args::argv(1) == Some(&b"orphan"[..]) {
        let Ok(t) = thread::spawn_with_stack(quit, 1) else {
            syscall::sys_exit_code(-1);
        };
        // sys_wait would reap it, so watch for it to die instead.
        while syscall::sys_task_info(t.tid()).map(|(state, _, _)| state) != Ok(DEAD) {
            syscall::sleep_ticks(1);
        }
        syscall::sys_exit_code(t.tid() as i32);
    }

    if let Some(mode @ (b"lock" | b"lock2")) = quark_rt::args::argv(1) {
        let path = quark_rt::args::argv(2).unwrap_or(b"");
        let held = nameserver::lookup_retry(b"vfs", 20).and_then(|vfs| {
            let (h, _, _) = vfs::open(vfs, path).ok()?;
            if mode == b"lock" {
                vfs::lock(vfs, h, vfs::LOCK_EXCLUSIVE, 0, 0, vfs::LOCK_WAIT).ok()?;
                let _ = syscall::sys_fd_write(CONN, b"L");
            } else {
                vfs::lock(vfs, h, vfs::LOCK_EXCLUSIVE, 1, 1, 0).ok()?;
                let _ = syscall::sys_fd_write(CONN, b"1");
                vfs::lock(vfs, h, vfs::LOCK_EXCLUSIVE, 0, 1, vfs::LOCK_WAIT).ok()?;
                let _ = syscall::sys_fd_write(CONN, b"2");
            }
            Some(())
        });
        // Held until the parent lets go of its end.
        let mut buf = [0u8; 1];
        while held.is_some() && matches!(syscall::sys_fd_read(CONN, &mut buf), 1..=0xFFFF) {}
        syscall::sys_exit_code(if held.is_some() { 0 } else { 1 });
    }
    if quark_rt::args::argv(1) == Some(&b"hog"[..]) {
        const HOG: usize = 0xB0_0000_0000;
        if syscall::sys_map_anon(HOG, 1 << 20, false).is_err() {
            syscall::sys_exit_code(1);
        }
        for page in 0..1usize << 20 {
            unsafe { core::ptr::write_volatile((HOG + page * 4096) as *mut u8, 1) };
        }
        // Four gigabytes, and nobody stopped it.
        syscall::sys_exit_code(2);
    }
    if quark_rt::args::argv(1) == Some(&b"unlinked"[..]) {
        let path = quark_rt::args::argv(2).unwrap_or(b"");
        let held = nameserver::lookup_retry(b"vfs", 20).and_then(|vfs| {
            let o = vfs::open_with(vfs, path, vfs::OPEN_CREATE | vfs::OPEN_TRUNCATE).ok()?;
            let data = [0x5Au8; 1000];
            for i in 0..5 {
                vfs::write(vfs, o.handle, &data, i * 1000).ok()?;
            }
            vfs::unlink(vfs, path).ok()?;
            Some(o.handle)
        });
        match held {
            Some(_) => println!("holding {}", core::str::from_utf8(path).unwrap_or("?")),
            None => syscall::sys_exit_code(1),
        }
        loop {
            let mut msg = Message::empty();
            let _ = syscall::sys_recv(TID_ANY, &mut msg);
        }
    }
    if quark_rt::args::argv(1) == Some(&b"cwd"[..]) {
        let found = nameserver::lookup_retry(b"vfs", 20).is_some_and(|vfs| {
            vfs::open(vfs, b"passwd").map(|(h, _, _)| vfs::close(vfs, h)).is_ok()
        });
        syscall::sys_exit_code(if found { 0 } else { 1 });
    }

    // Answer one call, whoever makes it, with 42: something for the parent to
    // reach, or to fail to reach.
    if quark_rt::args::argv(1) == Some(&b"serve"[..]) {
        serve_once();
    }
    if quark_rt::args::argv(1) == Some(&b"register"[..]) {
        let name = quark_rt::args::argv(2).unwrap_or(b"");
        if nameserver::register(name).is_err() {
            syscall::sys_exit_code(2);
        }
        serve_once();
    }
    if quark_rt::args::argv(1) == Some(&b"hold"[..]) {
        let want = quark_rt::args::argv(2).map_or(0, |n| {
            n.iter().fold(0usize, |acc, &d| acc * 10 + (d.wrapping_sub(b'0') as usize % 10))
        });
        let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
            syscall::sys_exit_code(-1);
        };
        let mut held = 0;
        for _ in 0..want {
            if vfs::open(vfs_tid, b"/etc/passwd").is_ok() {
                held += 1;
            }
        }
        syscall::sys_exit_code(held);
    }
    // Answer call after call, for a parent making a great many of them.
    if quark_rt::args::argv(1) == Some(&b"echo"[..]) {
        loop {
            let mut msg = Message::empty();
            if syscall::sys_recv_timeout(TID_ANY, &mut msg, 500).is_err() {
                syscall::sys_exit_code(1);
            }
            let answer = Message { sender: 0, tag: msg.tag.wrapping_add(1), data: [0; 6] };
            let _ = syscall::sys_reply(msg.sender, &answer);
            if msg.tag == 0 {
                syscall::sys_exit_code(0);
            }
        }
    }
    // Reach a service by name alone: nothing but the lookup gives this the
    // right to call it.
    if quark_rt::args::argv(1) == Some(&b"lookup"[..]) {
        let name = quark_rt::args::argv(2).unwrap_or(b"");
        let Some(tid) = nameserver::lookup(name) else {
            syscall::sys_exit_code(2);
        };
        let mut reply = Message::empty();
        match syscall::sys_call_timeout(tid, &Message::empty(), &mut reply, 100) {
            syscall::CallOutcome::Replied => syscall::sys_exit_code(reply.tag as i32),
            _ => syscall::sys_exit_code(3),
        }
    }

    // Wait for the parent's byte before answering, so this proves the stream
    // carries data in both directions between address spaces.
    let mut buf = [0u8; 8];
    let n = syscall::sys_fd_read(CONN, &mut buf);
    if n != 4 || &buf[..4] != b"go!\n" {
        println!("[dchild] bad greeting: {} bytes", n);
        syscall::sys_exit_code(2);
    }

    let Ok(mem) = syscall::sys_memfd_create(2) else {
        println!("[dchild] no memory");
        syscall::sys_exit_code(3);
    };
    if syscall::sys_mmap_fd(mem, MINE).is_err() {
        println!("[dchild] cannot map my own memory");
        syscall::sys_exit_code(4);
    }
    unsafe { core::ptr::write_volatile(MINE as *mut u64, WITNESS) };

    // Can this child push a capability into a task it has no authority over?
    //
    // It holds no TaskMgmt at all — its manifest asks for phys_alloc and
    // nothing else — and its parent is not calling it, so the answer must be
    // no. A grant can never *raise* anyone's authority, since it only ever
    // adds; what it can do is fill every slot, and a service that can no
    // longer be handed a capability can no longer be handed the display.
    //
    // An Endpoint to itself is a capability any task may mint, which is what
    // makes this test about the grant rather than about the mint.
    let me = syscall::sys_getpid() as usize;
    let parent = syscall::sys_task_info(me).map(|(_, p, _)| p).unwrap_or(0);
    let minted = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_ENDPOINT, me as u64, 0).is_ok();
    let refused = syscall::sys_cap_grant(parent, SCRATCH, VICTIM_SLOT).is_err();
    unsafe {
        core::ptr::write_volatile(VERDICT as *mut u64, (minted && refused) as u64);
    }

    if syscall::sys_fd_send(CONN, b"here", Some(mem)) != Ok(4) {
        println!("[dchild] send failed");
        syscall::sys_exit_code(5);
    }

    // A lock in memory the two of us share. The parent holds it when this
    // arrives, so acquiring it means blocking in one address space and being
    // woken from another — which works because the kernel keys its wait queue
    // on the physical address of the word, not the virtual one.
    let shared = unsafe { &*((MINE + 64) as *const sync::Mutex<u64>) };
    {
        let mut held = shared.lock();
        *held += 1;
    }
    println!("[dchild] sent, and took the shared lock");
    syscall::sys_exit_code(0);
}

/// Answer one call, from anybody, with 42, and exit. Waits five seconds at
/// most, so a parent whose caller never came is not left waiting for good.
fn serve_once() -> ! {
    let mut msg = Message::empty();
    if syscall::sys_recv_timeout(TID_ANY, &mut msg, 500).is_err() {
        syscall::sys_exit_code(1);
    }
    let _ = syscall::sys_reply(msg.sender, &Message { sender: 0, tag: 42, data: [0; 6] });
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dchild] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
