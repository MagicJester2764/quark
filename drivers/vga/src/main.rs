//! VGA text mode driver for the Quark microkernel.
//!
//! Compiled as a position-independent flat binary. The kernel loads this module
//! and calls `_entry` at offset 0 to obtain a vtable of console operations.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

const VGA_BASE: usize = 0xB8000;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const ATTR: u8 = 0x0F; // white on black

static mut COL: usize = 0;
static mut ROW: usize = 0;

/// Kernel services provided at init time.
#[repr(C)]
pub struct KernelServices {
    pub alloc_page: extern "C" fn() -> usize,
    pub free_page: extern "C" fn(addr: usize),
}

static mut SERVICES: *const KernelServices = core::ptr::null();

/// Vtable returned to the kernel. Must match the kernel's definition exactly.
#[repr(C)]
pub struct ConsoleVtable {
    pub clear: extern "C" fn(),
    pub putc: extern "C" fn(u8),
    pub puts: extern "C" fn(*const u8, usize),
}

/// Driver entry point — must be at offset 0 of the flat binary.
/// The kernel calls this once to initialize VGA state and obtain function pointers.
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _entry(out: *mut ConsoleVtable, services: *const KernelServices) {
    // Explicitly initialize all state (BSS may not be zeroed by the bootloader)
    unsafe {
        COL = 0;
        ROW = 0;
        SERVICES = services;
    }

    unsafe {
        (*out).clear = vga_clear;
        (*out).putc = vga_putc;
        (*out).puts = vga_puts;
    }
}

extern "C" fn vga_clear() {
    unsafe {
        let buf = VGA_BASE as *mut u8;
        for i in 0..(WIDTH * HEIGHT) {
            buf.add(i * 2).write_volatile(b' ');
            buf.add(i * 2 + 1).write_volatile(ATTR);
        }
        COL = 0;
        ROW = 0;
    }
}

extern "C" fn vga_putc(c: u8) {
    unsafe {
        match c {
            b'\n' => {
                COL = 0;
                ROW += 1;
            }
            byte => {
                let offset = (ROW * WIDTH + COL) * 2;
                let buf = VGA_BASE as *mut u8;
                buf.add(offset).write_volatile(byte);
                buf.add(offset + 1).write_volatile(ATTR);
                COL += 1;
                if COL >= WIDTH {
                    COL = 0;
                    ROW += 1;
                }
            }
        }
        if ROW >= HEIGHT {
            scroll();
        }
    }
}

extern "C" fn vga_puts(s: *const u8, len: usize) {
    for i in 0..len {
        vga_putc(unsafe { *s.add(i) });
    }
}

unsafe fn scroll() {
    let buf = VGA_BASE as *mut u8;
    // Copy rows 1..HEIGHT up to 0..HEIGHT-1
    for row in 1..HEIGHT {
        for col in 0..WIDTH {
            let src = (row * WIDTH + col) * 2;
            let dst = ((row - 1) * WIDTH + col) * 2;
            buf.add(dst).write_volatile(buf.add(src).read_volatile());
            buf.add(dst + 1).write_volatile(buf.add(src + 1).read_volatile());
        }
    }
    // Clear last row
    for col in 0..WIDTH {
        let offset = ((HEIGHT - 1) * WIDTH + col) * 2;
        buf.add(offset).write_volatile(b' ');
        buf.add(offset + 1).write_volatile(ATTR);
    }
    ROW = HEIGHT - 1;
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
