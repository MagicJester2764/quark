//! 8259A dual PIC driver.
//!
//! Remaps IRQ 0–7 to vectors 32–39 and IRQ 8–15 to vectors 40–47,
//! then masks all IRQs until individually enabled.

use crate::io;

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x11; // ICW1: init + ICW4 needed
const ICW4_8086: u8 = 0x01; // ICW4: 8086 mode

const PIC_EOI: u8 = 0x20;
const PIC_READ_ISR: u8 = 0x0B;

/// Initialize both PICs: remap IRQs, mask all lines.
pub unsafe fn init() {
    // Save current masks
    let mask1 = io::inb(PIC1_DATA);
    let mask2 = io::inb(PIC2_DATA);

    // ICW1: begin initialization sequence
    io::outb(PIC1_CMD, ICW1_INIT);
    io::io_wait();
    io::outb(PIC2_CMD, ICW1_INIT);
    io::io_wait();

    // ICW2: vector offsets
    io::outb(PIC1_DATA, 32); // IRQ 0–7 → vectors 32–39
    io::io_wait();
    io::outb(PIC2_DATA, 40); // IRQ 8–15 → vectors 40–47
    io::io_wait();

    // ICW3: wiring between master and slave
    io::outb(PIC1_DATA, 4); // slave on IRQ 2
    io::io_wait();
    io::outb(PIC2_DATA, 2); // slave cascade identity
    io::io_wait();

    // ICW4: 8086 mode
    io::outb(PIC1_DATA, ICW4_8086);
    io::io_wait();
    io::outb(PIC2_DATA, ICW4_8086);
    io::io_wait();

    // Mask all IRQs (caller will enable specific ones)
    io::outb(PIC1_DATA, 0xFF);
    io::outb(PIC2_DATA, 0xFF);

    // Suppress unused-variable warnings — masks saved for reference
    let _ = (mask1, mask2);
}

/// Enable (unmask) a specific IRQ line (0–15).
pub unsafe fn enable_irq(irq: u8) {
    if irq < 8 {
        let mask = io::inb(PIC1_DATA) & !(1 << irq);
        io::outb(PIC1_DATA, mask);
    } else {
        let mask = io::inb(PIC2_DATA) & !(1 << (irq - 8));
        io::outb(PIC2_DATA, mask);
        // Also unmask cascade line (IRQ 2) on master
        let master = io::inb(PIC1_DATA) & !(1 << 2);
        io::outb(PIC1_DATA, master);
    }
}

/// Disable (mask) a specific IRQ line (0–15).
#[allow(dead_code)]
pub unsafe fn disable_irq(irq: u8) {
    if irq < 8 {
        let mask = io::inb(PIC1_DATA) | (1 << irq);
        io::outb(PIC1_DATA, mask);
    } else {
        let mask = io::inb(PIC2_DATA) | (1 << (irq - 8));
        io::outb(PIC2_DATA, mask);
    }
}

/// Send End-Of-Interrupt. For IRQ >= 8, send to both slave and master.
pub unsafe fn send_eoi(irq: u8) {
    if irq >= 8 {
        io::outb(PIC2_CMD, PIC_EOI);
    }
    io::outb(PIC1_CMD, PIC_EOI);
}

/// Read the combined 16-bit In-Service Register (ISR).
/// Bits 0–7 = master, bits 8–15 = slave.
pub unsafe fn read_isr() -> u16 {
    io::outb(PIC1_CMD, PIC_READ_ISR);
    io::outb(PIC2_CMD, PIC_READ_ISR);
    let lo = io::inb(PIC1_CMD) as u16;
    let hi = io::inb(PIC2_CMD) as u16;
    lo | (hi << 8)
}
