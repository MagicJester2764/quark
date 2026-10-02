//! The interrupt controller devices come in through: whichever there is.
//!
//! A machine has a pair of 8259s, or an I/O APIC, or both and a choice.
//! Everything above this — the interrupt handler, the queue a driver reads
//! its interrupts from, the two calls a driver makes — speaks of an
//! interrupt by its ISA number and asks for one of five things. What each
//! means is the controller's:
//!
//! | | 8259 | I/O APIC |
//! |---|---|---|
//! | `enable` | unmask the line | unmask the line |
//! | `disable` | mask it | mask it |
//! | `done` — the kernel dealt with it | end of interrupt | end of interrupt, at the local APIC |
//! | `held` — a driver will | nothing: the 8259 holds the line, and those below it, until told | mask it if it is a level, and end of interrupt |
//! | `dropped` — a driver would have, and cannot be told | end of interrupt | as `held` |
//! | `ack` — the driver has | end of interrupt | unmask it if it is a level |
//!
//! The difference between the last two columns is the reason for this
//! file. An 8259 that has not been told an interrupt is over delivers no
//! more of that importance or less, which is how a level was kept from
//! interrupting again while its driver had yet to run — and how one driver
//! that was slow to answer held up every device below it. A local APIC
//! does the same by vector, and every device's vector is in one class, so
//! there it is not a use but a stall; it is told at once, and a level is
//! kept quiet by masking its own line and nobody else's.
//!
//! The I/O APIC is used when the firmware's tables list one and there is a
//! local APIC to deliver through. The 8259s are then masked, all sixteen
//! lines, and stay so.

use crate::{ioapic, lapic, pic};

/// An interrupt that is no controller's line: a message a device sends
/// straight to a local APIC (MSI), numbered from sixteen
/// (`irq_dispatch::allocate_message`). It is an edge with nothing to mask
/// here — whether it is sent is the device's own switch — so all there is
/// to say about one is that it has been taken.
fn message(irq: u8) -> bool {
    irq as usize >= crate::irq_dispatch::FIRST_MESSAGE
}

/// Find the controller and leave every line masked.
///
/// # Safety
/// Once, on the first processor, interrupts off, after `acpi::init`.
pub unsafe fn init() {
    unsafe {
        // Whichever is used, the 8259s are told where their vectors are
        // and to send nothing: a stray one then arrives where it is known
        // for what it is.
        pic::init();
        if lapic::init() && ioapic::init(lapic::id()) {
            // The first processor has been taking the 8259's interrupts
            // through its local APIC, as a wire. No longer.
            lapic::no_legacy_wire();
        }
    }
}

/// Let interrupt `irq` through.
pub fn enable(irq: u8) {
    if message(irq) {
        return;
    }
    if ioapic::in_use() {
        ioapic::enable(irq);
    } else {
        unsafe { pic::enable_irq(irq) };
    }
}

/// Stop interrupt `irq`: nobody is there to deal with it.
pub fn disable(irq: u8) {
    if message(irq) {
        return;
    }
    if ioapic::in_use() {
        ioapic::disable(irq);
    } else {
        unsafe { pic::disable_irq(irq) };
    }
}

/// The kernel has dealt with interrupt `irq` itself.
pub fn done(irq: u8) {
    if message(irq) || ioapic::in_use() {
        lapic::eoi();
    } else {
        unsafe { pic::send_eoi(irq) };
    }
}

/// Interrupt `irq` has been handed to a driver, which will say when it has
/// dealt with the device ([`ack`]).
pub fn held(irq: u8) {
    if message(irq) {
        lapic::eoi();
    } else if ioapic::in_use() {
        ioapic::held(irq);
        lapic::eoi();
    }
}

/// Interrupt `irq` was for a driver that has no room to be told of it, and
/// so will not answer for it.
pub fn dropped(irq: u8) {
    if message(irq) || ioapic::in_use() {
        held(irq);
    } else {
        unsafe { pic::send_eoi(irq) };
    }
}

/// The driver of `irq` has dealt with its device.
pub fn ack(irq: u8) {
    if message(irq) {
        return;
    }
    if ioapic::in_use() {
        ioapic::ack(irq);
    } else {
        unsafe { pic::send_eoi(irq) };
    }
}

/// Whether what arrived as `irq` is nothing: an 8259 answers with its last
/// line (7, or 15) when it is asked which line interrupted and by then
/// none is. It has to be told nothing, or — for the second of the pair —
/// only the first has.
pub fn spurious(irq: u8) -> bool {
    if ioapic::in_use() || (irq != 7 && irq != 15) {
        return false;
    }
    let isr = unsafe { pic::read_isr() };
    if isr & (1 << irq) != 0 {
        return false;
    }
    if irq == 15 {
        unsafe { pic::send_eoi(0) };
    }
    true
}
