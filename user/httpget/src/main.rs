#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{args, println, syscall};
use libquark::net;

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;

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

fn parse_ip(s: &[u8]) -> Option<u32> {
    let mut octets = [0u8; 4];
    let mut octet_idx = 0;
    let mut cur: u16 = 0;
    let mut has_digit = false;

    for &b in s {
        if b == b'.' {
            if !has_digit || octet_idx >= 3 { return None; }
            if cur > 255 { return None; }
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

    if !has_digit || octet_idx != 3 || cur > 255 { return None; }
    octets[3] = cur as u8;
    Some(u32::from_be_bytes(octets))
}

fn parse_u16(s: &[u8]) -> Option<u16> {
    let mut n: u16 = 0;
    if s.is_empty() { return None; }
    for &b in s {
        if b < b'0' || b > b'9' { return None; }
        n = n.checked_mul(10)?.checked_add((b - b'0') as u16)?;
    }
    Some(n)
}

/// Map for sending/receiving TCP data
const DATA_BUF: usize = 0x88_0000_0000;

fn alloc_data_buf() -> usize {
    let phys = match syscall::sys_phys_alloc(1) {
        Ok(p) => p,
        Err(()) => return 0,
    };
    if syscall::sys_map_phys(phys, DATA_BUF, 1).is_err() { return 0; }
    unsafe { core::ptr::write_bytes(DATA_BUF as *mut u8, 0, 4096); }
    phys
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let ip_arg = match args::argv(1) {
        Some(arg) => arg,
        None => {
            println!("Usage: httpget <ip> [port] [path]");
            syscall::sys_exit();
        }
    };

    let dst_ip = match parse_ip(ip_arg) {
        Some(ip) => ip,
        None => {
            println!("httpget: invalid IP address");
            syscall::sys_exit();
        }
    };

    let port = if let Some(arg) = args::argv(2) {
        parse_u16(arg).unwrap_or(80)
    } else {
        80
    };

    let path = if let Some(arg) = args::argv(3) { arg } else { b"/" };

    let net_tid = match lookup_service_retry(b"net", 50) {
        Some(tid) => tid,
        None => {
            println!("httpget: net service not found");
            syscall::sys_exit();
        }
    };

    let phys = alloc_data_buf();
    if phys == 0 {
        println!("httpget: failed to allocate buffer");
        syscall::sys_exit();
    }

    let ip_bytes = dst_ip.to_be_bytes();
    println!(
        "Connecting to {}.{}.{}.{}:{}...",
        ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3], port
    );

    let handle = match net::tcp_connect(net_tid, dst_ip, port, 0) {
        Ok(h) => h,
        Err(e) => {
            println!("httpget: connect failed (error {})", e);
            syscall::sys_exit();
        }
    };

    println!("Connected. Sending request...");

    // Build HTTP request in the data buffer
    let req_prefix = b"GET ";
    let req_mid = b" HTTP/1.0\r\nHost: ";
    let req_suffix = b"\r\nConnection: close\r\n\r\n";

    let mut len = 0usize;
    let buf = DATA_BUF as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(req_prefix.as_ptr(), buf.add(len), req_prefix.len());
        len += req_prefix.len();
        core::ptr::copy_nonoverlapping(path.as_ptr(), buf.add(len), path.len());
        len += path.len();
        core::ptr::copy_nonoverlapping(req_mid.as_ptr(), buf.add(len), req_mid.len());
        len += req_mid.len();
        // Write IP as host
        let host = format_ip_str(&ip_bytes);
        let host_len = host.iter().position(|&b| b == 0).unwrap_or(host.len());
        core::ptr::copy_nonoverlapping(host.as_ptr(), buf.add(len), host_len);
        len += host_len;
        core::ptr::copy_nonoverlapping(req_suffix.as_ptr(), buf.add(len), req_suffix.len());
        len += req_suffix.len();
    }

    match net::tcp_send(net_tid, handle, phys, len) {
        Ok(n) => {
            if n < len {
                println!("httpget: warning: only {} of {} bytes sent", n, len);
            }
        }
        Err(e) => {
            println!("httpget: send failed (error {})", e);
            let _ = net::tcp_close(net_tid, handle);
            syscall::sys_exit();
        }
    }

    // Read response
    let mut total = 0usize;
    loop {
        match net::tcp_recv(net_tid, handle, phys, 4096) {
            Ok(0) => break, // EOF
            Ok(n) => {
                total += n;
                // Print received data
                let data = unsafe { core::slice::from_raw_parts(DATA_BUF as *const u8, n) };
                for &b in data {
                    if b >= 0x20 && b < 0x7F || b == b'\n' || b == b'\r' || b == b'\t' {
                        print_char(b);
                    } else {
                        print_char(b'.');
                    }
                }
            }
            Err(e) => {
                println!("\nhttpget: recv error ({})", e);
                break;
            }
        }
    }

    println!("\n--- {} bytes received ---", total);

    let _ = net::tcp_close(net_tid, handle);
    syscall::sys_exit();
}

fn format_ip_str(ip: &[u8; 4]) -> [u8; 15] {
    let mut buf = [0u8; 15];
    let mut pos = 0;
    for i in 0..4 {
        if i > 0 {
            buf[pos] = b'.';
            pos += 1;
        }
        let mut v = ip[i];
        if v >= 100 {
            buf[pos] = b'0' + v / 100;
            pos += 1;
            v %= 100;
            buf[pos] = b'0' + v / 10;
            pos += 1;
            buf[pos] = b'0' + v % 10;
            pos += 1;
        } else if v >= 10 {
            buf[pos] = b'0' + v / 10;
            pos += 1;
            buf[pos] = b'0' + v % 10;
            pos += 1;
        } else {
            buf[pos] = b'0' + v;
            pos += 1;
        }
    }
    buf
}

fn print_char(b: u8) {
    let s = [b];
    // Use sys_write directly for single character output
    syscall::sys_write(&s);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("httpget: PANIC: {}", info);
    loop { core::hint::spin_loop(); }
}
