//! Programmable Interval Timer (8253/8254) — Channel 0 rate generator.

use crate::io;
use core::sync::atomic::{AtomicU64, Ordering};

const PIT_CH0_DATA: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_FREQUENCY: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Set Channel 0 to mode 2 (rate generator) at the given frequency in Hz.
pub unsafe fn init(hz: u32) { unsafe {
    let divisor = PIT_FREQUENCY / hz;
    // Command: channel 0, lo/hi byte, mode 2 (rate generator)
    io::outb(PIT_CMD, 0x34);
    io::outb(PIT_CH0_DATA, (divisor & 0xFF) as u8);
    io::outb(PIT_CH0_DATA, ((divisor >> 8) & 0xFF) as u8);
}}

/// Called from the IRQ 0 handler: count the tick, see to what is due, and
/// charge the running task for its turn.
///
/// What time it is, is the clock's to say (`clock.rs`), and so is what is
/// due — fired by the first processor's own timer when it is due, so the
/// tick looks at it only in case something was not said. A processor with
/// nothing to run takes no tick at all (`scheduler::idle`).
///
/// Seeing to what is due may not come back — an alarm or a hang-up can end
/// the program this interrupted — and then the turn is not charged: the
/// program has gone.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::random::stir();
    crate::clock::expire(false);
    crate::scheduler::timer_tick();
}

/// How many times the 8254 has interrupted. Not the time — `clock::now` is
/// — though on a machine with no finer clock the time is made of it.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
