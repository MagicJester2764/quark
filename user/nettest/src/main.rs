#![no_std]
#![no_main]

//! The network server's data path, end to end.
//!
//! A datagram and a TCP stream to the echo server on the machine QEMU runs on —
//! 10.0.2.2:7007 from here, which `boot-test.sh` provides — and what goes out
//! must come back. Every call lends its buffer to the server rather than
//! naming a page for it to map, which is why this asks for no capability.
//!
//! Exits 0 only if every check holds.

use quark_rt::{nameserver, net, println, syscall};

/// 10.0.2.2, the host as QEMU's user network shows it.
const ECHO_IP: u32 = 0x0A00_0202;
const ECHO_PORT: u16 = 7007;
const LOCAL_PORT: u16 = 40007;

static mut FAILED: u32 = 0;

fn check(what: &str, ok: bool) {
    println!("  {}  {}", if ok { "ok  " } else { "FAIL" }, what);
    if !ok {
        unsafe { FAILED += 1 };
    }
}

fn udp(net_tid: usize) {
    // The first datagram to an address the server has no hardware address for
    // goes to ARP instead, and is refused; one sent after the answer goes.
    let mut sent = Err(0);
    for _ in 0..10 {
        sent = net::udp_send(net_tid, b"quark-udp", ECHO_IP, ECHO_PORT, LOCAL_PORT);
        if sent.is_ok() {
            break;
        }
        syscall::sleep_ticks(10);
    }
    check("a datagram goes out", sent.is_ok());
    if sent.is_err() {
        return;
    }
    let mut buf = [0u8; 64];
    match net::udp_recv(net_tid, &mut buf, LOCAL_PORT) {
        Ok((n, ip, port, _)) => {
            check("and comes back", &buf[..n] == b"quark-udp");
            check("from the echo server", ip == ECHO_IP && port == ECHO_PORT);
        }
        Err(_) => check("and comes back", false),
    }
}

fn tcp(net_tid: usize) {
    let Ok(handle) = net::tcp_connect(net_tid, ECHO_IP, ECHO_PORT, 0) else {
        check("a connection opens", false);
        return;
    };
    check("a connection opens", true);
    check("bytes go out", net::tcp_send(net_tid, handle, b"quark-tcp") == Ok(9));
    let mut got = [0u8; 64];
    let mut n = 0;
    while n < 9 {
        match net::tcp_recv(net_tid, handle, &mut got[n..]) {
            Ok(k) if k > 0 => n += k,
            _ => break,
        }
    }
    check("and come back", &got[..n] == b"quark-tcp");
    let _ = net::tcp_close(net_tid, handle);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("nettest: echo at 10.0.2.2:{}", ECHO_PORT);
    let Some(net_tid) = nameserver::lookup_retry(b"net", 100) else {
        println!("nettest: no network service");
        syscall::sys_exit_code(1);
    };
    udp(net_tid);
    tcp(net_tid);
    let failed = unsafe { FAILED };
    println!("nettest: {}", if failed == 0 { "ok" } else { "FAIL" });
    syscall::sys_exit_code(if failed == 0 { 0 } else { 1 });
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("nettest: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
