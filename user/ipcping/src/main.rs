#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{args, println, syscall};

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const DEFAULT_COUNT: usize = 4;

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

fn ping_service(tid: usize, count: usize, name: &[u8]) {
    let name_str = core::str::from_utf8(name).unwrap_or("???");
    println!("PING {} (tid {}) — {} requests", name_str, tid, count);

    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut total = 0u64;
    let mut ok = 0usize;

    for seq in 0..count {
        // Send a lookup for the service's own name — a no-op round-trip
        let t0 = syscall::sys_ticks();

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

        if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
            let t1 = syscall::sys_ticks();
            let dt = t1 - t0;
            let ms = dt * 10; // 100 Hz PIT → 10ms per tick

            println!("seq={}: reply from tid {} time={}ms ({}t)", seq, tid, ms, dt);

            if dt < min { min = dt; }
            if dt > max { max = dt; }
            total += dt;
            ok += 1;
        } else {
            println!("seq={}: no reply", seq);
        }

        // Wait between pings
        if seq + 1 < count {
            syscall::sleep_ms(100);
        }
    }

    println!("--- {} ping stats ---", name_str);
    if ok > 0 {
        let avg_ms = (total * 10) / ok as u64;
        println!(
            "{} sent, {} ok, min={}ms avg={}ms max={}ms",
            count, ok, min * 10, avg_ms, max * 10
        );
    } else {
        println!("{} sent, 0 ok", count);
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Usage: ipcping [service] [count]
    let service_name = if let Some(arg) = args::argv(1) {
        arg
    } else {
        b"nameserver" as &[u8]
    };

    let count = if let Some(arg) = args::argv(2) {
        parse_usize(arg).unwrap_or(DEFAULT_COUNT)
    } else {
        DEFAULT_COUNT
    };

    match lookup_service(service_name) {
        Some(tid) => ping_service(tid, count, service_name),
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
