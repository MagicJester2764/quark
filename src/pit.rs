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
/// due; the tick is when everything is looked at whether or not anything
/// asked to be.
///
/// The order is that of what may not come back. Raising an alarm may end
/// the program this interrupted, and so may hanging up on a group a death
/// left stopped: what follows either is then left for the next tick, which
/// is why each is something that can wait for one.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::random::stir();
    crate::ipc::check_signal_deadlines();
    crate::clock::expire(false);
    // The groups a death left stopped with nobody to start them.
    crate::job::hang_up();
    crate::scheduler::timer_tick();
}

/// How many times the 8254 has interrupted. Not the time — `clock::now` is
/// — though on a machine with no finer clock the time is made of it.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
