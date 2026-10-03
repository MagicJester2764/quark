//! Memory for a screen: one contiguous run of RAM for each display device,
//! no task's to free and never given out again.
//!
//! A display controller that reads its picture out of memory — a virtio GPU —
//! has no framebuffer of its own to hand out, and the firmware's, if it gave
//! one, is not the device's. Its driver asks here (`SYS_DISPLAY_MEMORY`) and
//! is given a run as a `PhysRange`, which it hands to the framebuffer device,
//! which lends it to whoever has the display: exactly as the bootloader's
//! framebuffer is handled. The frames are nobody's. Were they the driver's,
//! its death would give them back to the allocator while the console was
//! still drawing into them. They are kept for the device, for as long as the
//! machine is up, and a driver started for it again is given the same run.

use crate::sync::IrqSpinLock;

const PAGE: usize = 4096;
/// Devices that may have a screen in memory.
const MAX_DISPLAYS: usize = 4;
/// The most one may ask for: 64 MiB, more than a 3840 by 2160 screen of four
/// bytes a pixel needs.
pub const MAX_PAGES: usize = 16384;

#[derive(Clone, Copy)]
struct Display {
    bdf: u16,
    base: usize,
    pages: usize,
}

static DISPLAYS: IrqSpinLock<[Option<Display>; MAX_DISPLAYS]> = IrqSpinLock::new([None; MAX_DISPLAYS]);

/// Device `bdf`'s screen, at least `pages` long: where it begins. Made the
/// first time — one contiguous run, from the top of memory — and the same
/// every time after, which a driver started again is given back. `None` if
/// there is no run that long, or no room to remember another device's, or
/// the device has one already and it is shorter.
pub fn memory(bdf: u16, pages: usize) -> Option<usize> {
    if pages == 0 || pages > MAX_PAGES {
        return None;
    }
    let mut displays = DISPLAYS.lock();
    if let Some(d) = displays.iter().flatten().find(|d| d.bdf == bdf) {
        return (d.pages >= pages).then_some(d.base);
    }
    let free = displays.iter().position(|d| d.is_none())?;
    let base = crate::pmm::alloc_contiguous(pages, false)?.address();
    // What was in them before is nobody's business: the screen starts black.
    unsafe { core::ptr::write_bytes(base as *mut u8, 0, pages * PAGE) };
    displays[free] = Some(Display { bdf, base, pages });
    Some(base)
}
