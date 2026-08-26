//! CPU feature setup: supervisor-mode access protections.
//!
//! SMEP stops ring 0 from *executing* pages marked USER, which turns a kernel
//! control-flow bug into a fault instead of a jump into attacker-supplied code.
//! SMAP extends that to *reads and writes*: the kernel may only touch user
//! pages while RFLAGS.AC is set, which the [`UserAccess`] guard does around the
//! handful of places that legitimately copy to or from user buffers.
//!
//! Both are enabled only when CPUID reports them, since `stac`/`clac` raise #UD
//! on a CPU without SMAP.

use core::sync::atomic::{AtomicBool, Ordering};

const CR4_SMEP: u64 = 1 << 20;
const CR4_SMAP: u64 = 1 << 21;

/// Whether SMAP was enabled, and therefore whether `stac`/`clac` are legal.
static SMAP_ENABLED: AtomicBool = AtomicBool::new(false);

/// CPUID leaf 7 subleaf 0: EBX bit 7 = SMEP, bit 20 = SMAP.
fn cpuid_7_0_ebx() -> u32 {
    let ebx: u32;
    unsafe {
        core::arch::asm!(
            // Preserve RBX: LLVM reserves it and refuses it as an operand.
            "mov {tmp:r}, rbx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "mov rbx, {tmp:r}",
            tmp = out(reg) _,
            ebx = out(reg) ebx,
            inout("eax") 7 => _,
            inout("ecx") 0 => _,
            out("edx") _,
            options(nostack),
        );
    }
    ebx
}

fn read_cr4() -> u64 {
    let val: u64;
    unsafe { core::arch::asm!("mov {}, cr4", out(reg) val, options(nomem, nostack)) };
    val
}

/// # Safety
/// Caller must not clear bits the kernel depends on (PAE, OSFXSR, ...).
unsafe fn write_cr4(val: u64) { unsafe {
    core::arch::asm!("mov cr4, {}", in(reg) val, options(nomem, nostack));
}}

/// Enable SMEP and SMAP if the CPU supports them.
///
/// Must run after paging is up and before entering user mode. Every user
/// mapping lives at or above `paging::USER_MIN_ADDR` (PML4[1]), so no USER bit
/// is set anywhere in the kernel's own identity map and SMAP cannot fire on
/// ordinary kernel memory access.
///
/// # Safety
/// Must be called once, during boot, on the bootstrap CPU.
pub unsafe fn init_protections() { unsafe {
    let features = cpuid_7_0_ebx();
    let smep = features & (1 << 7) != 0;
    let smap = features & (1 << 20) != 0;

    let mut cr4 = read_cr4();
    if smep {
        cr4 |= CR4_SMEP;
    }
    if smap {
        cr4 |= CR4_SMAP;
    }
    if smep || smap {
        write_cr4(cr4);
    }

    // Set only after CR4 is live: the guard checks this before issuing stac.
    SMAP_ENABLED.store(smap, Ordering::SeqCst);

    let smep_s: &[u8] = if smep { b"on" } else { b"unavailable" };
    let smap_s: &[u8] = if smap { b"on" } else { b"unavailable" };
    for out in [
        crate::console::puts as fn(&[u8]),
        crate::serial::puts as fn(&[u8]),
    ] {
        out(b"CPU protections: SMEP ");
        out(smep_s);
        out(b", SMAP ");
        out(smap_s);
        out(b".\n");
    }
}}

/// Scoped permission for the kernel to touch user memory.
///
/// Sets RFLAGS.AC for the lifetime of the guard and clears it on drop. Keep the
/// scope as small as the copy itself — never hold one across a block or yield,
/// because AC travels with the task's saved RFLAGS and would leave the window
/// open in whatever runs next.
pub struct UserAccess;

impl UserAccess {
    #[inline(always)]
    pub fn begin() -> Self {
        if SMAP_ENABLED.load(Ordering::Relaxed) {
            unsafe { core::arch::asm!("stac", options(nomem, nostack)) };
        }
        UserAccess
    }
}

impl Drop for UserAccess {
    #[inline(always)]
    fn drop(&mut self) {
        if SMAP_ENABLED.load(Ordering::Relaxed) {
            unsafe { core::arch::asm!("clac", options(nomem, nostack)) };
        }
    }
}
