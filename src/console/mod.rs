mod framebuffer;
mod vga;

use crate::modules;
use crate::multiboot2::FramebufferInfo;

/// 0 = not initialized, 1 = VGA text (driver), 2 = pixel framebuffer
static mut MODE: u8 = 0;

/// Initialize the console from multiboot2 framebuffer info.
/// The module registry must be initialized before calling this.
pub fn init(fb: Option<FramebufferInfo>) {
    // Try to load VGA driver from boot modules
    if let Some(m) = modules::find(b"VGA.DRV") {
        unsafe { vga::init_from_driver(m.start) };
    } else if let Some(m) = modules::find(b"vga.drv") {
        unsafe { vga::init_from_driver(m.start) };
    }

    unsafe {
        match fb {
            Some(info) if info.fb_type == 1 => {
                // Direct RGB pixel framebuffer (UEFI GOP)
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
                MODE = 2;
            }
            _ => {
                // VGA text mode — use driver if available
                if vga::is_loaded() {
                    MODE = 1;
                }
                // If no driver and no framebuffer, MODE stays 0 (no output)
            }
        }
    }
}

pub fn clear() {
    unsafe {
        match MODE {
            1 => vga::clear(),
            2 => framebuffer::clear(),
            _ => {}
        }
    }
}

pub fn cursor_pos_and_disable() -> (usize, usize) {
    unsafe {
        match MODE {
            2 => framebuffer::cursor_pos_and_disable(),
            _ => (0, 0),
        }
    }
}

pub fn puts(s: &[u8]) {
    unsafe {
        match MODE {
            1 => vga::puts(s),
            2 => framebuffer::puts(s),
            _ => {}
        }
    }
}
