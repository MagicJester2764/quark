#![no_std]
#![no_main]

mod console;
#[allow(dead_code)]
mod modules;
mod multiboot2;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: usize) -> ! {
    // Initialize module registry first — console::init needs it to find the VGA driver
    unsafe { modules::init(multiboot_info) };

    let fb = unsafe { multiboot2::parse_framebuffer(multiboot_info) };
    console::init(fb);
    console::clear();
    console::puts(b"Quark v0.1.0 - microkernel\n");
    console::puts(b"Booted successfully.\n");

    let mod_count = modules::count();
    if mod_count > 0 {
        console::puts(b"Boot modules:\n");
        for i in 0..mod_count {
            if let Some(m) = modules::get(i) {
                console::puts(b"  Module: ");
                console::puts(modules::name_str(m));
                console::puts(b" [");
                print_hex(m.start);
                console::puts(b"..");
                print_hex(m.end);
                console::puts(b"] (");
                print_dec(m.end - m.start);
                console::puts(b" bytes)\n");
            }
        }
    } else {
        console::puts(b"No boot modules loaded.\n");
    }

    loop {
        core::hint::spin_loop();
    }
}

fn print_hex(val: usize) {
    console::puts(b"0x");
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 16];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        let digit = (n & 0xF) as u8;
        buf[i] = if digit < 10 { b'0' + digit } else { b'A' + digit - 10 };
        n >>= 4;
        i += 1;
    }
    // Reverse
    let mut out = [0u8; 16];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

fn print_dec(val: usize) {
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console::puts(b"\nKERNEL PANIC!");
    loop {
        core::hint::spin_loop();
    }
}
