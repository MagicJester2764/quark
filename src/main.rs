#![no_std]
#![no_main]

mod console;
mod multiboot2;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: usize) -> ! {
    let fb = unsafe { multiboot2::parse_framebuffer(multiboot_info) };
    console::init(fb);
    console::clear();
    console::puts(b"Quark v0.1.0 - microkernel\n");
    console::puts(b"Booted successfully.\n");

    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console::puts(b"\nKERNEL PANIC!");
    loop {
        core::hint::spin_loop();
    }
}
