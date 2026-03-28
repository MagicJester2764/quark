#![no_std]
#![no_main]

use quark_rt::{args, println, syscall};

const MAX_TASKS: usize = 64;

/// ACPI PM1a Control register port (QEMU PIIX4 / i440fx).
const ACPI_PM1A_CNT: u16 = 0x604;

/// S5 sleep value: SLP_EN (bit 13) | SLP_TYP=S5 (bits 10-12, value varies).
/// QEMU i440fx/PIIX4 uses SLP_TYP=0 for S5, so just SLP_EN.
const ACPI_S5_VALUE: u16 = 1 << 13;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let force = has_flag(b"-f") || has_flag(b"--force");
    let my_tid = syscall::sys_getpid() as usize;

    // Phase 1: Signal all user tasks to terminate
    let mut signaled = 0usize;
    for tid in 2..MAX_TASKS {
        if tid == my_tid {
            continue;
        }
        if let Ok((state, _, _)) = syscall::sys_task_info(tid) {
            if state == 3 {
                continue; // Dead
            }
            let sig = if force { syscall::SIG_KILL } else { syscall::SIG_TERM };
            if syscall::sys_signal(tid, sig).is_ok() {
                signaled += 1;
            }
        }
    }

    if signaled > 0 {
        if force {
            println!("shutdown: killed {} tasks", signaled);
        } else {
            println!("shutdown: sent SIGTERM to {} tasks, waiting...", signaled);
            // Wait for tasks to exit gracefully (SIG_TERM has 2s kernel grace period)
            syscall::sleep_ms(2500);
        }
    }

    println!("shutdown: powering off...");

    // Phase 2: Force-kill any survivors
    if !force {
        for tid in 2..MAX_TASKS {
            if tid == my_tid {
                continue;
            }
            if let Ok((state, _, _)) = syscall::sys_task_info(tid) {
                if state != 3 {
                    let _ = syscall::sys_signal(tid, syscall::SIG_KILL);
                }
            }
        }
    }

    // Phase 3: ACPI S5 power-off
    syscall::sys_ioport_write16(ACPI_PM1A_CNT, ACPI_S5_VALUE);

    // If ACPI didn't work, try alternate QEMU ports
    syscall::sys_ioport_write16(0xB004, ACPI_S5_VALUE);

    // Last resort: HLT loop
    println!("shutdown: ACPI power-off failed, system halted");
    loop {
        core::hint::spin_loop();
    }
}

fn has_flag(flag: &[u8]) -> bool {
    let argc = args::argc();
    for i in 1..argc {
        if let Some(arg) = args::argv(i) {
            if arg == flag {
                return true;
            }
        }
    }
    false
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("shutdown: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
