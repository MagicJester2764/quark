/// Minimal multiboot2 info parser — extracts framebuffer tag (type 8).

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub struct FramebufferInfo {
    pub addr: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    /// 0 = indexed color, 1 = direct RGB, 2 = EGA text
    pub fb_type: u8,
    // Color channel positions (valid when fb_type == 1)
    pub red_pos: u8,
    pub red_mask: u8,
    pub green_pos: u8,
    pub green_mask: u8,
    pub blue_pos: u8,
    pub blue_mask: u8,
}

const TAG_TYPE_END: u32 = 0;
const TAG_TYPE_FRAMEBUFFER: u32 = 8;

/// Parse the multiboot2 info structure and return framebuffer info if present.
///
/// # Safety
/// `info_addr` must point to a valid multiboot2 boot information structure.
pub unsafe fn parse_framebuffer(info_addr: usize) -> Option<FramebufferInfo> {
    let ptr = info_addr as *const u8;

    // First 8 bytes: total_size (u32) + reserved (u32)
    // Tags start at offset 8
    let mut offset: usize = 8;
    let total_size = (ptr as *const u32).read_unaligned() as usize;

    loop {
        if offset >= total_size {
            return None;
        }

        // Align offset to 8 bytes (tags are 8-byte aligned)
        offset = (offset + 7) & !7;

        let tag_ptr = ptr.add(offset);
        let tag_type = (tag_ptr as *const u32).read_unaligned();
        let tag_size = (tag_ptr.add(4) as *const u32).read_unaligned();

        if tag_type == TAG_TYPE_END {
            return None;
        }

        if tag_type == TAG_TYPE_FRAMEBUFFER {
            // Framebuffer tag layout (after type + size):
            //   offset  0: addr (u64)
            //   offset  8: pitch (u32)
            //   offset 12: width (u32)
            //   offset 16: height (u32)
            //   offset 20: bpp (u8)
            //   offset 21: fb_type (u8)
            //   offset 22: reserved (u16)
            // For fb_type == 1 (direct RGB), color info follows at offset 24:
            //   offset 24: red_field_position (u8)
            //   offset 25: red_mask_size (u8)
            //   offset 26: green_field_position (u8)
            //   offset 27: green_mask_size (u8)
            //   offset 28: blue_field_position (u8)
            //   offset 29: blue_mask_size (u8)
            let data = tag_ptr.add(8); // skip tag type + size

            let addr = (data as *const u64).read_unaligned();
            let pitch = (data.add(8) as *const u32).read_unaligned();
            let width = (data.add(12) as *const u32).read_unaligned();
            let height = (data.add(16) as *const u32).read_unaligned();
            let bpp = *data.add(20);
            let fb_type = *data.add(21);

            let (red_pos, red_mask, green_pos, green_mask, blue_pos, blue_mask) = if fb_type == 1 {
                (
                    *data.add(24),
                    *data.add(25),
                    *data.add(26),
                    *data.add(27),
                    *data.add(28),
                    *data.add(29),
                )
            } else {
                (0, 0, 0, 0, 0, 0)
            };

            return Some(FramebufferInfo {
                addr,
                pitch,
                width,
                height,
                bpp,
                fb_type,
                red_pos,
                red_mask,
                green_pos,
                green_mask,
                blue_pos,
                blue_mask,
            });
        }

        offset += tag_size as usize;
    }
}
