//! The local APIC: the interrupt controller each processor has to itself.
//!
//! The kernel wants three things of it, and all three are about there
//! being more than one processor:
//!
//! - **An interrupt from one processor to another.** It is the only way one
//!   can tell another anything: that there is work, that the task it is
//!   running has been ended, that a translation it may be holding is no
//!   longer true.
//! - **A timer of its own.** The 8254 interrupts one processor. Every other
//!   needs a tick to end a task's turn with, and its local APIC has one.
//!   The first processor's is set, a shot at a time, for whatever is due
//!   before the next tick (`clock.rs`).
//! - **The way the others are started**: INIT and STARTUP are messages sent
//!   through it.
//!
//! And a fourth, which is about devices: an I/O APIC delivers a device's
//! interrupt as a message to a local APIC (`ioapic.rs`), so on a machine
//! with one this is how every device interrupt arrives, even with one
//! processor. Where there is none, devices interrupt through the 8259 and
//! the first processor's local APIC passes them on as the firmware set it
//! up to.
//!
//! Registers are reached through memory, or through MSRs where the firmware
//! left the APIC in x2APIC mode — which it must on a machine with more than
//! 255 processors, and may on any.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

const MSR_APIC_BASE: u32 = 0x1B;
const APIC_BASE_X2: u64 = 1 << 10;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const MSR_X2APIC: u32 = 0x800;

const REG_ID: u32 = 0x20;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xB0;
const REG_SPURIOUS: u32 = 0xF0;
const REG_ERROR: u32 = 0x280;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_INITIAL: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3E0;

const SPURIOUS_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const TIMER_PERIODIC: u32 = 1 << 17;
/// Count the bus clock in sixteens.
const TIMER_DIVIDE_16: u32 = 0b0011;

const ICR_INIT: u32 = 0b101 << 8;
const ICR_STARTUP: u32 = 0b110 << 8;
const ICR_PENDING: u32 = 1 << 12;
const ICR_ASSERT: u32 = 1 << 14;
const ICR_LEVEL: u32 = 1 << 15;

/// Whether there is a local APIC the kernel is using.
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Whether its registers are MSRs.
static X2: AtomicBool = AtomicBool::new(false);
/// Where its registers are, when they are memory.
static BASE: AtomicUsize = AtomicUsize::new(0);
/// How far the timer counts in one tick of the system's clock, counting in
/// sixteens. Measured once, on the first processor; the processors of one
/// machine share a bus clock.
static PER_TICK: AtomicU32 = AtomicU32::new(0);

fn read(reg: u32) -> u32 {
    if X2.load(Ordering::Relaxed) {
        crate::cpu::rdmsr(MSR_X2APIC + (reg >> 4)) as u32
    } else {
        unsafe { core::ptr::read_volatile((BASE.load(Ordering::Relaxed) + reg as usize) as *const u32) }
    }
}

fn write(reg: u32, value: u32) {
    if X2.load(Ordering::Relaxed) {
        unsafe { crate::cpu::wrmsr(MSR_X2APIC + (reg >> 4), value as u64) };
    } else {
        unsafe { core::ptr::write_volatile((BASE.load(Ordering::Relaxed) + reg as usize) as *mut u32, value) };
    }
}

/// Whether the kernel is using local APICs: whether [`init`] found one.
pub fn present() -> bool {
    PRESENT.load(Ordering::Relaxed)
}

/// Find the first processor's local APIC and set it up. `false` if there is
/// none to use, which leaves the machine as it was: one processor and the
/// 8259.
///
/// # Safety
/// Once, on the first processor, interrupts off.
pub unsafe fn init() -> bool {
    // Asked for twice: by whoever sets up devices' interrupts, and by
    // whoever starts the other processors.
    if PRESENT.load(Ordering::Relaxed) {
        return true;
    }
    if !crate::cpu::has_apic() {
        return false;
    }
    let base = crate::cpu::rdmsr(MSR_APIC_BASE);
    if base & APIC_BASE_ENABLE == 0 {
        // Turned off by the firmware. Turning it back on is allowed on some
        // processors and not on others, and a machine whose firmware did
        // that is not one to start more processors on.
        return false;
    }
    if base & APIC_BASE_X2 != 0 {
        X2.store(true, Ordering::Relaxed);
    } else {
        let addr = base & 0x000F_FFFF_FFFF_F000;
        // The kernel's own map ends at four gigabytes.
        if addr == 0 || addr >= 1 << 32 {
            return false;
        }
        BASE.store(addr as usize, Ordering::Relaxed);
    }
    PRESENT.store(true, Ordering::Relaxed);
    unsafe { init_local(true) };
    true
}

/// Set up the local APIC of the processor this runs on.
///
/// The first processor's two interrupt pins are left as the firmware has
/// them: one of them is how the 8259's interrupts reach it, and the clock
/// and every device are behind that. The others take neither pin: an
/// interrupt from a device is for one processor.
///
/// # Safety
/// Once per processor, on that processor, interrupts off.
pub unsafe fn init_local(first: bool) {
    write(REG_SPURIOUS, SPURIOUS_ENABLE | crate::idt::VEC_SPURIOUS as u32);
    write(REG_TPR, 0);
    write(REG_LVT_TIMER, LVT_MASKED);
    write(REG_LVT_ERROR, LVT_MASKED);
    if !first {
        write(REG_LVT_LINT0, LVT_MASKED);
        write(REG_LVT_LINT1, LVT_MASKED);
    }
    // The error register is read by writing it first; twice clears it.
    write(REG_ERROR, 0);
    write(REG_ERROR, 0);
}

/// Stop taking the 8259's interrupts through this processor's local APIC:
/// the pin they came in on is masked. For the first processor, once devices
/// interrupt through the I/O APIC instead.
///
/// # Safety
/// On the first processor, interrupts off.
pub unsafe fn no_legacy_wire() {
    write(REG_LVT_LINT0, LVT_MASKED);
}

/// The id this processor's local APIC answers to.
pub fn id() -> u32 {
    if X2.load(Ordering::Relaxed) {
        read(REG_ID)
    } else {
        read(REG_ID) >> 24
    }
}

/// Say an interrupt that came through the local APIC has been taken.
#[inline]
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Send `low` — a kind of message, and for most kinds a vector — to the
/// processor whose local APIC is `apic_id`, and wait for it to be sent.
///
/// With the registers in memory it is two writes to one processor's APIC,
/// so interrupts are held off across them: an interrupt between the two
/// that sent a message of its own would send this one to its destination,
/// and a task moved between them would write the halves to two APICs.
fn send_raw(apic_id: u32, low: u32) {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
        if X2.load(Ordering::Relaxed) {
            crate::cpu::wrmsr(MSR_X2APIC + (REG_ICR_LOW >> 4), (apic_id as u64) << 32 | low as u64);
        } else {
            write(REG_ICR_HIGH, apic_id << 24);
            write(REG_ICR_LOW, low);
            while read(REG_ICR_LOW) & ICR_PENDING != 0 {
                core::hint::spin_loop();
            }
        }
        if flags & (1 << 9) != 0 {
            core::arch::asm!("sti", options(nostack, nomem));
        }
    }
}

/// Interrupt another processor, with `vector`.
pub fn send(apic_id: u32, vector: u8) {
    send_raw(apic_id, ICR_ASSERT | vector as u32);
}

/// Reset another processor: it stops, and waits to be told where to start.
/// Asserted and then taken away, which is what the oldest local APICs need
/// and the rest ignore.
pub fn send_init(apic_id: u32) {
    send_raw(apic_id, ICR_INIT | ICR_LEVEL | ICR_ASSERT);
    send_raw(apic_id, ICR_INIT | ICR_LEVEL);
}

/// Tell a processor that has been reset where to start: in real mode, at
/// the beginning of page `page` of the first megabyte.
pub fn send_startup(apic_id: u32, page: u8) {
    send_raw(apic_id, ICR_STARTUP | ICR_ASSERT | page as u32);
}

/// Find out how far the timer counts in one tick of the 8254. `false` if it
/// does not count.
///
/// Against the tick itself, for a machine with no finer clock to measure it
/// by: one that has was measured when the clock was started
/// ([`start_one_shot`]). Interrupts must be on: it watches the tick count
/// change.
pub fn calibrate() -> bool {
    if PER_TICK.load(Ordering::Relaxed) != 0 {
        return true;
    }
    const OVER: u64 = 5;
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
    // From the edge of one tick to the edge of the fifth after it.
    let start = crate::pit::ticks();
    while crate::pit::ticks() == start {
        core::hint::spin_loop();
    }
    write(REG_TIMER_INITIAL, u32::MAX);
    let from = crate::pit::ticks();
    while crate::pit::ticks() < from + OVER {
        core::hint::spin_loop();
    }
    let counted = u32::MAX - read(REG_TIMER_CURRENT);
    write(REG_TIMER_INITIAL, 0);
    let per_tick = counted / OVER as u32;
    PER_TICK.store(per_tick, Ordering::Relaxed);
    per_tick != 0
}

/// Start this processor's tick: an interrupt as often as the 8254's.
///
/// # Safety
/// After [`init_local`] on this processor and [`calibrate`] on the first.
pub unsafe fn start_timer() {
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
    write(REG_LVT_TIMER, TIMER_PERIODIC | crate::idt::VEC_TIMER as u32);
    write(REG_TIMER_INITIAL, PER_TICK.load(Ordering::Relaxed));
}

/// How far the timer counts in a tick, by the clock: the most of three
/// short measurements. Whatever goes wrong with one — this processor taken
/// away between starting the timer and reading the time, or between reading
/// the timer and reading the time — makes it come out too few, never too
/// many.
fn measure_by_clock() -> u32 {
    use crate::clock::{now, TICK_NS};
    const OVER_NS: u64 = 4_000_000;
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
    write(REG_LVT_TIMER, LVT_MASKED);
    let mut most = 0u64;
    for _ in 0..3 {
        let from = now();
        write(REG_TIMER_INITIAL, u32::MAX);
        while now() < from + OVER_NS {
            core::hint::spin_loop();
        }
        let counted = (u32::MAX - read(REG_TIMER_CURRENT)) as u64;
        let took = now() - from;
        write(REG_TIMER_INITIAL, 0);
        most = most.max(counted * TICK_NS / took.max(1));
    }
    most.min(u32::MAX as u64) as u32
}

/// Make this processor's timer one that is set a shot at a time and raises
/// `idt::VEC_CLOCK` when the shot is spent. `false` if there is no timer to
/// do it with. Leaves it not set.
///
/// The first processor's, which is ticked by the 8254 and has no other use
/// for its own.
///
/// # Safety
/// On the first processor, interrupts off, with the clock already fine.
pub unsafe fn start_one_shot() -> bool {
    if !present() {
        return false;
    }
    if PER_TICK.load(Ordering::Relaxed) == 0 {
        PER_TICK.store(measure_by_clock(), Ordering::Relaxed);
    }
    if PER_TICK.load(Ordering::Relaxed) == 0 {
        return false;
    }
    write(REG_TIMER_DIVIDE, TIMER_DIVIDE_16);
    write(REG_TIMER_INITIAL, 0);
    // Neither periodic nor masked: it counts down once and interrupts.
    write(REG_LVT_TIMER, crate::idt::VEC_CLOCK as u32);
    true
}

/// Set this processor's timer to interrupt `ns` nanoseconds from now,
/// instead of whenever it was set for. No more than two ticks away: the
/// tick sets it again.
///
/// A thousandth late rather than at all early: an interrupt before the time
/// finds nothing due and has to be taken again.
pub fn one_shot(ns: u64) {
    let per_tick = PER_TICK.load(Ordering::Relaxed) as u64;
    let ns = ns.min(2 * crate::clock::TICK_NS);
    let count = ns * per_tick / crate::clock::TICK_NS;
    write(REG_TIMER_INITIAL, (count + count / 1024 + 1).min(u32::MAX as u64) as u32);
}

/// Take back whatever this processor's timer was set for.
pub fn cancel_one_shot() {
    if present() {
        write(REG_TIMER_INITIAL, 0);
    }
}
