//! Minimal COM1 serial debug output (0x3F8).
//! Used for kernel debugging — bypasses the console server entirely.

use crate::io;

const COM1: u16 = 0x3F8;

/// Initialize COM1 at 115200 baud.
pub fn init() {
    unsafe {
        io::outb(COM1 + 1, 0x00); // Disable interrupts
        io::outb(COM1 + 3, 0x80); // Enable DLAB
        io::outb(COM1 + 0, 0x01); // Divisor low (115200 baud)
        io::outb(COM1 + 1, 0x00); // Divisor high
        io::outb(COM1 + 3, 0x03); // 8N1
        io::outb(COM1 + 2, 0xC7); // Enable FIFO
        io::outb(COM1 + 4, 0x03); // RTS/DSR set
    }
}

/// Write a single byte to serial.
#[inline]
pub fn putb(b: u8) {
    unsafe {
        // Wait for transmit buffer empty
        while io::inb(COM1 + 5) & 0x20 == 0 {}
        io::outb(COM1, b);
    }
}

/// Write a string to serial.
pub fn puts(s: &[u8]) {
    for &b in s {
        if b == b'\n' {
            putb(b'\r');
        }
        putb(b);
    }
}

/// Write a decimal number to serial.
pub fn put_usize(mut n: usize) {
    if n == 0 {
        putb(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putb(buf[i]);
    }
}

/// Write a hex byte to serial.
pub fn put_hex8(v: u8) {
    const HEX: &[u8] = b"0123456789abcdef";
    putb(HEX[(v >> 4) as usize]);
    putb(HEX[(v & 0xF) as usize]);
}

/// Write a usize as hex to serial.
pub fn put_hex_usize(mut n: usize) {
    const HEX: &[u8] = b"0123456789abcdef";
    if n == 0 {
        putb(b'0');
        return;
    }
    let mut buf = [0u8; 16];
    let mut i = 0;
    while n > 0 {
        buf[i] = HEX[n & 0xF];
        n >>= 4;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        putb(buf[i]);
    }
}
