//! The I/O APIC: where devices' interrupts come in, on a machine that has one.
//!
//! The 8259 has sixteen lines, one processor to interrupt and an order of
//! importance wired into it. The I/O APIC has a line for each of the
//! machine's interrupts — twenty-four, usually — and a table saying, for
//! each, what to send and to which processor's local APIC: any vector, any
//! processor, and whether the line is to be read as a level or an edge.
//!
//! What the kernel does with it is kept small on purpose:
//!
//! - **The sixteen ISA interrupts keep their numbers**, and their vectors
//!   (32 and up), so nothing above this knows which controller there is: a
//!   driver registers for interrupt 11 and is told of interrupt 11. Where
//!   each one comes in is the firmware's to say, in the MADT's overrides —
//!   the clock, interrupt 0, has arrived on line 2 of every PC's I/O APIC
//!   since the day the second 8259 was hung off the first one's — and so is
//!   whether it is high or low, a level or an edge.
//! - **Every one is sent to the first processor.** It could be any; nothing
//!   yet gives a reason for another, and the clock that the whole system
//!   keeps time by is one of them.
//! - **A line that is a level is masked when it interrupts, and unmasked
//!   when its driver says the device has been dealt with** (`ack`). A level
//!   goes on interrupting for as long as the device asks, and the driver is
//!   a program that has not run yet. An edge is not masked: an edge that
//!   arrives while its line is masked is lost, where the 8259 would have
//!   remembered it.
//!
//! Interrupts above the sixteen — the ones a PCI device on a newer machine
//! is wired to — are not used. Which device is on which of those is in the
//! firmware's bytecode, not its tables; a device that wants one of its own
//! asks for a message instead (MSI).

use crate::acpi;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

const REG_SELECT: usize = 0x00;
const REG_WINDOW: usize = 0x10;

const IOAPIC_VERSION: u32 = 0x01;
const IOAPIC_TABLE: u32 = 0x10;

const ENTRY_LOW_ACTIVE: u32 = 1 << 13;
const ENTRY_LEVEL: u32 = 1 << 15;
const ENTRY_MASKED: u32 = 1 << 16;

/// The first vector of the sixteen: where the 8259s were told to put them.
pub const FIRST_VECTOR: u8 = 32;
const LINES: usize = 16;

/// Where an ISA interrupt comes in.
#[derive(Clone, Copy)]
struct Line {
    /// Whether there is anywhere for it to come in at all.
    there: bool,
    /// The I/O APIC's registers, and which of its lines.
    base: usize,
    pin: u32,
    /// A level, to be masked while its driver works; else an edge.
    level: bool,
    /// What its table entry holds, the mask bit aside.
    entry: u32,
}

const NO_LINE: Line = Line { there: false, base: 0, pin: 0, level: false, entry: 0 };

/// Written once, in [`init`], before anything can interrupt.
static mut LINES_AT: [Line; LINES] = [NO_LINE; LINES];
static IN_USE: AtomicBool = AtomicBool::new(false);
/// Which of the sixteen are unmasked as far as their drivers are concerned:
/// a level line that is masked only until its driver answers is still here.
static ENABLED: AtomicU32 = AtomicU32::new(0);

fn line(irq: u8) -> Option<Line> {
    if (irq as usize) >= LINES {
        return None;
    }
    let l = unsafe { (*(&raw const LINES_AT))[irq as usize] };
    l.there.then_some(l)
}

/// A register of the I/O APIC at `base`. Two steps — say which, then read
/// or write it — so nothing may come between them.
///
/// # Safety
/// Interrupts off.
unsafe fn read(base: usize, reg: u32) -> u32 {
    unsafe {
        core::ptr::write_volatile((base + REG_SELECT) as *mut u32, reg);
        core::ptr::read_volatile((base + REG_WINDOW) as *const u32)
    }
}

/// # Safety
/// Interrupts off.
unsafe fn write(base: usize, reg: u32, value: u32) {
    unsafe {
        core::ptr::write_volatile((base + REG_SELECT) as *mut u32, reg);
        core::ptr::write_volatile((base + REG_WINDOW) as *mut u32, value);
    }
}

fn with_interrupts_off<T>(f: impl FnOnce() -> T) -> T {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    let out = f();
    if flags & (1 << 9) != 0 {
        unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
    }
    out
}

/// Whether devices interrupt through this.
pub fn in_use() -> bool {
    IN_USE.load(Ordering::Relaxed)
}

/// Find where each of the sixteen ISA interrupts comes in, and set every
/// line of every I/O APIC to send nothing. `false` if the firmware lists
/// none that can be reached, which leaves the machine to its 8259.
///
/// `to` is the local APIC every interrupt is sent to.
///
/// # Safety
/// Once, on the first processor, interrupts off, after `acpi::init`.
pub unsafe fn init(to: u32) -> bool {
    let info = acpi::info();
    if !info.found || info.nioapics == 0 || to > 0xFF {
        return false;
    }
    // How many lines each has, and all of them masked: the firmware may
    // have left some going.
    let mut pins = [0u32; acpi::MAX_IOAPICS];
    for (i, io) in info.ioapics[..info.nioapics].iter().enumerate() {
        let base = io.addr as usize;
        // The kernel's map ends at four gigabytes; `addr` is 32 bits wide.
        if base == 0 {
            return false;
        }
        unsafe {
            pins[i] = ((read(base, IOAPIC_VERSION) >> 16) & 0xFF) + 1;
            for pin in 0..pins[i] {
                write(base, IOAPIC_TABLE + 2 * pin, ENTRY_MASKED);
                write(base, IOAPIC_TABLE + 2 * pin + 1, 0);
            }
        }
    }
    let lines = unsafe { &mut *(&raw mut LINES_AT) };
    for irq in 0..LINES as u32 {
        // Where the firmware says it comes in, and how; an interrupt it
        // says nothing about comes in on the line of its own number, an
        // edge, high — which is what an ISA interrupt is.
        let over = info.overrides[..info.noverrides].iter().find(|o| o.irq as u32 == irq);
        let (gsi, flags) = over.map_or((irq, 0), |o| (o.gsi, o.flags));
        // Another interrupt has been moved onto this one's line, and it
        // was not moved off: it does not exist. (2, where the second 8259
        // was, on a machine whose clock is on line 2.)
        if over.is_none() && info.overrides[..info.noverrides].iter().any(|o| o.gsi == irq) {
            continue;
        }
        let Some(i) = (0..info.nioapics).find(|&i| {
            let io = &info.ioapics[i];
            gsi >= io.gsi_base && gsi - io.gsi_base < pins[i]
        }) else {
            continue;
        };
        let low = flags & 0b11 == 0b11;
        let level = (flags >> 2) & 0b11 == 0b11;
        let entry = (FIRST_VECTOR as u32 + irq)
            | if low { ENTRY_LOW_ACTIVE } else { 0 }
            | if level { ENTRY_LEVEL } else { 0 };
        let l = Line {
            there: true,
            base: info.ioapics[i].addr as usize,
            pin: gsi - info.ioapics[i].gsi_base,
            level,
            entry,
        };
        unsafe {
            // To whom, first; then what, still masked.
            write(l.base, IOAPIC_TABLE + 2 * l.pin + 1, to << 24);
            write(l.base, IOAPIC_TABLE + 2 * l.pin, entry | ENTRY_MASKED);
        }
        lines[irq as usize] = l;
    }
    IN_USE.store(true, Ordering::Relaxed);
    true
}

fn set_masked(l: Line, masked: bool) {
    with_interrupts_off(|| unsafe {
        write(l.base, IOAPIC_TABLE + 2 * l.pin, l.entry | if masked { ENTRY_MASKED } else { 0 });
    });
}

/// Let interrupt `irq` through.
pub fn enable(irq: u8) {
    if let Some(l) = line(irq) {
        ENABLED.fetch_or(1 << irq, Ordering::Relaxed);
        set_masked(l, false);
    }
}

/// Stop interrupt `irq`: nobody is there to deal with it.
pub fn disable(irq: u8) {
    if let Some(l) = line(irq) {
        ENABLED.fetch_and(!(1 << irq), Ordering::Relaxed);
        set_masked(l, true);
    }
}

/// Interrupt `irq` has been taken and is being handed to a driver, which
/// has not run yet. A level would interrupt again the moment it could, so
/// its line is masked until the driver says it has dealt with the device.
pub fn held(irq: u8) {
    if let Some(l) = line(irq).filter(|l| l.level) {
        set_masked(l, true);
    }
}

/// The driver of `irq` has dealt with its device.
pub fn ack(irq: u8) {
    if let Some(l) = line(irq).filter(|l| l.level) {
        if ENABLED.load(Ordering::Relaxed) & (1 << irq) != 0 {
            set_masked(l, false);
        }
    }
}

/// Say on the serial line where the sixteen come in.
pub fn describe() {
    use crate::serial::{put_usize, puts};
    for irq in 0..LINES as u8 {
        let Some(l) = line(irq) else { continue };
        if l.pin != irq as u32 || l.level {
            puts(b"  interrupt ");
            put_usize(irq as usize);
            puts(b" on line ");
            put_usize(l.pin as usize);
            puts(if l.level { b", a level\n" } else { b", an edge\n" });
        }
    }
}
