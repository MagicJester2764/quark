mod framebuffer;
mod vga;

use core::sync::atomic::{AtomicU8, Ordering};

use crate::modules;
use crate::multiboot2::FramebufferInfo;

/// 0 = not initialized, 1 = VGA text (driver), 2 = pixel framebuffer
static MODE: AtomicU8 = AtomicU8::new(0);

/// Initialize the console from multiboot2 framebuffer info.
/// The module registry must be initialized before calling this.
pub fn init(fb: Option<FramebufferInfo>) {
    // Try to load VGA driver from boot modules
    if let Some(m) = modules::find(b"VGA.DRV") {
        unsafe { vga::init_from_driver(m.start) };
    } else if let Some(m) = modules::find(b"vga.drv") {
        unsafe { vga::init_from_driver(m.start) };
    }

    match fb {
        Some(info) if info.fb_type == 1 => {
            // Direct RGB pixel framebuffer (UEFI GOP)
            unsafe {
                framebuffer::init(
                    info.addr,
                    info.pitch,
                    info.width,
                    info.height,
                    info.bpp,
                    info.red_pos,
                    info.green_pos,
                    info.blue_pos,
                );
            }
            MODE.store(2, Ordering::Release);
        }
        _ => {
            // VGA text mode — use driver if available
            if vga::is_loaded() {
                MODE.store(1, Ordering::Release);
            }
            // If no driver and no framebuffer, MODE stays 0 (no output)
        }
    }
}

pub fn clear() {
    match MODE.load(Ordering::Acquire) {
        1 => vga::clear(),
        2 => framebuffer::clear(),
        _ => {}
    }
}

pub fn cursor_pos_and_disable() -> (usize, usize) {
    match MODE.load(Ordering::Acquire) {
        2 => framebuffer::cursor_pos_and_disable(),
        _ => (0, 0),
    }
}

pub fn puts(s: &[u8]) {
    match MODE.load(Ordering::Acquire) {
        1 => vga::puts(s),
        2 => framebuffer::puts(s),
        _ => {}
    }
}
