//! x86-64 port I/O helpers.

/// Write a byte to an I/O port.
pub unsafe fn outb(port: u16, val: u8) {
    core::arch::asm!(
        "out %al, %dx",
        in("dx") port,
        in("al") val,
        options(att_syntax, nostack, nomem)
    );
}

/// Read a byte from an I/O port.
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    core::arch::asm!(
        "in %dx, %al",
        in("dx") port,
        out("al") val,
        options(att_syntax, nostack, nomem)
    );
    val
}

/// Small delay for PIC initialization timing (write to unused port 0x80).
pub unsafe fn io_wait() {
    outb(0x80, 0);
}
