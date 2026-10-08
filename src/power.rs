//! Turning the machine off, and starting it again.
//!
//! Both are things the firmware says how to do, in its tables (`acpi.rs`),
//! and for a long time both were done by a program in ring 3 writing to
//! ports that QEMU happens to listen on: the right one for the machine QEMU
//! pretends to be, and nothing at all on any other.
//!
//! **Off** is a value written to a control register. Which register, the
//! FADT says. Which value, it does not: that is in the machine's own table
//! of methods (the DSDT), as an object named `\_S5` — the fifth sleeping
//! state, the one nothing wakes from. Reading that table properly takes an
//! interpreter for the language it is written in; reading that one object
//! out of it takes knowing what four names in a row look like, which is
//! what every small kernel does and what is done here (`acpi::s5`).
//!
//! **Starting again** is a value written to the reset register, where the
//! tables name one. Where they do not, or it does nothing, there is the
//! keyboard controller's reset line, which every PC has had since the AT;
//! and when that does nothing either, a fault the processor cannot deliver
//! — three in a row — resets it, which no PC has ever failed to do.
//!
//! Neither comes back. Everything else is stopped first: the other
//! processors, by the message a kernel fault stops them with. What a
//! machine about to go off owes its disks is user space's to see to before
//! it asks — the file servers are programs — and `shutdown` does.

use crate::acpi;
use crate::io::{inb, inw, outb, outw};

/// The bit of a control register that says the firmware has handed the
/// power-management hardware over to the operating system.
const SCI_EN: u16 = 1 << 0;
/// Where the kind of sleep goes in a control register, and the bit that
/// says "now".
const SLP_TYP_SHIFT: u16 = 10;
const SLP_TYP_MASK: u16 = 0b111 << SLP_TYP_SHIFT;
const SLP_EN: u16 = 1 << 13;

/// Roughly so many milliseconds, with nothing to count them by that can be
/// relied on here: interrupts are off, and the clock may be the tick. A read
/// of port 0x80 takes about a microsecond on anything.
fn wait_ms(ms: u32) {
    for _ in 0..ms.saturating_mul(1000) {
        unsafe { inb(0x80) };
    }
}

/// Whether the machine can be turned off by its firmware's tables.
pub fn can_turn_off() -> bool {
    let info = acpi::info();
    info.s5.is_some() && info.pm1a_cnt != 0 && info.pm1a_cnt <= 0xFFFF
}

fn stop_everything_else() {
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    crate::smp::halt_others();
}

/// Turn the machine off. Comes back only if it could not be: there is
/// nothing in the tables to do it with, or it was done and the machine is
/// still here. The other processors are stopped by then, and stay stopped:
/// whoever asked is the only thing running, and decides what is left to try.
pub fn off() {
    let info = acpi::info();
    let Some((a, b)) = info.s5.filter(|_| can_turn_off()) else {
        return;
    };
    stop_everything_else();
    crate::kstack::say_deepest();
    crate::heap::say_usage();
    crate::serial::puts(b"Power: off.\n");
    let port = info.pm1a_cnt as u16;
    unsafe {
        // The firmware's until it is asked for, on a machine that started
        // with it: asked for by a value written to a port, both of them the
        // tables', and given when the bit says so.
        if inw(port) & SCI_EN == 0 && info.smi_cmd != 0 && info.smi_cmd <= 0xFFFF && info.acpi_enable != 0 {
            outb(info.smi_cmd as u16, info.acpi_enable);
            for _ in 0..300 {
                if inw(port) & SCI_EN != 0 {
                    break;
                }
                wait_ms(10);
            }
        }
        // The kind of sleep first and then the word to do it, as two
        // writes: some hardware wants to see the kind before it is told.
        let sleep = |port: u16, kind: u8| {
            let keep = inw(port) & !(SLP_TYP_MASK | SLP_EN);
            let value = keep | (kind as u16) << SLP_TYP_SHIFT;
            outw(port, value);
            outw(port, value | SLP_EN);
        };
        if info.pm1b_cnt != 0 && info.pm1b_cnt <= 0xFFFF {
            sleep(info.pm1b_cnt as u16, b);
        }
        sleep(port, a);
    }
    wait_ms(1000);
    crate::serial::puts(b"Power: the machine did not turn off.\n");
}

/// Start the machine again. Does not come back.
pub fn restart() -> ! {
    let info = acpi::info();
    stop_everything_else();
    crate::kstack::say_deepest();
    crate::heap::say_usage();
    crate::serial::puts(b"Power: restarting.\n");
    unsafe {
        if let Some(reset) = info.reset {
            match reset.space {
                1 if reset.addr <= 0xFFFF => outb(reset.addr as u16, info.reset_value),
                0 if reset.addr < 1 << 32 => core::ptr::write_volatile(reset.addr as *mut u8, info.reset_value),
                _ => {}
            }
            wait_ms(100);
        }
        // The keyboard controller's reset line: wait for it to be ready to
        // be told, and pulse it.
        for _ in 0..1000 {
            if inb(0x64) & 0x02 == 0 {
                break;
            }
            wait_ms(1);
        }
        outb(0x64, 0xFE);
        wait_ms(100);
        // And what cannot fail: an interrupt with nowhere to go, whose
        // fault has nowhere to go, whose fault has nowhere to go.
        let nowhere = [0u8; 10];
        core::arch::asm!("lidt [{}]", "int3", in(reg) nowhere.as_ptr(), options(nostack));
    }
    crate::smp::halt_here()
}
