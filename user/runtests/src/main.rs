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
//! skipped.

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

enum Outcome {
    Passed,
    Exit(i32),
    Signal(i32),
    Missing,
}

impl Outcome {
    fn from(code: i32) -> Outcome {
        match code {
            0 => Outcome::Passed,
            i32::MIN => Outcome::Missing,
            c if c < 0 => Outcome::Signal(-c),
            c => Outcome::Exit(c),
        }
    }
}

fn report(name: &[u8], o: Outcome) {
    let name = core::str::from_utf8(name).unwrap_or("?");
    match o {
        Outcome::Passed => println!("  ok    {}", name),
        Outcome::Exit(c) => println!("  FAIL  {} (exit {})", name, c),
        Outcome::Signal(s) => println!("  FAIL  {} (signal {})", name, s),
        Outcome::Missing => println!("  FAIL  {} (not found)", name),
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

/// Run one program to completion and return its status, or `i32::MIN` if it
/// could not be started at all.
fn run(vfs_tid: usize, name: &[u8], args: &[u8]) -> i32 {
    let mut path = [0u8; 64];
    let n = join(b"/usr/bin/", name, &mut path);
    if n == 0 {
        return i32::MIN;
    }
    let Ok(info) = spawn::load_path(vfs_tid, &path[..n], IMAGE_AT, &SCRATCH, grant) else {
        return i32::MIN;
    };
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);
    let mut argv = [&b""[..]; 16];
    let argc = split_words(name, args, &mut argv);
    let _ = spawn::set_args_env(&info, &argv[..argc], &ENV, &SCRATCH);
    if info.start().is_err() {
        let _ = syscall::sys_task_kill(info.tid);
        return i32::MIN;
    }
    loop {
        match syscall::sys_wait() {
            Ok((tid, code)) if tid == info.tid => return code,
            // Not this program — nothing else here starts children, but a
            // status that is not ours is not a reason to stop waiting.
            Ok(_) => continue,
            // No children at all: it vanished without being reaped by us.
            Err(()) => return i32::MIN,
        }
    }
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
        let (name, args) = split_first_word(line);
        let code = run(vfs_tid, name, args);
        if code == 0 {
            passed += 1;
        } else {
            failed += 1;
        }
        report(name, Outcome::from(code));
    }

    println!("runtests: {} passed, {} failed", passed, failed);
    syscall::sys_exit_code(if failed == 0 { 0 } else { 1 });
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("runtests: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
