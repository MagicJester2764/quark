//! Floating-point and SSE state, one copy per task.
//!
//! The kernel is built soft-float and never touches these registers, but every
//! user program may, and there is only one set of them per CPU. Without a save
//! area per task, a task preempted in the middle of a floating-point
//! computation resumes holding whatever the last task to run left behind — and
//! can read it, which is one task observing another's data.
//!
//! That went unnoticed until pixman, because nothing before it did much
//! floating-point work. Its own test suite failed on Quark with blend results
//! equal to the destination pixel and a region check that no longer held.
//!
//! Eager rather than lazy. Saving on every switch costs one FXSAVE and one
//! FXRSTOR — five hundred and twelve bytes each way — and lazy switching, which
//! defers that with CR0.TS until a task actually touches the registers, is how
//! the LazyFP vulnerability happened.
//!
//! **FXSAVE is only enough while CR4.OSXSAVE is clear.** It covers x87, MMX and
//! SSE, and nothing wider. With OSXSAVE clear an AVX instruction faults in user
//! space, so there is no wider state to lose. Setting OSXSAVE — to let programs
//! use AVX — without moving this to XSAVE and its full component mask would
//! reopen exactly this hole for the upper halves of the YMM registers.

/// The area FXSAVE writes: the x87 environment and stack, MXCSR, and the
/// sixteen XMM registers, in the processor's own format.
///
/// Aligned to 64 bytes rather than the 16 the instruction asks for, so that a
/// change to XSAVE, whose area wants 64, does not also have to change this.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct FpuState([u8; 512]);

/// The state every task starts in: the x87 unit reset, and MXCSR at its
/// power-on value — every exception masked, round to nearest.
///
/// Captured from the processor rather than written out by hand, so that the
/// reserved fields are whatever this processor considers valid.
static mut CLEAN: FpuState = FpuState([0; 512]);

/// Default MXCSR: all six exceptions masked, rounding to nearest.
const MXCSR_DEFAULT: u32 = 0x1F80;

/// Reset the unit and capture the clean state. Called once, at boot, after
/// `boot.s` has cleared CR0.EM and set CR4.OSFXSR — without those every
/// instruction below faults.
pub fn init() {
    let mxcsr = MXCSR_DEFAULT;
    unsafe {
        core::arch::asm!(
            "fninit",
            "ldmxcsr [{m}]",
            "fxsave64 [{area}]",
            m = in(reg) &mxcsr as *const u32,
            area = in(reg) &raw mut CLEAN,
            options(nostack, preserves_flags),
        );
    }
}

/// A copy of the state a new task should start in.
pub fn clean() -> FpuState {
    unsafe { CLEAN }
}

/// Save the processor's current floating-point state into `area`.
///
/// # Safety
/// `area` must be valid for writes of an `FpuState`.
pub unsafe fn save(area: *mut FpuState) {
    unsafe {
        core::arch::asm!(
            "fxsave64 [{}]",
            in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}

/// Load the processor's floating-point state from `area`.
///
/// # Safety
/// `area` must hold a state `save` or `clean` produced; FXRSTOR faults on a
/// reserved MXCSR bit, and an arbitrary buffer will have some set.
pub unsafe fn restore(area: *const FpuState) {
    unsafe {
        core::arch::asm!(
            "fxrstor64 [{}]",
            in(reg) area,
            options(nostack, preserves_flags),
        );
    }
}
