#![no_std]
#![no_main]

//! Proof that a program init has never heard of still gets exactly the
//! authority it asks for, and nothing else.
//!
//! Nothing in `init` or the shell names this program. Its capabilities come
//! only from the manifest below, which is why it is worth keeping around: if
//! granting ever regresses to a table of names, this stops working.

use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall};

// Eight pages, and deliberately no I/O ports.
quark_rt::manifest!([CapReq::phys_alloc(8)]);

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("capdemo: a program init does not know about");

    // Asked for: should be granted.
    match syscall::sys_phys_alloc(1) {
        Ok(frame) => {
            println!("  phys_alloc      -> ok (frame 0x{:x})  [in manifest]", frame);
            let _ = syscall::sys_phys_free(frame, 1);
        }
        Err(()) => println!("  phys_alloc      -> REFUSED  [should have been granted]"),
    }

    // Not asked for: should be refused. Port 0x60 is the keyboard's, and this
    // program has no business touching it.
    // sys_ioport_read returns u64::MAX when the capability check refuses it.
    let v = syscall::sys_ioport_read(0x60);
    if v == u64::MAX {
        println!("  ioport 0x60     -> refused  [not in manifest]");
    } else {
        println!("  ioport 0x60     -> ok (0x{:x})  [SHOULD HAVE BEEN REFUSED]", v);
    }

    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
