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
/// RDFSBASE, WRFSBASE, RDGSBASE and WRGSBASE in ring 3.
const CR4_FSGSBASE: u64 = 1 << 16;

/// Whether a program may read and write its own FS and GS bases: set once, at
/// boot, before the other processors take the first one's CR4.
static FSGSBASE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether FSGSBASE is on: a program can change its FS and GS bases without
/// a call, and what they are is read back from the processor.
pub fn fsgsbase() -> bool {
    FSGSBASE_ENABLED.load(Ordering::Relaxed)
}
// CR4.PKE (bit 22) must stay clear. Memory objects keep their slot in bits
// 52–62 of page-table entries, and with protection keys on the CPU reads bits
// 59–62 of a present entry as the page's key.

/// Whether SMAP was enabled, and therefore whether `stac`/`clac` are legal.
///
/// The interrupt and exception stubs read it too, as the byte it is: they
/// clear AC on the way in, where there is a `clac` to do it with.
pub static SMAP_ENABLED: AtomicBool = AtomicBool::new(false);

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

/// CPUID leaf 1: ECX bit 30 = RDRAND.
fn cpuid_1_ecx() -> u32 {
    let ecx: u32;
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "mov rbx, {tmp:r}",
            tmp = out(reg) _,
            inout("eax") 1 => _,
            inout("ecx") 0 => ecx,
            out("edx") _,
            options(nostack),
        );
    }
    ecx
}

/// Whether the CPU has RDSEED and RDRAND, in that order.
pub fn random_instructions() -> (bool, bool) {
    (cpuid_7_0_ebx() & (1 << 18) != 0, cpuid_1_ecx() & (1 << 30) != 0)
}

/// Read a model-specific register.
pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    unsafe {
        core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi,
                         options(nostack, nomem, preserves_flags));
    }
    (hi as u64) << 32 | lo as u64
}

/// Write a model-specific register.
///
/// # Safety
/// Whatever the register means.
pub unsafe fn wrmsr(msr: u32, value: u64) {
    unsafe {
        core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
                         options(nostack, nomem, preserves_flags));
    }
}

/// CPUID leaf 1: EDX bit 9 = the processor has a local APIC.
pub fn has_apic() -> bool {
    let edx: u32;
    unsafe {
        core::arch::asm!(
            "mov {tmp:r}, rbx",
            "cpuid",
            "mov rbx, {tmp:r}",
            tmp = out(reg) _,
            inout("eax") 1 => _,
            inout("ecx") 0 => _,
            out("edx") edx,
            options(nostack),
        );
    }
    edx & (1 << 9) != 0
}

/// CR0 and CR4 as this processor has them: what it has turned on.
pub fn control_registers() -> (u64, u64) {
    let cr0: u64;
    unsafe { core::arch::asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack)) };
    (cr0, read_cr4())
}

/// Turn on, on this processor, what another has: CR0 and CR4 as
/// [`control_registers`] read them there. For a processor being started,
/// which begins with only what long mode needs.
///
/// # Safety
/// The values must be another processor's of the same machine, and this
/// one must already be in long mode with paging on.
pub unsafe fn set_control_registers((cr0, cr4): (u64, u64)) { unsafe {
    write_cr4(cr4);
    core::arch::asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));
}}

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
    let fsgsbase = features & 1 != 0;

    let mut cr4 = read_cr4();
    if smep {
        cr4 |= CR4_SMEP;
    }
    if smap {
        cr4 |= CR4_SMAP;
    }
    // A program's own FS and GS bases, read and written in the program. Safe
    // only because nothing in the kernel that can run on the program's GS
    // uses it: see `idt.rs` (NMI and machine checks) and `syscall.rs` (what
    // a `syscall` clears, and the return address `sysret` is given).
    if fsgsbase {
        cr4 |= CR4_FSGSBASE;
    }
    if smep || smap || fsgsbase {
        write_cr4(cr4);
    }

    // Set only after CR4 is live: the guard checks this before issuing stac.
    SMAP_ENABLED.store(smap, Ordering::SeqCst);
    FSGSBASE_ENABLED.store(fsgsbase, Ordering::SeqCst);

    let smep_s: &[u8] = if smep { b"on" } else { b"unavailable" };
    let smap_s: &[u8] = if smap { b"on" } else { b"unavailable" };
    let fsgs_s: &[u8] = if fsgsbase { b"on" } else { b"unavailable" };
    for out in [
        crate::console::puts as fn(&[u8]),
        crate::serial::puts as fn(&[u8]),
    ] {
        out(b"CPU protections: SMEP ");
        out(smep_s);
        out(b", SMAP ");
        out(smap_s);
        out(b"; FSGSBASE ");
        out(fsgs_s);
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

/// IA32_FS_BASE. The base FS-relative addressing resolves against, which is
/// where a thread's thread-locals live.
const MSR_FS_BASE: u32 = 0xC000_0100;

/// IA32_KERNEL_GS_BASE: while the kernel runs, the program's GS base, which
/// `swapgs` put there on the way in and takes back on the way out.
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// The running task's FS base, as the processor has it.
pub fn fs_base() -> u64 {
    if fsgsbase() {
        let v: u64;
        unsafe { core::arch::asm!("rdfsbase {}", out(reg) v, options(nostack, nomem, preserves_flags)) };
        v
    } else {
        rdmsr(MSR_FS_BASE)
    }
}

/// The running task's GS base in ring 3: in the kernel it waits in
/// IA32_KERNEL_GS_BASE, where `swapgs` left it.
pub fn user_gs_base() -> u64 {
    rdmsr(MSR_KERNEL_GS_BASE)
}

/// Give the task about to run its GS base in ring 3, where `swapgs` takes it
/// from on the way out.
pub fn set_user_gs_base(base: u64) {
    unsafe { wrmsr(MSR_KERNEL_GS_BASE, base) };
}

/// Set the FS segment base for the task about to run.
///
/// Called on every switch. Writing an MSR is not free, but a thread-local read
/// has to be a plain FS-relative load with no test in it, so the cost belongs
/// here rather than in every access.
pub fn set_fs_base(base: u64) {
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") MSR_FS_BASE,
            in("eax") base as u32,
            in("edx") (base >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }
}
