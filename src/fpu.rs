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
//! **How much there is to save depends on the processor**, and on what the
//! kernel has told it programs may use. FXSAVE covers x87, MMX and SSE and
//! nothing wider; with CR4.OSXSAVE clear an AVX instruction faults in user
//! space, so there was nothing wider to lose, and for a long time that is
//! how it was left. Programs may use AVX now, and AVX-512 where the
//! processor has it: [`init`] turns on what it finds and can hold, and the
//! state is saved with XSAVE, which writes all of it. The one thing that
//! must never be true is the first without the second — OSXSAVE set and
//! FXSAVE doing the saving — because that hands one task the upper halves of
//! another's registers, and nothing faults to say so.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The most a task's state comes to: the 512 bytes FXSAVE writes, XSAVE's
/// header of 64, the upper halves of sixteen YMM registers (256), and
/// AVX-512's three parts — eight mask registers (64), the upper halves of
/// sixteen ZMM registers (512) and sixteen more ZMM registers whole (1024).
/// Those are where XSAVE puts each, in its standard form, and the kernel
/// turns on nothing that would need more.
pub const AREA_SIZE: usize = 2688;

/// The area XSAVE (or, on a processor without it, FXSAVE) writes, in the
/// processor's own format, aligned to the 64 bytes XSAVE asks for.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct FpuState([u8; AREA_SIZE]);

/// The state every task starts in: the x87 unit reset, MXCSR at its
/// power-on value — every exception masked, round to nearest — and every
/// wider register nought.
static mut CLEAN: FpuState = FpuState([0; AREA_SIZE]);

/// Default MXCSR: all six exceptions masked, rounding to nearest.
const MXCSR_DEFAULT: u32 = 0x1F80;

/// Whether the state is saved with XSAVE, and which of its components the
/// processor has been told programs may use (XCR0). Without XSAVE it is
/// FXSAVE, and programs have x87 and SSE.
static XSAVE: AtomicBool = AtomicBool::new(false);
static COMPONENTS: AtomicU64 = AtomicU64::new(0);
/// Whether the processor has the form of XSAVE that leaves out what a task
/// has not touched: a component still as the processor first had it is
/// noted in the header and not written. Most tasks never touch a wide
/// register, and theirs is then a save of what FXSAVE saved.
static LEAVES_OUT: AtomicBool = AtomicBool::new(false);

const CR4_OSXSAVE: u64 = 1 << 18;

/// XCR0's bits: the components of the state.
const X87: u64 = 1 << 0;
const SSE: u64 = 1 << 1;
const AVX: u64 = 1 << 2;
/// AVX-512's three, which are turned on together or not at all.
const AVX512: u64 = 0b111 << 5;

fn cpuid(leaf: u32, sub: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid_count(leaf, sub)
}

/// # Safety
/// CR4.OSXSAVE set, and `components` ones the processor has.
unsafe fn set_components(components: u64) {
    unsafe {
        core::arch::asm!(
            "xsetbv",
            in("ecx") 0u32,
            in("eax") components as u32,
            in("edx") (components >> 32) as u32,
            options(nostack, nomem, preserves_flags),
        );
    }
}

/// Turn on, on this processor, what programs may use, if the processor
/// saves it with XSAVE: which components, or `None` where it is FXSAVE.
///
/// # Safety
/// CR0.EM clear and CR4.OSFXSR set (`boot.s`).
unsafe fn enable() -> Option<u64> {
    // CPUID 1, ECX bit 26: XSAVE, and the register that says what it saves.
    if cpuid(0, 0).eax < 0xD || cpuid(1, 0).ecx & (1 << 26) == 0 {
        return None;
    }
    let has = {
        let d = cpuid(0xD, 0);
        d.eax as u64 | (d.edx as u64) << 32
    };
    if has & (X87 | SSE) != X87 | SSE {
        return None;
    }
    let mut want = X87 | SSE;
    if has & AVX != 0 {
        want |= AVX;
        // AVX-512 is on top of AVX, and all of it or none.
        if has & AVX512 == AVX512 {
            want |= AVX512;
        }
    }
    unsafe {
        let cr4: u64;
        core::arch::asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        core::arch::asm!("mov cr4, {}", in(reg) cr4 | CR4_OSXSAVE, options(nomem, nostack));
        set_components(want);
        // What XSAVE now writes: it has to fit a task's area. It does, by
        // where each component goes; a processor that says otherwise is
        // given the two every processor has.
        if cpuid(0xD, 0).ebx as usize > AREA_SIZE {
            want = X87 | SSE;
            set_components(want);
        }
    }
    Some(want)
}

/// Reset the unit, turn on what programs may use, and make the clean state.
/// Called once, at boot, on the first processor, after `boot.s` has cleared
/// CR0.EM and set CR4.OSFXSR — without those every instruction below faults.
pub fn init() {
    let mxcsr = MXCSR_DEFAULT;
    unsafe {
        core::arch::asm!(
            "fninit",
            "ldmxcsr [{m}]",
            m = in(reg) &mxcsr as *const u32,
            options(nostack, preserves_flags),
        );
        // The clean state begins as what FXSAVE says of a unit just reset:
        // taken from the processor rather than written out by hand, so that
        // its reserved fields are whatever this processor considers valid.
        core::arch::asm!(
            "fxsave64 [{area}]",
            area = in(reg) &raw mut CLEAN,
            options(nostack, preserves_flags),
        );
        if let Some(components) = enable() {
            COMPONENTS.store(components, Ordering::Relaxed);
            XSAVE.store(true, Ordering::Relaxed);
            // CPUID 0xD, sub-leaf 1, EAX bit 0: XSAVEOPT.
            LEAVES_OUT.store(cpuid(0xD, 1).eax & 1 != 0, Ordering::Relaxed);
            // And for XSAVE: a header that says no component is other than
            // as the processor first has it. XRSTOR then makes every
            // register it restores nought — the SSE and AVX ones, which
            // whoever ran before this kernel may have left anything in —
            // and takes MXCSR from where FXSAVE put it.
            let clean = &mut *(&raw mut CLEAN);
            clean.0[512..576].fill(0);
            clean.0[160..416].fill(0);
        }
        // And this processor's registers made that: the first task is
        // entered without a switch to load its state for it.
        restore(&raw const CLEAN);
    }
    let has = |bit: u64| COMPONENTS.load(Ordering::Relaxed) & bit == bit;
    crate::serial::puts(b"Registers: x87 and SSE");
    if has(AVX) {
        crate::serial::puts(b", AVX");
    }
    if has(AVX512) {
        crate::serial::puts(b", AVX-512");
    }
    crate::serial::puts(if XSAVE.load(Ordering::Relaxed) { b"; saved with XSAVE.\n" } else { b"; saved with FXSAVE.\n" });
}

/// Reset the unit on a processor other than the first, and turn on there
/// what the first has: which components programs may use is a register of
/// each processor's own. What a clean state is was made on the first; a
/// task's own is loaded before it runs here.
///
/// # Safety
/// CR0.EM clear, and CR4 as the first processor has it — OSFXSR, and
/// OSXSAVE if it used XSAVE.
pub unsafe fn init_processor() {
    let mxcsr = MXCSR_DEFAULT;
    unsafe {
        if XSAVE.load(Ordering::Relaxed) {
            set_components(COMPONENTS.load(Ordering::Relaxed));
        }
        core::arch::asm!(
            "fninit",
            "ldmxcsr [{m}]",
            m = in(reg) &mxcsr as *const u32,
            options(nostack, preserves_flags),
        );
        restore(&raw const CLEAN);
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
        if XSAVE.load(Ordering::Relaxed) {
            let components = COMPONENTS.load(Ordering::Relaxed);
            if LEAVES_OUT.load(Ordering::Relaxed) {
                core::arch::asm!(
                    "xsaveopt64 [{}]",
                    in(reg) area,
                    in("eax") components as u32,
                    in("edx") (components >> 32) as u32,
                    options(nostack, preserves_flags),
                );
            } else {
                core::arch::asm!(
                    "xsave64 [{}]",
                    in(reg) area,
                    in("eax") components as u32,
                    in("edx") (components >> 32) as u32,
                    options(nostack, preserves_flags),
                );
            }
        } else {
            core::arch::asm!(
                "fxsave64 [{}]",
                in(reg) area,
                options(nostack, preserves_flags),
            );
        }
    }
}

/// Load the processor's floating-point state from `area`.
///
/// # Safety
/// `area` must hold a state `save` or `clean` produced; the instruction
/// faults on a reserved MXCSR bit or a header that names what is not there,
/// and an arbitrary buffer will have some of each.
pub unsafe fn restore(area: *const FpuState) {
    unsafe {
        if XSAVE.load(Ordering::Relaxed) {
            let components = COMPONENTS.load(Ordering::Relaxed);
            core::arch::asm!(
                "xrstor64 [{}]",
                in(reg) area,
                in("eax") components as u32,
                in("edx") (components >> 32) as u32,
                options(nostack, preserves_flags),
            );
        } else {
            core::arch::asm!(
                "fxrstor64 [{}]",
                in(reg) area,
                options(nostack, preserves_flags),
            );
        }
    }
}
