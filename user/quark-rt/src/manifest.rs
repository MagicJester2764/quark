//! What a program needs, declared by the program.
//!
//! Capabilities used to be handed out by name: `init::grant_caps_by_name` was a
//! chain of `base == b"KEYBOARD"` comparisons, and the shell and login carried
//! smaller versions of the same idea. That works only for programs the tree
//! already knows about — a package installed later has no branch, and cannot be
//! given one without editing and rebuilding init.
//!
//! A program now states its own requirements, and they travel inside its image
//! rather than beside it, so they cannot be lost or go stale relative to the
//! binary they describe.
//!
//! ```ignore
//! quark_rt::manifest!([
//!     CapReq::ioport(0x60, 0x64),
//!     CapReq::irq(1),
//! ]);
//! ```
//!
//! Declaring a requirement is not the same as receiving it. A spawner mints
//! each capability from one it already holds, so it can never hand out more
//! authority than it has itself — asking for more is refused, not granted.

use crate::syscall;

/// Marks a manifest inside a program image. Distinctive enough that finding it
/// by scanning is not going to collide with ordinary data.
pub const MANIFEST_MAGIC: u64 = 0x4649_4E41_4D4B_5251; // "QRKMANIF", little endian

/// Current manifest layout version.
pub const MANIFEST_VERSION: u64 = 1;

/// One capability a program is asking for.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CapReq {
    pub cap_type: u64,
    pub param0: u64,
    pub param1: u64,
}

impl CapReq {
    /// An empty slot; ignored when granting.
    pub const NONE: CapReq = CapReq { cap_type: 0, param0: 0, param1: 0 };

    /// Port-mapped I/O over an inclusive range.
    pub const fn ioport(first: u16, last: u16) -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_IOPORT, param0: first as u64, param1: last as u64 }
    }

    /// A hardware interrupt line. 0xFF is the wildcard.
    pub const fn irq(line: u8) -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_IRQ, param0: line as u64, param1: 0 }
    }

    /// Physical memory the allocator never owned — device MMIO, a framebuffer.
    /// Frames the program allocates for itself need no capability at all.
    pub const fn phys_range(start: u64, end: u64) -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_PHYS_RANGE, param0: start, param1: end }
    }

    /// Permission to allocate physical frames, up to `max_pages` (0 = no cap).
    pub const fn phys_alloc(max_pages: u64) -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_PHYS_ALLOC, param0: max_pages, param1: 0 }
    }

    /// Create, start, signal and configure tasks. `0` means any target.
    pub const fn task_mgmt(target: u64) -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_TASK_MGMT, param0: target, param1: 0 }
    }

    /// Change a task's user or group.
    pub const fn set_uid() -> Self {
        CapReq { cap_type: syscall::CAP_TYPE_SET_UID, param0: 0, param1: 0 }
    }
}

/// A manifest as it sits in the image: a header the scanner can recognise,
/// followed by the requests themselves.
#[repr(C)]
pub struct Manifest<const N: usize> {
    pub magic: u64,
    pub version: u64,
    pub count: u64,
    pub reqs: [CapReq; N],
}

impl<const N: usize> Manifest<N> {
    pub const fn new(reqs: [CapReq; N]) -> Self {
        Manifest { magic: MANIFEST_MAGIC, version: MANIFEST_VERSION, count: N as u64, reqs }
    }
}

/// Declare what this program needs. See the module docs.
#[macro_export]
macro_rules! manifest {
    ($reqs:expr) => {
        #[used]
        #[unsafe(link_section = ".quark.manifest")]
        static __QUARK_MANIFEST: $crate::manifest::Manifest<{ $reqs.len() }> =
            $crate::manifest::Manifest::new($reqs);
    };
}

/// Find a manifest in a program image.
///
/// Located by scanning for the magic rather than by section name, so it does
/// not depend on section headers surviving, on a particular linker script, or
/// on the manifest landing in a predictable segment.
///
/// Returns the requests, or `None` if the image has no manifest — which is not
/// an error: a program that needs no capabilities declares nothing.
pub fn find(image: &[u8]) -> Option<&[CapReq]> {
    const HDR: usize = 24; // magic + version + count
    let magic = MANIFEST_MAGIC.to_le_bytes();

    let mut off = 0;
    while off + HDR <= image.len() {
        if image[off..off + 8] == magic {
            let version = u64::from_le_bytes(image[off + 8..off + 16].try_into().ok()?);
            let count = u64::from_le_bytes(image[off + 16..off + 24].try_into().ok()?) as usize;
            if version != MANIFEST_VERSION {
                off += 8;
                continue;
            }
            // A count that does not fit is a corrupt or mis-detected header,
            // not a reason to read past the end of the image.
            let bytes = count.checked_mul(core::mem::size_of::<CapReq>())?;
            if off + HDR + bytes > image.len() {
                off += 8;
                continue;
            }
            let ptr = unsafe { image.as_ptr().add(off + HDR) as *const CapReq };
            return Some(unsafe { core::slice::from_raw_parts(ptr, count) });
        }
        // The manifest is a static, so it is at least 8-byte aligned.
        off += 8;
    }
    None
}

/// Mint each requested capability and grant it to `child`.
///
/// `scratch_slot` is a slot in *our* CSpace used to hold each capability while
/// it is handed over; it is emptied afterwards. Requests we cannot satisfy are
/// skipped rather than failing the whole spawn — a spawner is allowed to hold
/// less than a program asks for, and the program finds out when it tries to act.
///
/// Returns how many were granted.
pub fn grant(child: usize, reqs: &[CapReq], scratch_slot: usize) -> usize {
    let mut granted = 0;
    for (i, req) in reqs.iter().enumerate() {
        if req.cap_type == 0 {
            continue;
        }
        if syscall::sys_cap_mint(scratch_slot, req.cap_type, req.param0, req.param1).is_err() {
            continue;
        }
        if syscall::sys_cap_grant(child, scratch_slot, i).is_ok() {
            granted += 1;
        }
        let _ = syscall::sys_cap_delete(scratch_slot);
    }
    granted
}
