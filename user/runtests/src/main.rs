#![no_std]
#![no_main]

//! Run a list of test programs and say how many passed.
//!
//! `runtests /etc/pixman.tests` loads each program named in the file, runs it
//! with the arguments given on its line, waits for it, and prints one line per
//! program and one summary line at the end. It exits 0 only if every program
//! did.
//!
//! It exists because a ported library's test suite is thirty programs, and
//! typing thirty commands into a shell is not a test: a slow one overlaps the
//! next, and thirty programs' output does not fit on a screen. One summary line
//! does.
//!
//! The list is one program per line: its name in `/usr/bin`, then its
//! arguments separated by spaces. Blank lines and lines starting with `#` are
//! skipped. Two words may come before the name: `?`, which counts any exit
//! status as a pass (a program that is expected to refuse its arguments) but
//! not a fault, and `@N`, which gives it N seconds instead of 300 before it is
//! killed and reported as timed out.

use quark_rt::manifest::CapReq;
use quark_rt::spawn::{self, Scratch};
use quark_rt::{args, nameserver, println, syscall, vfs};

// The same authority the shell has, because this does what the shell does:
// start programs and give them what their manifests ask for.
quark_rt::manifest!([CapReq::task_mgmt(0), CapReq::phys_alloc(64)]);

/// Where a program's image is read to before it is loaded. Freed each time.
const IMAGE_AT: usize = 0x9A_0000_0000;
const LIST_MAX: usize = 4096;

static SCRATCH: Scratch = Scratch {
    elf: 0x9B_0000_0000,
    stack: 0x9C_0000_0000,
    args: 0x9D_0000_0000,
};

/// What every program is started with, as the shell starts them.
const ENV: [&[u8]; 4] = [
    b"HOME=/home/root",
    b"PATH=/usr/bin",
    b"TERM=quark",
    b"USER=root",
];

/// As the shell does: the IPC reach it has, then whatever the manifest asks.
fn grant(image: &[u8], tid: usize) {
    let _ = syscall::sys_cap_grant(tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);
    quark_rt::manifest::grant_image(tid, image, 12);
}

/// The first space-separated word, and whatever follows the spaces after it.
fn split_first_word(line: &[u8]) -> (&[u8], &[u8]) {
    let end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let mut rest = end;
    while rest < line.len() && line[rest] == b' ' {
        rest += 1;
    }
    (&line[..end], &line[rest..])
}

/// `prefix` then `name` into `out`; the length written, or 0 if it does not fit.
fn join(prefix: &[u8], name: &[u8], out: &mut [u8]) -> usize {
    let n = prefix.len() + name.len();
    if n > out.len() {
        return 0;
    }
    out[..prefix.len()].copy_from_slice(prefix);
    out[prefix.len()..n].copy_from_slice(name);
    n
}

/// `name` followed by each space-separated word of `args`, at most sixteen.
fn split_words<'a>(name: &'a [u8], args: &'a [u8], out: &mut [&'a [u8]; 16]) -> usize {
    out[0] = name;
    let mut n = 1;
    for w in args.split(|&b| b == b' ').filter(|w| !w.is_empty()) {
        if n == out.len() {
            break;
        }
        out[n] = w;
        n += 1;
    }
    n
}

/// How long a program gets unless its line says otherwise.
const DEFAULT_SECONDS: u64 = 300;
const TICKS_PER_SECOND: u64 = 100;

enum Outcome {
    Passed,
    Exit(i32),
    Signal(i32),
    Missing,
    TimedOut,
}

impl Outcome {
    /// What a program's end means, where `any_exit` counts every exit status
    /// as a pass.
    fn from(end: End, any_exit: bool) -> Outcome {
        match end {
            End::Missing => Outcome::Missing,
            End::TimedOut => Outcome::TimedOut,
            End::Status(c) if c < 0 => Outcome::Signal(-c),
            End::Status(0) => Outcome::Passed,
            End::Status(_) if any_exit => Outcome::Passed,
            End::Status(c) => Outcome::Exit(c),
        }
    }
}

/// How a program run by `run` ended.
enum End {
    Status(i32),
    Missing,
    TimedOut,
}

fn report(name: &[u8], o: &Outcome) {
    let name = core::str::from_utf8(name).unwrap_or("?");
    match o {
        Outcome::Passed => println!("  ok    {}", name),
        Outcome::Exit(c) => println!("  FAIL  {} (exit {})", name, c),
        Outcome::Signal(s) => println!("  FAIL  {} (signal {})", name, s),
        Outcome::Missing => println!("  FAIL  {} (not found)", name),
        Outcome::TimedOut => println!("  FAIL  {} (timed out)", name),
    }
}

/// Wait for `tid` to end, for at most `ticks`. Its status, or `None` if it
/// was still running when the time ran out (and has been killed since).
fn wait_until(tid: usize, ticks: u64) -> Option<i32> {
    let deadline = syscall::sys_ticks() + ticks;
    // Told of its death rather than waiting in sys_wait, which has no end.
    let watched = syscall::sys_task_watch(tid).is_ok();
    let mut finished = !watched;
    while !finished {
        let now = syscall::sys_ticks();
        if now >= deadline {
            let _ = syscall::sys_task_kill(tid);
            let _ = reap(tid);
            return None;
        }
        let mut msg = quark_rt::ipc::Message::empty();
        if syscall::sys_recv_timeout(quark_rt::ipc::TID_ANY, &mut msg, deadline - now).is_ok()
            && msg.sender == 0
            && msg.tag == quark_rt::ipc::TAG_TASK_DIED
            && msg.data[0] as usize == tid
        {
            finished = true;
        }
    }
    reap(tid)
}

/// Collect `tid`'s status.
fn reap(tid: usize) -> Option<i32> {
    loop {
        match syscall::sys_wait() {
            Ok((t, code)) if t == tid => return Some(code),
            // Not this program — nothing else here starts children, but a
            // status that is not ours is not a reason to stop waiting.
            Ok(_) => continue,
            // No children at all: it vanished without being reaped by us.
            Err(()) => return Some(i32::MIN),
        }
    }
}

/// Read a list into `buf`. Returns its length.
fn read_list(vfs_tid: usize, path: &[u8], buf: &mut [u8; LIST_MAX]) -> Option<usize> {
    let (handle, size, _) = vfs::open(vfs_tid, path).ok()?;
    let size = (size as usize).min(LIST_MAX);
    let got = vfs::read(vfs_tid, handle, &mut buf[..size], 0).ok();
    let _ = vfs::close(vfs_tid, handle);
    got.map(|n| (n as usize).min(size))
}

/// Run one program to completion, or for `seconds` at most.
fn run(vfs_tid: usize, name: &[u8], args: &[u8], seconds: u64) -> End {
    let mut path = [0u8; 64];
    let n = join(b"/usr/bin/", name, &mut path);
    if n == 0 {
        return End::Missing;
    }
    let Ok(info) = spawn::load_path(vfs_tid, &path[..n], IMAGE_AT, &SCRATCH, grant) else {
        return End::Missing;
    };
    // A test runs where runtests was started.
    let _ = vfs::give_cwd(vfs_tid, info.tid);
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);
    let mut argv = [&b""[..]; 16];
    let argc = split_words(name, args, &mut argv);
    let _ = spawn::set_args_env(&info, &argv[..argc], &ENV, &SCRATCH);
    if info.start().is_err() {
        let _ = syscall::sys_task_kill(info.tid);
        return End::Missing;
    }
    match wait_until(info.tid, seconds.saturating_mul(TICKS_PER_SECOND)) {
        Some(i32::MIN) => End::Missing,
        Some(code) => End::Status(code),
        None => End::TimedOut,
    }
}

/// A whole number of seconds, at least one.
fn parse_seconds(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = digits.iter().fold(0u64, |acc, d| acc * 10 + u64::from(d - b'0'));
    (n > 0).then_some(n)
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(list) = args::argv(1) else {
        println!("usage: runtests <list>");
        syscall::sys_exit_code(2);
    };
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        println!("runtests: no filesystem");
        syscall::sys_exit_code(2);
    };
    let mut buf = [0u8; LIST_MAX];
    let Some(len) = read_list(vfs_tid, list, &mut buf) else {
        println!("runtests: cannot read the list");
        syscall::sys_exit_code(2);
    };

    let mut passed = 0u32;
    let mut failed = 0u32;
    for raw in buf[..len].split(|&b| b == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if line.is_empty() || line[0] == b'#' {
            continue;
        }
        let (mut name, mut args) = split_first_word(line);
        let any_exit = name == b"?";
        if any_exit {
            (name, args) = split_first_word(args);
        }
        let mut seconds = DEFAULT_SECONDS;
        if let Some(n) = name.strip_prefix(b"@").and_then(parse_seconds) {
            seconds = n;
            (name, args) = split_first_word(args);
        }
        if name.is_empty() {
            continue;
        }
        let outcome = Outcome::from(run(vfs_tid, name, args, seconds), any_exit);
        if matches!(outcome, Outcome::Passed) {
            passed += 1;
        } else {
            failed += 1;
        }
        report(name, &outcome);
    }

    println!("runtests: {} passed, {} failed", passed, failed);
    syscall::sys_exit_code(if failed == 0 { 0 } else { 1 });
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("runtests: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
