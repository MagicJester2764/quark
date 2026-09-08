#![no_std]
#![no_main]

use quark_rt::ipc::Message;
use quark_rt::{args, println, syscall};

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const DEFAULT_COUNT: usize = 4;
const NAME_LEN: usize = 24; // 3 x u64, as the nameserver stores it

fn parse_usize(s: &[u8]) -> Option<usize> {
    let mut n: usize = 0;
    if s.is_empty() {
        return None;
    }
    for &b in s {
        if b < b'0' || b > b'9' {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
    }
    Some(n)
}

fn lookup_service(name: &[u8]) -> Option<usize> {
    let mut buf = [0u8; 24];
    let len = name.len().min(24);
    buf[..len].copy_from_slice(&name[..len]);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        Some(reply.tag as usize)
    } else {
        None
    }
}

/// Nameserver reverse lookup: what name did this TID register under?
const TAG_NS_LOOKUP_TID: u64 = 3;
const TAG_NS_OK: u64 = 0;

/// Seconds to wait for a reply before giving up, in 100 Hz PIT ticks.
const PING_TIMEOUT_TICKS: u64 = 300;

/// Look up the service name a TID registered under, if any.
fn service_name_for(tid: usize, buf: &mut [u8; NAME_LEN]) -> Option<usize> {
    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP_TID,
        data: [tid as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag == TAG_NS_OK {
        buf[0..8].copy_from_slice(&reply.data[0].to_le_bytes());
        buf[8..16].copy_from_slice(&reply.data[1].to_le_bytes());
        buf[16..24].copy_from_slice(&reply.data[2].to_le_bytes());
        Some((reply.data[3] as usize).min(NAME_LEN))
    } else {
        None
    }
}

/// Ping `tid`, showing `name` if we know one.
///
/// Both entry points land here: naming a service resolves it to a TID, naming
/// a TID resolves it back to a name. They used to be separate, and the service
/// path measured sys_call to the *nameserver* whatever service you asked for,
/// so `ipcping vfs` reported the nameserver's round trip under VFS's TID.
///
/// The request is TAG_PING, which every IPC server answers and none acts on, so
/// the round trip measures the service without asking it to do work. The call
/// carries a deadline: a TID need not be a server at all, and the console is
/// not one even though it is a service, so an unanswered ping has to end.
fn ping(tid: usize, name: Option<&str>, count: usize) {
    match name {
        Some(n) => println!("PING {} (tid {}) — {} requests", n, tid, count),
        None => println!("PING tid {} — {} requests", tid, count),
    }

    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut total = 0u64;
    let mut ok = 0usize;

    for seq in 0..count {
        let t0 = syscall::sys_ticks();
        let msg = Message { sender: 0, tag: quark_rt::ipc::TAG_PING, data: [0; 6] };
        let mut reply = Message::empty();

        match syscall::sys_call_timeout(tid, &msg, &mut reply, PING_TIMEOUT_TICKS) {
            syscall::CallOutcome::Replied => {
                let dt = syscall::sys_ticks() - t0;
                println!("seq={}: reply from tid {} time={}ms ({}t)", seq, tid, dt * 10, dt);
                if dt < min { min = dt; }
                if dt > max { max = dt; }
                total += dt;
                ok += 1;
            }
            syscall::CallOutcome::TimedOut => {
                println!("seq={}: no reply within {}ms", seq, PING_TIMEOUT_TICKS * 10);
            }
            syscall::CallOutcome::Failed => {
                match name {
                    Some(n) => println!("ipcping: {} (tid {}) unreachable", n, tid),
                    None => println!("ipcping: tid {} unreachable", tid),
                }
                return;
            }
        }

        if seq + 1 < count {
            syscall::sleep_ms(100);
        }
    }

    match name {
        Some(n) => println!("--- {} ping stats ---", n),
        None => println!("--- tid {} ping stats ---", tid),
    }
    if ok > 0 {
        println!(
            "{} sent, {} ok, min={}ms avg={}ms max={}ms",
            count, ok, min * 10, (total * 10) / ok as u64, max * 10
        );
    } else {
        println!("{} sent, 0 ok", count);
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Usage: ipcping [service|tid] [count]
    let service_name = if let Some(arg) = args::argv(1) {
        arg
    } else {
        b"vfs" as &[u8]
    };

    let count = if let Some(arg) = args::argv(2) {
        parse_usize(arg).unwrap_or(DEFAULT_COUNT)
    } else {
        DEFAULT_COUNT
    };

    // A numeric argument names a TID directly, so a task the nameserver does
    // not know about — the shell, or another user program — can be reached.
    // No service name is numeric, so this is unambiguous.
    if let Some(tid) = parse_usize(service_name) {
        let mut buf = [0u8; NAME_LEN];
        let len = service_name_for(tid, &mut buf);
        let name = len.and_then(|n| core::str::from_utf8(&buf[..n]).ok());
        ping(tid, name, count);
        syscall::sys_exit();
    }

    match lookup_service(service_name) {
        Some(tid) => ping(tid, core::str::from_utf8(service_name).ok(), count),
        None => {
            if let Ok(s) = core::str::from_utf8(service_name) {
                println!("ipcping: service '{}' not found", s);
            }
        }
    }

    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("ipcping: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
