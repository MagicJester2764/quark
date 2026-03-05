#![no_std]
#![no_main]

use libquark::{println, syscall};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("Hello from user space!");
    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[hello] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
