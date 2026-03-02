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
const TAG_TYPE_MODULE: u32 = 3;
const TAG_TYPE_MMAP: u32 = 6;
const TAG_TYPE_FRAMEBUFFER: u32 = 8;

/// Maximum number of boot modules we track.
pub const MAX_MODULES: usize = 32;

/// Maximum number of memory regions we track.
pub const MAX_MEMORY_REGIONS: usize = 64;

#[allow(dead_code)]
pub const MMAP_TYPE_AVAILABLE: u32 = 1;
#[allow(dead_code)]
pub const MMAP_TYPE_RESERVED: u32 = 2;
#[allow(dead_code)]
pub const MMAP_TYPE_ACPI_RECLAIMABLE: u32 = 3;
#[allow(dead_code)]
pub const MMAP_TYPE_ACPI_NVS: u32 = 4;
#[allow(dead_code)]
pub const MMAP_TYPE_BAD_MEMORY: u32 = 5;

#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub base: u64,
    pub length: u64,
    pub region_type: u32,
}

impl MemoryRegion {
    pub const fn empty() -> Self {
        MemoryRegion {
            base: 0,
            length: 0,
            region_type: 0,
        }
    }
}

/// Information about a loaded boot module.
#[derive(Debug, Clone, Copy)]
pub struct ModuleInfo {
    pub start: usize,
    pub end: usize,
    pub name: [u8; 64],
}

impl ModuleInfo {
    pub const fn empty() -> Self {
        ModuleInfo {
            start: 0,
            end: 0,
            name: [0u8; 64],
        }
    }
}

/// Parse module tags (type 3) from the multiboot2 info structure.
///
/// # Safety
/// `info_addr` must point to a valid multiboot2 boot information structure.
pub unsafe fn parse_modules(info_addr: usize) -> (usize, [ModuleInfo; MAX_MODULES]) {
    let ptr = info_addr as *const u8;
    let total_size = (ptr as *const u32).read_unaligned() as usize;
    let mut offset: usize = 8;
    let mut count: usize = 0;
    let mut modules = [ModuleInfo::empty(); MAX_MODULES];

    loop {
        if offset >= total_size {
            break;
        }

        offset = (offset + 7) & !7;

        let tag_ptr = ptr.add(offset);
        let tag_type = (tag_ptr as *const u32).read_unaligned();
        let tag_size = (tag_ptr.add(4) as *const u32).read_unaligned();

        if tag_type == TAG_TYPE_END {
            break;
        }

        if tag_type == TAG_TYPE_MODULE && count < MAX_MODULES {
            let data = tag_ptr.add(8); // skip type + size
            let mod_start = (data as *const u32).read_unaligned() as usize;
            let mod_end = (data.add(4) as *const u32).read_unaligned() as usize;

            // String starts at offset 8 from data (after mod_start + mod_end)
            let string_ptr = data.add(8);
            let string_len = (tag_size as usize).saturating_sub(16); // 8 (type+size) + 8 (start+end)
            let mut name = [0u8; 64];
            let copy_len = string_len.min(63);
            for i in 0..copy_len {
                let ch = *string_ptr.add(i);
                if ch == 0 {
                    break;
                }
                name[i] = ch;
            }

            modules[count] = ModuleInfo {
                start: mod_start,
                end: mod_end,
                name,
            };
            count += 1;
        }

        offset += tag_size as usize;
    }

    (count, modules)
}

/// Parse memory map tags (type 6) from the multiboot2 info structure.
///
/// # Safety
/// `info_addr` must point to a valid multiboot2 boot information structure.
pub unsafe fn parse_memory_map(
    info_addr: usize,
) -> (usize, [MemoryRegion; MAX_MEMORY_REGIONS]) {
    let ptr = info_addr as *const u8;
    let total_size = (ptr as *const u32).read_unaligned() as usize;
    let mut offset: usize = 8;
    let mut count: usize = 0;
    let mut regions = [MemoryRegion::empty(); MAX_MEMORY_REGIONS];

    loop {
        if offset >= total_size {
            break;
        }

        offset = (offset + 7) & !7;

        let tag_ptr = ptr.add(offset);
        let tag_type = (tag_ptr as *const u32).read_unaligned();
        let tag_size = (tag_ptr.add(4) as *const u32).read_unaligned() as usize;

        if tag_type == TAG_TYPE_END {
            break;
        }

        if tag_type == TAG_TYPE_MMAP {
            let entry_size = (tag_ptr.add(8) as *const u32).read_unaligned() as usize;
            // entry_version at +12 (unused)
            // entries start at +16 from tag start
            let entries_start = tag_ptr.add(16);
            let entries_end = tag_ptr.add(tag_size);

            let mut entry_ptr = entries_start;
            while entry_ptr < entries_end && count < MAX_MEMORY_REGIONS {
                let base = (entry_ptr as *const u64).read_unaligned();
                let length = (entry_ptr.add(8) as *const u64).read_unaligned();
                let region_type = (entry_ptr.add(16) as *const u32).read_unaligned();

                regions[count] = MemoryRegion {
                    base,
                    length,
                    region_type,
                };
                count += 1;

                entry_ptr = entry_ptr.add(entry_size);
            }
        }

        offset += tag_size;
    }

    (count, regions)
}

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
