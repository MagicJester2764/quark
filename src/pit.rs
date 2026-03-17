//! Programmable Interval Timer (8253/8254) — Channel 0 rate generator.

use crate::io;
use core::sync::atomic::{AtomicU64, Ordering};

const PIT_CH0_DATA: u16 = 0x40;
const PIT_CMD: u16 = 0x43;
const PIT_FREQUENCY: u32 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

/// Set Channel 0 to mode 2 (rate generator) at the given frequency in Hz.
pub unsafe fn init(hz: u32) {
    let divisor = PIT_FREQUENCY / hz;
    // Command: channel 0, lo/hi byte, mode 2 (rate generator)
    io::outb(PIT_CMD, 0x34);
    io::outb(PIT_CH0_DATA, (divisor & 0xFF) as u8);
    io::outb(PIT_CH0_DATA, ((divisor >> 8) & 0xFF) as u8);
}

/// Called from the IRQ 0 handler to bump the tick counter and trigger scheduling.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    crate::ipc::check_timeouts();
    crate::ipc::check_signal_deadlines();
    crate::scheduler::timer_tick();
}

/// Return the current tick count.
#[allow(dead_code)]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}
