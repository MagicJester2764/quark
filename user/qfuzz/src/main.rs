#![no_std]
#![no_main]

//! Random requests to every service, and a check that each survives them.
//!
//! `qfuzz ROUNDS [SEED]` sends each service ROUNDS requests made from SEED,
//! or from the kernel's generator without one: tags from the service's own
//! protocol, from the range the kernel's notices use, and from anywhere; data
//! words small, handle-sized, huge or random; with and without a buffer lent,
//! and with and without a capability offered. Each request has half a second
//! to be answered. Then the service must still be running, answer a ping
//! within a second, and still do its job. The exit status is how many
//! services did not.
//!
//! Some requests would do harm that is not the service's fault, and are
//! steered away from it:
//!
//! - What is lent to the VFS never holds `/` or `.`, since a request may take
//!   any part of it as a path. Every name the VFS resolves is then under
//!   `/tmp/qfuzz`, where this program works, and nothing climbs out of it.
//! - Addresses given to the network server are 10.0.2.99, which QEMU's network
//!   does not answer, so nothing leaves the machine. Name lookups are not
//!   asked for at all: the host would make them.
//! - A driver is fuzzed only once it has refused a harmless request. One that
//!   answers anybody would be writing random sectors, or taking keys.

use quark_rt::ipc::{Message, TAG_PING};
use quark_rt::syscall::{self, CallOutcome, CallWith, LEND_READ, LEND_WRITE};
use quark_rt::{args, nameserver, print, println, vfs};

/// xorshift64*: small, fast, and the same sequence for the same seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // splitmix64 once, so that small seeds do not start out mostly zero,
        // and never zero, which xorshift cannot leave.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Nameserver,
    Vfs,
    Fb,
    Console,
    Input,
    Net,
    Disk,
    Keyboard,
}

struct Target {
    name: &'static [u8],
    kind: Kind,
    /// The tags of its own protocol.
    tags: &'static [u64],
}

const TARGETS: &[Target] = &[
    Target { name: b"nameserver", kind: Kind::Nameserver, tags: &[0, 1, 2, 3, 4] },
    Target {
        name: b"vfs",
        kind: Kind::Vfs,
        tags: &[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
        ],
    },
    Target { name: b"fb", kind: Kind::Fb, tags: &[0, 1, 2, 3, 100, 0x100, 0x101] },
    Target { name: b"console", kind: Kind::Console, tags: &[0, 1, 2, 0x100, 0x101] },
    Target {
        name: b"input",
        kind: Kind::Input,
        tags: &[0, 1, 2, 3, 0x200, 0x201, 0x202, 0x203, 0x204, 0x205, 0x206],
    },
    Target {
        name: b"net",
        kind: Kind::Net,
        tags: &[0, 1, 2, 3, 4, 5, 6, 8, 10, 11, 12, 13, 14, 15, 16, 17],
    },
    Target { name: b"disk", kind: Kind::Disk, tags: &[0, 1, 2, 3, 4, 5, 6] },
    Target { name: b"keyboard", kind: Kind::Keyboard, tags: &[0, 1, 2, 3, 4, 5, 6, 7, 8] },
];

const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

/// How long a request has to be answered, and a ping.
const REQUEST_TICKS: u64 = 50;
const PING_TICKS: u64 = 100;

/// Where this program's own endpoint is kept, to offer.
const SELF_SLOT: usize = 10;
/// Where the framebuffer device grants the display to a claimant.
const FB_LEASE_SLOT: usize = 2;

/// What is lent with a request.
const BUF_LEN: usize = 1 << 16;
static mut BUF: [u8; BUF_LEN] = [0; BUF_LEN];

/// Nobody answers here on QEMU's network: 10.0.2.99, packed big-endian. On
/// the machine's own subnet, so nothing is routed out to find it.
const NOWHERE: u64 = 0x0A00_0263;
/// The network's own host, which answers pings: 10.0.2.2.
const GATEWAY: u64 = 0x0A00_0202;

/// The VFS requests whose first word is a handle, and those whose first word
/// is the length of a path lent with the call. Sending a handle where one
/// belongs is what gets past the first check and into the code that reads and
/// writes files; a random number is turned away at the door.
const VFS_HANDLE_TAGS: &[u64] = &[2, 3, 5, 6, 8, 13, 19, 22, 23];
const VFS_PATH_TAGS: &[u64] = &[1, 9, 10, 11, 12, 15, 16, 17, 18];

/// Handles this program opened under `/tmp/qfuzz`, for the requests that take
/// one. They are closed, reopened and ruined as the fuzzing goes on, which is
/// the point of giving them out.
static mut HANDLES: [u64; 8] = [0; 8];
static mut NHANDLES: usize = 0;

fn handles() -> &'static [u64] {
    unsafe {
        let all: &'static [u64; 8] = &*core::ptr::addr_of!(HANDLES);
        &all[..NHANDLES]
    }
}

/// Open a few files and the directory to work in, so that the requests that
/// take a handle have one.
fn open_some(rng: &mut Rng, vfs_tid: usize) {
    let mut n = 0;
    if let Ok(dir) = vfs::open_with(vfs_tid, b".", vfs::OPEN_DIRECTORY) {
        unsafe { HANDLES[n] = dir.handle as u64 };
        n += 1;
    }
    for _ in 0..7 {
        // A name of this run's own, so that two runs do not fight over one.
        let mut name = [0u8; 12];
        name[..6].copy_from_slice(b"qfuzz-");
        let mut len = 6;
        for _ in 0..5 {
            name[len] = rng.pick(b"abcdefghijklmnopqrstuvwxyz");
            len += 1;
        }
        let flags = vfs::OPEN_CREATE | if rng.below(4) == 0 { vfs::OPEN_TRUNCATE } else { 0 };
        if let Ok(f) = vfs::open_with(vfs_tid, &name[..len], flags) {
            let _ = vfs::write(vfs_tid, f.handle, b"qfuzz", 0);
            if n < 8 {
                unsafe { HANDLES[n] = f.handle as u64 };
                n += 1;
            }
        }
    }
    unsafe { NHANDLES = n };
}

/// The network server's tags whose words name an address, and where.
const NET_UDP_SEND: u64 = 1;
const NET_INFO: u64 = 4;
const NET_ICMP_PING: u64 = 5;
const NET_DNS_RESOLVE: u64 = 7;
const NET_TCP_CONNECT: u64 = 10;
const NET_TCP_CLOSE: u64 = 15;
const NET_SOCK_WRITE: u64 = 16;
const NET_SOCK_READ: u64 = 17;

fn tag(rng: &mut Rng, own: &[u64]) -> u64 {
    match rng.below(8) {
        0..=3 => rng.pick(own),
        // The kernel's notices, and a ping now and then.
        4 => 0xFFFF_0000 + rng.below(16),
        5 => rng.below(64),
        6 => rng.pick(&[TAG_PING, u64::MAX, 1 << 63, 0x1_0000_0001]),
        _ => rng.next(),
    }
}

fn word(rng: &mut Rng) -> u64 {
    match rng.below(10) {
        0..=2 => rng.below(16),
        3 | 4 => rng.below(256),
        5 => rng.pick(&[u64::MAX, u64::MAX - 1, 1 << 63, 1 << 32, 0xFFFF_FFFF, 4096]),
        6 => 1 << rng.below(64),
        7 => rng.below(1 << 20),
        _ => rng.next(),
    }
}

/// Bytes a VFS request may take as a name: no `/`, no `.`.
fn name_byte(rng: &mut Rng) -> u8 {
    match rng.below(8) {
        0 => 0,
        1 => 0x80 + rng.below(0x80) as u8,
        2 => rng.pick(b"_- \n~"),
        _ => rng.pick(b"abcdefghijklmnopqrstuvwxyz0123456789"),
    }
}

/// How much to lend: nothing, a little, about a page, or a lot.
fn lend_len(rng: &mut Rng) -> usize {
    match rng.below(6) {
        0 | 1 => 0,
        2 => 1 + rng.below(32) as usize,
        3 => 1 + rng.below(512) as usize,
        4 => 4090 + rng.below(12) as usize,
        _ => 1 + rng.below(BUF_LEN as u64) as usize,
    }
}

/// Keep a request from doing harm that would not be the service's fault, and
/// aim it where the service has code to reach.
fn steer(kind: Kind, rng: &mut Rng, tag: &mut u64, data: &mut [u64; 6]) {
    if kind == Kind::Vfs {
        // A path's base directory: mostly the working one, since a random
        // handle turns nearly every path request away before it is read.
        for w in &mut data[4..] {
            if rng.below(4) != 0 {
                *w = 0;
            }
        }
        // And the first word, which is a handle or a path's length depending
        // on the request.
        if rng.below(4) != 0 && !handles().is_empty() {
            if VFS_HANDLE_TAGS.contains(tag) {
                data[0] = rng.pick(handles());
            } else if VFS_PATH_TAGS.contains(tag) {
                data[0] = 1 + rng.below(40);
            }
        }
        return;
    }
    if kind != Kind::Net {
        return;
    }
    let op = *tag & 0xFFFF_FFFF;
    if op == NET_DNS_RESOLVE {
        *tag = NET_INFO;
    }
    match op {
        NET_UDP_SEND => data[2] = NOWHERE,
        NET_ICMP_PING | NET_TCP_CONNECT => data[0] = NOWHERE,
        // A connection handle rides above the operation.
        NET_SOCK_WRITE | NET_SOCK_READ if rng.below(2) == 0 => *tag = op | (rng.below(10) << 32),
        _ => {}
    }
}

#[derive(Default, Clone, Copy)]
struct Tally {
    answered: u32,
    timed_out: u32,
    /// Requests the kernel would not make at all.
    failed: u32,
}

fn fuzz(rng: &mut Rng, t: &Target, tid: usize, rounds: u32) -> Tally {
    let mut tally = Tally::default();
    let fb = if t.kind == Kind::Console { nameserver::lookup(b"fb") } else { None };
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
    for _ in 0..rounds {
        let mut tag = tag(rng, t.tags);
        let mut data = [0u64; 6];
        for w in data.iter_mut() {
            *w = word(rng);
        }
        steer(t.kind, rng, &mut tag, &mut data);

        let mut with = CallWith { ticks: REQUEST_TICKS, ..CallWith::PLAIN };
        let len = lend_len(rng);
        if len > 0 {
            for b in buf[..len].iter_mut() {
                *b = if t.kind == Kind::Vfs { name_byte(rng) } else { rng.next() as u8 };
            }
            let access = rng.pick(&[LEND_READ, LEND_WRITE, LEND_READ | LEND_WRITE]);
            with.buf = buf.as_ptr() as u64;
            with.len_access = len as u64 | access;
        }
        with.offer = match rng.below(10) {
            0..=2 => SELF_SLOT as u64,
            3 => syscall::SLOT_ENDPOINT as u64,
            _ => u64::MAX,
        };

        // The console's input is its pipe as much as its requests, and it
        // has to keep up with the display coming and going under it.
        if t.kind == Kind::Console && rng.below(16) == 0 {
            if let Some(fb) = fb {
                if rng.below(2) == 0 {
                    let claim = Message { sender: 0, tag: 2, data: [0; 6] };
                    let mut reply = Message::empty();
                    let offer = CallWith { offer: SELF_SLOT as u64, ticks: PING_TICKS, ..CallWith::PLAIN };
                    let _ = syscall::sys_call_with(fb, &claim, &mut reply, &offer);
                } else {
                    let_go(Kind::Fb, fb);
                }
            }
        }
        if t.kind == Kind::Console && rng.below(4) == 0 {
            let mut junk = [0u8; 64];
            let n = 1 + rng.below(64) as usize;
            for b in junk[..n].iter_mut() {
                *b = match rng.below(6) {
                    0 => 0x1B,
                    1 => b'[',
                    2 => rng.pick(b"0123456789;"),
                    3 => rng.pick(b"ABCDHJKmhl?"),
                    4 => rng.next() as u8,
                    _ => rng.pick(b"qfuzz \n\t\x08\r"),
                };
            }
            let _ = syscall::sys_fd_write(1, &junk[..n]);
        }

        let msg = Message { sender: 0, tag, data };
        let mut reply = Message::empty();
        match syscall::sys_call_with(tid, &msg, &mut reply, &with) {
            CallOutcome::Replied => tally.answered += 1,
            CallOutcome::TimedOut => tally.timed_out += 1,
            CallOutcome::Failed => tally.failed += 1,
        }
    }
    tally
}

fn call(tid: usize, tag: u64, data: [u64; 6], ticks: u64) -> Option<Message> {
    let msg = Message { sender: 0, tag, data };
    let mut reply = Message::empty();
    match syscall::sys_call_timeout(tid, &msg, &mut reply, ticks) {
        CallOutcome::Replied => Some(reply),
        _ => None,
    }
}

fn refused(tid: usize, tag: u64) -> bool {
    call(tid, tag, [0; 6], PING_TICKS).is_some_and(|r| r.tag == TAG_ERROR)
}

/// This program's address, as the network server reports it.
fn net_address(tid: usize) -> Option<u64> {
    call(tid, NET_INFO, [0; 6], PING_TICKS).filter(|r| r.tag == TAG_OK).map(|r| r.data[1])
}

/// Give back whatever a request was granted: the display, the keyboard,
/// connections.
fn let_go(kind: Kind, tid: usize) {
    match kind {
        Kind::Fb => {
            let _ = call(tid, 3, [0; 6], PING_TICKS);
            let _ = syscall::sys_cap_delete(FB_LEASE_SLOT);
        }
        Kind::Input => {
            let _ = call(tid, 0x201, [0; 6], PING_TICKS);
        }
        Kind::Net => {
            for handle in 0..8 {
                let _ = call(tid, NET_TCP_CLOSE, [handle, 0, 0, 0, 0, 0], PING_TICKS);
            }
        }
        _ => {}
    }
}

/// What the network could do before the fuzzing, so that afterwards it is held
/// to that and no more: a machine without the host to ping is no failure.
#[derive(Clone, Copy, Default)]
struct NetBefore {
    address: Option<u64>,
    pinged: bool,
}

/// Ping the network's host. The reply's tag and first word, if one came.
fn ping_gateway(tid: usize) -> Option<(u64, u64)> {
    let ping = [GATEWAY, 0x5146, 1, 0, 0, 0];
    call(tid, NET_ICMP_PING, ping, 400).map(|r| (r.tag, r.data[0]))
}

/// What a failed check says, and a number to go with it.
type Problem = (&'static str, u64);

/// Whether the service still does what it is for.
fn job(kind: Kind, tid: usize, net_before: NetBefore) -> Result<(), Problem> {
    let fail = |what: &'static str, bad: bool| if bad { Err((what, 0)) } else { Ok(()) };
    match kind {
        Kind::Nameserver => fail("cannot find the VFS", nameserver::lookup(b"vfs").is_none()),
        Kind::Vfs => {
            let mut buf = [0u8; 16];
            let read = vfs::open(tid, b"/etc/passwd").and_then(|(h, _, _)| {
                let n = vfs::read(tid, h, &mut buf, 0);
                let _ = vfs::close(tid, h);
                n
            });
            match read {
                Ok(n) if n > 0 => Ok(()),
                Ok(_) => Err(("reads nothing from /etc/passwd", 0)),
                Err(code) => Err(("cannot read /etc/passwd", code)),
            }
        }
        Kind::Fb => {
            let info = call(tid, 1, [0; 6], PING_TICKS);
            fail("has no mode", !info.is_some_and(|r| r.tag == TAG_OK && r.data[0] != 0))
        }
        Kind::Console => {
            let line = b"qfuzz: \x1b[0mthe console is still here\n";
            fail("cannot print", syscall::sys_fd_write(1, line) != line.len() as u64)
        }
        Kind::Input => {
            let claimed = call(tid, 0x200, [0; 6], PING_TICKS).is_some_and(|r| r.tag == TAG_OK);
            let released = call(tid, 0x201, [0; 6], PING_TICKS).is_some_and(|r| r.tag == TAG_OK);
            fail("cannot claim and release the keyboard", !(claimed && released))
        }
        Kind::Net => {
            fail("has a different address", net_address(tid) != net_before.address)?;
            match ping_gateway(tid) {
                _ if !net_before.pinged => Ok(()),
                Some((TAG_OK, _)) => Ok(()),
                Some((_, code)) => Err(("cannot ping 10.0.2.2", code)),
                None => Err(("cannot ping 10.0.2.2 in time", 0)),
            }
        }
        Kind::Disk => fail("answers a program that is not the VFS", !refused(tid, 3)),
        Kind::Keyboard => fail("answers a program that is not input", !refused(tid, 5)),
    }
}

fn number(arg: &[u8]) -> Option<u64> {
    if arg.is_empty() || arg.len() > 19 {
        return None;
    }
    arg.iter().try_fold(0u64, |n, &c| c.is_ascii_digit().then(|| n * 10 + (c - b'0') as u64))
}

fn show(name: &[u8]) -> &str {
    core::str::from_utf8(name).unwrap_or("?")
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let rounds = args::argv(1).and_then(number).filter(|&n| n <= 1_000_000);
    let seed = match args::argv(2) {
        None => {
            let mut bytes = [0u8; 8];
            let _ = quark_rt::random::fill(&mut bytes);
            Some(u64::from_le_bytes(bytes))
        }
        Some(arg) => number(arg),
    };
    let (Some(rounds), Some(seed), None) = (rounds, seed, args::argv(3)) else {
        println!("usage: qfuzz ROUNDS [SEED]");
        syscall::sys_exit_code(2);
    };
    let rounds = rounds as u32;
    println!("qfuzz: seed {}, {} requests to each service", seed, rounds);

    let me = syscall::sys_getpid();
    if syscall::sys_cap_mint(SELF_SLOT, syscall::CAP_TYPE_ENDPOINT, me, 0).is_err() {
        println!("qfuzz: cannot make an endpoint to offer");
        syscall::sys_exit_code(1);
    }

    let mut rng = Rng::new(seed);
    let mut outcomes: [Outcome; TARGETS.len()] = [const { Outcome::Skipped("") }; TARGETS.len()];
    for (t, outcome) in TARGETS.iter().zip(outcomes.iter_mut()) {
        *outcome = run(&mut rng, t, rounds);
        report(t, outcome);
    }
    let _ = syscall::sys_cap_delete(SELF_SLOT);

    // The console has been sent escapes that move its cursor about, so what
    // was printed since is scattered through what they left. Start again and
    // say it all once more, where it can be read.
    let _ = syscall::sys_fd_write(1, b"\x1b[0m\x1b[2J\x1b[H");
    println!("qfuzz: seed {}, {} requests to each service", seed, rounds);
    let mut failed = 0;
    for (t, outcome) in TARGETS.iter().zip(outcomes.iter()) {
        report(t, outcome);
        if !matches!(outcome, Outcome::Ran(_, Ok(()))) {
            failed += 1;
        }
    }
    println!("qfuzz: seed {}: {} of {} services failed", seed, failed, TARGETS.len());
    syscall::sys_exit_code(failed);
}

/// What became of one service.
enum Outcome {
    /// Not fuzzed, and why.
    Skipped(&'static str),
    Ran(Tally, Result<(), Problem>),
}

fn report(t: &Target, outcome: &Outcome) {
    let _ = syscall::sys_fd_write(1, b"\x1b[0m");
    match outcome {
        Outcome::Skipped(why) => println!("  FAIL  {}: {}", show(t.name), why),
        Outcome::Ran(tally, problem) => {
            print!(
                "  {}  {}: {} answered, {} timed out, {} not sent",
                if problem.is_ok() { "ok  " } else { "FAIL" },
                show(t.name),
                tally.answered,
                tally.timed_out,
                tally.failed
            );
            match problem {
                Ok(()) => println!(),
                Err((what, 0)) => println!("; {}", what),
                Err((what, code)) => println!("; {} ({:#x})", what, code),
            }
        }
    }
}

/// Fuzz one service and check on it.
fn run(rng: &mut Rng, t: &Target, rounds: u32) -> Outcome {
    let Some(tid) = nameserver::lookup(t.name) else {
        return Outcome::Skipped("not registered");
    };
    // Refusing a harmless request is what makes the rest safe to send.
    let probe = match t.kind {
        Kind::Disk => refused(tid, 3),
        Kind::Keyboard => refused(tid, 5),
        _ => true,
    };
    if !probe {
        return Outcome::Skipped("answers anybody; not fuzzed");
    }
    if t.kind == Kind::Vfs {
        let _ = vfs::mkdir(tid, b"/tmp/qfuzz");
        if vfs::chdir(tid, b"/tmp/qfuzz").is_err() {
            return Outcome::Skipped("cannot work in /tmp/qfuzz; not fuzzed");
        }
        open_some(rng, tid);
    }
    let net_before = if t.kind == Kind::Net {
        NetBefore {
            address: net_address(tid),
            pinged: ping_gateway(tid).is_some_and(|(tag, _)| tag == TAG_OK),
        }
    } else {
        NetBefore::default()
    };
    if t.kind == Kind::Net && !net_before.pinged {
        println!("  note  net: 10.0.2.2 does not answer pings here, so that is not checked");
    }

    let tally = fuzz(rng, t, tid, rounds);
    let_go(t.kind, tid);
    if t.kind == Kind::Console {
        if let Some(fb) = nameserver::lookup(b"fb") {
            let_go(Kind::Fb, fb);
        }
    }

    let alive = syscall::sys_task_info(tid).is_ok_and(|(state, _, _)| state != 3);
    let problem = if !alive {
        Err(("died", 0))
    } else if call(tid, TAG_PING, [0; 6], PING_TICKS).is_none() {
        Err(("does not answer a ping", 0))
    } else {
        job(t.kind, tid, net_before)
    };
    Outcome::Ran(tally, problem)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("qfuzz: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
