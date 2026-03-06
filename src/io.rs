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

/// Write a 16-bit word to an I/O port.
pub unsafe fn outw(port: u16, val: u16) {
    core::arch::asm!(
        "out %ax, %dx",
        in("dx") port,
        in("ax") val,
        options(att_syntax, nostack, nomem)
    );
}

/// Read a 16-bit word from an I/O port.
pub unsafe fn inw(port: u16) -> u16 {
    let val: u16;
    core::arch::asm!(
        "in %dx, %ax",
        in("dx") port,
        out("ax") val,
        options(att_syntax, nostack, nomem)
    );
    val
}

/// Write a 32-bit dword to an I/O port.
pub unsafe fn outl(port: u16, val: u32) {
    core::arch::asm!(
        "out %eax, %dx",
        in("dx") port,
        in("eax") val,
        options(att_syntax, nostack, nomem)
    );
}

/// Read a 32-bit dword from an I/O port.
pub unsafe fn inl(port: u16) -> u32 {
    let val: u32;
    core::arch::asm!(
        "in %dx, %eax",
        in("dx") port,
        out("eax") val,
        options(att_syntax, nostack, nomem)
    );
    val
}

/// Read `count` 16-bit words from an I/O port into a buffer using `rep insw`.
///
/// # Safety
/// `buf` must point to at least `count * 2` writable bytes.
pub unsafe fn rep_insw(port: u16, buf: *mut u16, count: usize) {
    core::arch::asm!(
        "rep insw",
        in("dx") port,
        inout("rdi") buf => _,
        inout("rcx") count => _,
        options(att_syntax, nostack)
    );
}

/// Write `count` 16-bit words from a buffer to an I/O port using `rep outsw`.
///
/// # Safety
/// `buf` must point to at least `count * 2` readable bytes.
pub unsafe fn rep_outsw(port: u16, buf: *const u16, count: usize) {
    core::arch::asm!(
        "rep outsw",
        in("dx") port,
        inout("rsi") buf => _,
        inout("rcx") count => _,
        options(att_syntax, nostack)
    );
}

/// Small delay for PIC initialization timing (write to unused port 0x80).
pub unsafe fn io_wait() {
    outb(0x80, 0);
}
