#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libquark::{println, syscall};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("Hello from user space!");

    // Test Vec
    let mut v: Vec<u32> = Vec::new();
    for i in 0..10 {
        v.push(i * i);
    }
    println!("Vec: {:?}", v.as_slice());

    // Test String
    let mut s = String::from("Quark");
    s.push_str(" has a heap!");
    println!("{}", s);

    // Test larger allocation
    let mut big: Vec<u8> = Vec::with_capacity(8192);
    for i in 0..8192u16 {
        big.push((i & 0xFF) as u8);
    }
    println!("Big vec len: {}", big.len());

    // Test deallocation + reuse
    drop(big);
    let reuse: Vec<u64> = (0..100).collect();
    println!("Reuse vec len: {}", reuse.len());

    println!("Heap test passed!");
    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[hello] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
