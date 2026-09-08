#![no_std]
#![no_main]

//! A TCP client that never names the net server.
//!
//! The point of the exercise: `TcpStream` finds the service, opens the
//! connection, and hands back a descriptor that reads and writes through the
//! ordinary fd calls — the same ones a console or a pipe uses.

use quark_rt::manifest::CapReq;
use quark_rt::socket::TcpStream;
use quark_rt::{args, println, syscall};

// Talking to the net server is all this needs; the descriptor comes from the
// kernel, and the pages the connection uses belong to the server.
quark_rt::manifest!([CapReq::phys_alloc(4)]);

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let host = args::argv(1).unwrap_or(b"10.0.2.2");
    let port: u16 = args::argv(2)
        .and_then(|p| core::str::from_utf8(p).ok())
        .and_then(|p| p.parse().ok())
        .unwrap_or(8088);

    println!("socktest: connecting to {}:{}", Bytes(host), port);

    let stream = match TcpStream::connect_host(host, port) {
        Ok(s) => s,
        Err(e) => {
            println!("socktest: connect failed: {:?}", e);
            syscall::sys_exit_code(1);
        }
    };
    println!("  connected, fd {}", stream.as_fd());

    let request = b"GET /hello HTTP/1.0\r\nHost: quark\r\n\r\n";
    if let Err(e) = stream.write_all(request) {
        println!("  write failed: {:?}", e);
        syscall::sys_exit_code(1);
    }
    println!("  wrote {} bytes", request.len());

    let mut total = 0usize;
    let mut buf = [0u8; 256];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                print_bytes(&buf[..n]);
            }
            Err(e) => {
                println!("\n  read failed: {:?}", e);
                break;
            }
        }
    }
    println!("\nsocktest: read {} bytes total", total);
    syscall::sys_exit_code(0);
}

fn print_bytes(b: &[u8]) {
    for &c in b {
        let ch = if c == b'\n' || (0x20..0x7F).contains(&c) { c } else { b'.' };
        quark_rt::print!("{}", ch as char);
    }
}

/// Printing a byte slice as text without allocating.
struct Bytes<'a>(&'a [u8]);

impl core::fmt::Display for Bytes<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for &c in self.0 {
            write!(f, "{}", c as char)?;
        }
        Ok(())
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    quark_rt::syscall::sys_exit_code(255);
}
