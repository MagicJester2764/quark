#![feature(restricted_std)]

//! Fetch a URL over HTTP.
//!
//! Written against `std::net`. It names no service, allocates no pages, and
//! drives no IPC protocol: `TcpStream::connect` returns something that reads
//! and writes, and the rest is the same code it would be on any other system.
//! The previous version did all three of those things by hand.

use std::io::{Read, Write};
use std::net::TcpStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: httpget <host> [port] [path]");
        std::process::exit(2);
    }

    let host = &args[1];
    let port: u16 = args.get(2).and_then(|p| p.parse().ok()).unwrap_or(80);
    let path = args.get(3).map(String::as_str).unwrap_or("/");

    let mut stream = match TcpStream::connect((host.as_str(), port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("httpget: connect to {}:{} failed: {}", host, port, e);
            std::process::exit(1);
        }
    };

    // HTTP/1.0 with an explicit close, so the server ends the body by closing
    // rather than by a chunked encoding this does not parse.
    let request =
        format!("GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", path, host);
    if let Err(e) = stream.write_all(request.as_bytes()) {
        eprintln!("httpget: send failed: {}", e);
        std::process::exit(1);
    }

    let mut body = Vec::new();
    if let Err(e) = stream.read_to_end(&mut body) {
        eprintln!("httpget: receive failed: {}", e);
        std::process::exit(1);
    }

    // The console renders one glyph per byte, so anything unprintable would
    // otherwise scramble the display.
    let text: String = body
        .iter()
        .map(|&b| if b == b'\n' || (0x20..0x7F).contains(&b) { b as char } else { '.' })
        .collect();
    print!("{}", text);
    println!("\n--- {} bytes received ---", body.len());
}
