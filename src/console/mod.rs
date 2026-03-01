mod framebuffer;
mod vga;

use crate::multiboot2::FramebufferInfo;

/// 0 = not initialized (default to VGA text), 1 = VGA text, 2 = pixel framebuffer
static mut MODE: u8 = 0;

/// Initialize the console from multiboot2 framebuffer info.
pub fn init(fb: Option<FramebufferInfo>) {
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
            Some(info) if info.fb_type == 2 => {
                // EGA text mode — use VGA backend with provided address
                vga::init(info.addr, info.width, info.height);
                MODE = 1;
            }
            _ => {
                // No framebuffer tag or unknown type — assume legacy VGA text
                MODE = 1;
            }
        }
    }
}

pub fn clear() {
    unsafe {
        match MODE {
            2 => framebuffer::clear(),
            _ => vga::clear(),
        }
    }
}

pub fn puts(s: &[u8]) {
    unsafe {
        match MODE {
            2 => framebuffer::puts(s),
            _ => vga::puts(s),
        }
    }
}
