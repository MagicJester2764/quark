#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{args, println, signal, syscall};
use libquark::net;

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const DEFAULT_COUNT: usize = 4;

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

fn lookup_service_retry(name: &[u8], max_attempts: usize) -> Option<usize> {
    for _ in 0..max_attempts {
        if let Some(tid) = lookup_service(name) {
            return Some(tid);
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    }
    None
}

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

/// Parse dotted-decimal IP (e.g. "10.0.2.2") into big-endian u32.
fn parse_ip(s: &[u8]) -> Option<u32> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0;
    let mut cur: u16 = 0;
    let mut has_digit = false;

    for &b in s {
        if b == b'.' {
            if !has_digit || octet_idx >= 3 {
                return None;
            }
            if cur > 255 {
                return None;
            }
            octets[octet_idx] = cur as u8;
            octet_idx += 1;
            cur = 0;
            has_digit = false;
        } else if b >= b'0' && b <= b'9' {
            cur = cur * 10 + (b - b'0') as u16;
            has_digit = true;
        } else {
            return None;
        }
    }

    if !has_digit || octet_idx != 3 || cur > 255 {
        return None;
    }
    octets[3] = cur as u8;

    Some(u32::from_be_bytes(octets))
}

fn format_ip(ip: u32) -> [u8; 4] {
    ip.to_be_bytes()
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let ip_arg = match args::argv(1) {
        Some(arg) => arg,
        None => {
            println!("Usage: ping <ip> [count]");
            syscall::sys_exit();
        }
    };

    let dst_ip = match parse_ip(ip_arg) {
        Some(ip) => ip,
        None => {
            println!("ping: invalid IP address");
            syscall::sys_exit();
        }
    };

    let count = if let Some(arg) = args::argv(2) {
        parse_usize(arg).unwrap_or(DEFAULT_COUNT)
    } else {
        DEFAULT_COUNT
    };

    let net_tid = match lookup_service_retry(b"net", 50) {
        Some(tid) => tid,
        None => {
            println!("ping: net service not found");
            syscall::sys_exit();
        }
    };

    let ip_bytes = format_ip(dst_ip);
    let pid = syscall::sys_getpid() as u16;

    println!(
        "PING {}.{}.{}.{} — {} data bytes",
        ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], 32
    );

    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut total = 0u64;
    let mut ok = 0usize;

    let mut sent = 0usize;
    for seq in 0..count {
        sent += 1;
        let ping_result = net::icmp_ping(net_tid, dst_ip, pid, seq as u16);

        // Non-blocking check: did a signal arrive (possibly interrupting the IPC call)?
        let mut msg = Message::empty();
        let signaled = syscall::sys_recv_timeout(0, &mut msg, 0).is_ok()
            && signal::extract_signal(&msg) != 0;

        match ping_result {
            Ok((rtt_ticks, ttl, size)) => {
                let ms = rtt_ticks * 10;
                println!(
                    "{} bytes from {}.{}.{}.{}: icmp_seq={} ttl={} time={}ms",
                    size,
                    ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3],
                    seq, ttl, ms
                );
                if rtt_ticks < min { min = rtt_ticks; }
                if rtt_ticks > max { max = rtt_ticks; }
                total += rtt_ticks;
                ok += 1;
            }
            Err(_) if !signaled => {
                println!(
                    "Request timeout for icmp_seq={}",
                    seq
                );
            }
            Err(_) => {} // interrupted by signal, skip timeout message
        }

        if signaled {
            break;
        }

        if seq + 1 < count {
            // Sleep 1 second, but wake on signal (recv from kernel with timeout)
            let mut msg = Message::empty();
            if syscall::sys_recv_timeout(0, &mut msg, 100).is_ok() {
                if signal::extract_signal(&msg) != 0 {
                    break;
                }
            }
        }
    }

    let loss = if sent > 0 { ((sent - ok) * 100) / sent } else { 100 };
    println!(
        "\n--- {}.{}.{}.{} ping statistics ---",
        ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]
    );
    if ok > 0 {
        let avg_ms = (total * 10) / ok as u64;
        println!(
            "{} packets transmitted, {} received, {}% loss, min/avg/max = {}/{}/{}ms",
            sent, ok, loss, min * 10, avg_ms, max * 10
        );
    } else {
        println!(
            "{} packets transmitted, 0 received, 100% loss",
            sent
        );
    }

    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("ping: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
