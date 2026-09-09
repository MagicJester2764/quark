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

/// A scheduling band rather than a capability. Numbered above the `CAP_TYPE_*`
/// values so a spawner can tell it apart from something to mint.
pub const PRIORITY_REQ: u64 = 0x100;

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

    /// Which scheduling band to run in — see `syscall::PRIO_*`.
    ///
    /// Not a capability: nothing is minted and no slot is used. It is here
    /// because it is the same kind of statement as the rest of the manifest —
    /// what the program needs in order to do its job — and because a spawner
    /// applies it under the same rule, unable to grant a better band than it
    /// is in itself.
    pub const fn priority(band: u8) -> Self {
        CapReq { cap_type: PRIORITY_REQ, param0: band as u64, param1: 0 }
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

/// Every manifest block in a program image, in the order they appear.
///
/// Blocks are found by scanning for the magic rather than by section name, so
/// this does not depend on section headers surviving, on a particular linker
/// script, or on the manifest landing in a predictable segment. An image with
/// no manifest yields nothing, which is not an error: a program that needs no
/// capabilities declares nothing.
///
/// An image is linked from several objects and any of them may declare what it
/// needs. That is not a corner case: a C library asks for the page file data
/// moves through, because the library is what knows a page is needed and the
/// program only knows it called `fopen`. So an image's manifest is the sum of
/// the blocks in it rather than whichever one the linker happened to put
/// first — a program that declared nothing still gets what its library asked
/// for, and one that declared something gets both.
pub struct Blocks<'a> {
    image: &'a [u8],
    off: usize,
}

impl<'a> Iterator for Blocks<'a> {
    type Item = &'a [CapReq];

    fn next(&mut self) -> Option<&'a [CapReq]> {
        const HDR: usize = 24; // magic + version + count
        let magic = MANIFEST_MAGIC.to_le_bytes();

        while self.off + HDR <= self.image.len() {
            let off = self.off;
            if self.image[off..off + 8] != magic {
                // The manifest is a static, so it is at least 8-byte aligned.
                self.off += 8;
                continue;
            }
            let version =
                u64::from_le_bytes(self.image[off + 8..off + 16].try_into().ok()?);
            let count =
                u64::from_le_bytes(self.image[off + 16..off + 24].try_into().ok()?) as usize;
            if version != MANIFEST_VERSION {
                self.off += 8;
                continue;
            }
            // A count that does not fit is a corrupt or mis-detected header,
            // not a reason to read past the end of the image.
            let bytes = match count.checked_mul(core::mem::size_of::<CapReq>()) {
                Some(b) => b,
                None => {
                    self.off += 8;
                    continue;
                }
            };
            if off + HDR + bytes > self.image.len() {
                self.off += 8;
                continue;
            }
            self.off = off + HDR + bytes;
            let ptr = unsafe { self.image.as_ptr().add(off + HDR) as *const CapReq };
            return Some(unsafe { core::slice::from_raw_parts(ptr, count) });
        }
        None
    }
}

pub fn blocks(image: &[u8]) -> Blocks<'_> {
    Blocks { image, off: 0 }
}

/// Mint everything an image asks for, across every block in it, and grant it
/// to `child`.
///
/// `scratch_slot` is a slot in *our* CSpace used to hold each capability while
/// it is handed over; it is emptied afterwards, and it also bounds how far into
/// the child's CSpace a manifest can reach. Requests we cannot satisfy are
/// skipped rather than failing the whole spawn — a spawner is allowed to hold
/// less than a program asks for, and the program finds out when it tries to
/// act.
///
/// Slots are handed out in order as capabilities are actually minted, so a
/// request that is only a scheduling band does not leave a hole and two blocks
/// do not land on top of each other.
///
/// Returns how many were granted.
pub fn grant_image(child: usize, image: &[u8], scratch_slot: usize) -> usize {
    let mut slot = 0usize;
    let mut granted = 0usize;
    for reqs in blocks(image) {
        for req in reqs {
            if req.cap_type == 0 {
                continue;
            }
            if req.cap_type == PRIORITY_REQ {
                // Nothing to mint: this asks to be scheduled differently, not
                // to be allowed to do something.
                if syscall::sys_task_priority(child, req.param0 as u8).is_ok() {
                    granted += 1;
                }
                continue;
            }
            // Slots at and above the scratch one are the spawner's own
            // working space and the endpoint sets; a manifest cannot reach
            // into them.
            if slot >= scratch_slot {
                break;
            }
            if syscall::sys_cap_mint(scratch_slot, req.cap_type, req.param0, req.param1).is_err()
            {
                slot += 1;
                continue;
            }
            if syscall::sys_cap_grant(child, scratch_slot, slot).is_ok() {
                granted += 1;
            }
            let _ = syscall::sys_cap_delete(scratch_slot);
            slot += 1;
        }
    }
    granted
}
