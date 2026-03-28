#![no_std]
#![no_main]

use quark_rt::{args, print, println, syscall};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let argc = args::argc();
    for i in 1..argc {
        if i > 1 {
            print!(" ");
        }
        if let Some(arg) = args::argv(i) {
            if let Ok(s) = core::str::from_utf8(arg) {
                print!("{}", s);
            }
        }
    }
    println!();
    syscall::sys_exit();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
