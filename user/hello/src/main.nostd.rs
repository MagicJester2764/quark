#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use quark_rt::{args, println, syscall};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Print program arguments
    let argc = args::argc();
    println!("argc={}", argc);
    for i in 0..argc {
        if let Some(arg) = args::argv(i) {
            if let Ok(s) = core::str::from_utf8(arg) {
                println!("  argv[{}] = \"{}\"", i, s);
            }
        }
    }

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

    // Test sleep
    let t0 = syscall::sys_ticks();
    println!("Sleeping 500ms (tick {})...", t0);
    syscall::sleep_ms(500);
    let t1 = syscall::sys_ticks();
    println!("Woke up at tick {} ({}ms elapsed)", t1, (t1 - t0) * 10);

    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[hello] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
