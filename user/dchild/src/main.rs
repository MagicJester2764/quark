#![no_std]
#![no_main]

//! The other half of `dtest`'s cross-process check.
//!
//! Started by `dtest` with one end of a socketpair already at descriptor 3.
//! Allocates memory, writes a witness into it, and sends the descriptor back —
//! which is what a Wayland client does with `wl_shm`, minus the drawing.

use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall};

quark_rt::manifest!([CapReq::phys_alloc(16)]);

const CONN: usize = 3;
const MINE: usize = 0x97_0000_0000;
const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Wait for the parent's byte before answering, so this proves the stream
    // carries data in both directions between address spaces.
    let mut buf = [0u8; 8];
    let n = syscall::sys_fd_read(CONN, &mut buf);
    if n != 4 || &buf[..4] != b"go!\n" {
        println!("[dchild] bad greeting: {} bytes", n);
        syscall::sys_exit_code(2);
    }

    let Ok(mem) = syscall::sys_memfd_create(2) else {
        println!("[dchild] no memory");
        syscall::sys_exit_code(3);
    };
    if syscall::sys_mmap_fd(mem, MINE).is_err() {
        println!("[dchild] cannot map my own memory");
        syscall::sys_exit_code(4);
    }
    unsafe { core::ptr::write_volatile(MINE as *mut u64, WITNESS) };

    if syscall::sys_fd_send(CONN, b"here", Some(mem)) != Ok(4) {
        println!("[dchild] send failed");
        syscall::sys_exit_code(5);
    }
    println!("[dchild] sent");
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dchild] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
