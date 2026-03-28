#![no_std]
#![no_main]

use quark_rt::{println, syscall};

const MAX_TASKS: usize = 64;

const STATE_NAMES: [&str; 4] = ["READY", "RUN", "BLOCK", "DEAD"];

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("  TID  STATE  UID  PPID");
    for tid in 0..MAX_TASKS {
        if let Ok((state, parent, uid)) = syscall::sys_task_info(tid) {
            if state == 3 { continue; } // skip Dead tasks
            let state_str = if (state as usize) < STATE_NAMES.len() {
                STATE_NAMES[state as usize]
            } else {
                "???"
            };
            println!("{:5}  {:5}  {:3}  {:4}", tid, state_str, uid, parent);
        }
    }
    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("ps: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
