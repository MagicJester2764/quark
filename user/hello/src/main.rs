#![no_std]
#![no_main]

use libquark::syscall;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    syscall::sys_write(b"Hello from user space!\n");
    syscall::sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    syscall::sys_write(b"[hello] PANIC!\n");
    loop {
        core::hint::spin_loop();
    }
}
