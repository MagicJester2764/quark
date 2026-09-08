//! Thread-local storage.
//!
//! Threads share an address space, so an address alone cannot tell one
//! thread's copy of a variable from another's. x86-64 resolves that with a
//! segment base: each thread points FS somewhere different, and a thread-local
//! access is an ordinary load at an offset from it.
//!
//! The layout is the x86-64 "variant II" the ABI specifies, which is worth
//! stating because it is the opposite of the obvious one:
//!
//! ```text
//!     lower addresses                                   higher addresses
//!     +--------------------------------+----------------+
//!     |  thread-local variables        |  TCB           |
//!     |  (a copy of the PT_TLS image)  |  self-pointer  |
//!     +--------------------------------+----------------+
//!     ^                                ^
//!     block                            TP == FS base
//! ```
//!
//! The thread pointer sits at the *end* of the variables, and they are reached
//! at negative offsets from it. The first word at TP must hold TP itself: code
//! that needs the thread pointer as a value reads `fs:[0]`, since there is no
//! instruction to read a segment base directly from user mode.
//!
//! The template comes from the linker script, which delimits `.tdata` (the
//! initialised image) and `.tbss` (the zeroed tail). A static binary has no
//! other way to find its own `PT_TLS` segment.

use crate::syscall;

unsafe extern "C" {
    // Linker-defined: the *address* of each symbol is the value.
    static __tdata_start: u8;
    static __tdata_size: u8;
    static __tls_size: u8;
    static __tls_align: u8;
}

#[inline]
fn linker_value(sym: &u8) -> usize {
    sym as *const u8 as usize
}

/// Size of the whole template, initialised part plus zeroed tail.
pub fn template_size() -> usize {
    unsafe { linker_value(&__tls_size) }
}

/// Alignment the linker gave the TLS segment.
///
/// Not a constant: it varies with what a program actually puts in thread-local
/// storage, and the placement below depends on the real value. Never zero, so
/// a program without a TLS segment still gets a usable alignment.
pub fn template_align() -> usize {
    let a = unsafe { linker_value(&__tls_align) };
    if a == 0 { 8 } else { a }
}

/// Alignment callers should give the region they pass to [`init_in`].
pub const TLS_ALIGN: usize = 64;

/// Bytes of the TCB following the thread pointer. Only the self-pointer is
/// used, but the ABI reserves the area.
pub const TCB_SIZE: usize = 16;

/// How much memory [`init_in`] needs for the current program.
pub fn required_bytes() -> usize {
    // The block is sized to the *linker's* alignment, because that is what its
    // offsets were computed against. The extra TLS_ALIGN covers rounding the
    // caller's region up to a suitable base.
    align_up(template_size(), template_align()) + TCB_SIZE + TLS_ALIGN
}

const fn align_up(v: usize, a: usize) -> usize {
    (v + a - 1) & !(a - 1)
}

/// Lay out a thread's storage in `region` and point FS at it.
///
/// `region` must be at least [`required_bytes`] long and aligned to
/// [`TLS_ALIGN`]; it must also outlive the thread, so a stack buffer will not
/// do. Takes no allocator, so it is usable from a `no_std` program and from
/// std's runtime alike.
///
/// # Safety
/// `region` must not be in use for anything else, and must not be handed to a
/// second thread: each needs storage of its own, which is the entire point.
pub unsafe fn init_in(region: *mut u8, len: usize) -> Result<(), ()> {
    let tls_size = template_size();
    let tdata_size = unsafe { linker_value(&__tdata_size) };
    let tdata_start = unsafe { linker_value(&__tdata_start) } as *const u8;

    let need = required_bytes();
    if len < need || region.is_null() {
        return Err(());
    }

    // Round the base up to whichever is stricter: what the template asks for,
    // or the region alignment callers are told to provide.
    let base = align_up(region as usize, template_align().max(TLS_ALIGN));
    if base + need > region as usize + len {
        return Err(());
    }

    // The block starts at TP - ALIGN_UP(size, align), not TP - size: the
    // linker computed every offset against the padded figure, so anything else
    // puts each variable off by the padding.
    let block = align_up(tls_size, template_align());
    let tp = base + block;

    unsafe {
        // Variables occupy [tp - block, tp): the initialised image first, then
        // the zeroed tail, then the alignment padding.
        let vars = (tp - block) as *mut u8;
        core::ptr::write_bytes(vars, 0, block);
        if tdata_size > 0 {
            core::ptr::copy_nonoverlapping(tdata_start, vars, tdata_size);
        }

        // The self-pointer. `fs:[0]` is how the thread pointer is read back.
        *(tp as *mut usize) = tp;
    }

    syscall::sys_set_fs_base(tp)
}

/// The current thread pointer, read through the self-pointer at `fs:[0]`.
///
/// Returns 0 if TLS has not been set up for this thread, since the FS base
/// starts at zero and reading through it would fault.
pub fn thread_pointer() -> usize {
    let tp: usize;
    unsafe {
        core::arch::asm!("mov {}, fs:[0]", out(reg) tp, options(nostack, readonly));
    }
    tp
}
