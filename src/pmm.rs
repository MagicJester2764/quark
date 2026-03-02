/// Physical memory manager — bitmap-based frame allocator.
///
/// Each bit in the bitmap represents one 4 KiB page frame.
/// Bit = 0 means free, bit = 1 means used.
/// Covers up to 4 GiB of physical memory (131072 bytes = 1048576 bits = 1048576 frames).

use crate::multiboot2::{MemoryRegion, MMAP_TYPE_AVAILABLE, MAX_MEMORY_REGIONS};

const PAGE_SIZE: usize = 4096;

/// 4 GiB / 4 KiB = 1048576 frames, 1048576 / 8 = 131072 bytes.
const BITMAP_SIZE: usize = 131072;

/// All frames start as used (0xFF). Init clears available regions.
static mut BITMAP: [u8; BITMAP_SIZE] = [0xFF; BITMAP_SIZE];
static mut TOTAL_FRAMES: usize = 0;
static mut FREE_FRAMES: usize = 0;

extern "C" {
    static __bss_end: u8;
}

/// A 4 KiB-aligned physical frame address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PhysFrame(usize);

impl PhysFrame {
    pub fn address(&self) -> usize {
        self.0
    }

    pub fn from_address(addr: usize) -> Self {
        PhysFrame(addr & !(PAGE_SIZE - 1))
    }
}

fn frame_index(addr: usize) -> usize {
    addr / PAGE_SIZE
}

unsafe fn set_used(frame_idx: usize) {
    BITMAP[frame_idx / 8] |= 1 << (frame_idx % 8);
}

unsafe fn set_free(frame_idx: usize) {
    BITMAP[frame_idx / 8] &= !(1 << (frame_idx % 8));
}

fn is_used(frame_idx: usize) -> bool {
    unsafe { BITMAP[frame_idx / 8] & (1 << (frame_idx % 8)) != 0 }
}

/// Mark a range of physical addresses as used in the bitmap.
unsafe fn mark_range_used(start: usize, end: usize) {
    let first = frame_index(start);
    let last = frame_index(end.saturating_sub(1));
    for i in first..=last {
        if i < BITMAP_SIZE * 8 {
            if !is_used(i) {
                set_used(i);
                FREE_FRAMES -= 1;
            }
        }
    }
}

/// Initialize the physical memory manager.
///
/// # Safety
/// Must be called once with valid memory region data from multiboot2.
/// `mb_info_addr` and `mb_info_size` describe the multiboot2 info struct location.
pub unsafe fn init(
    regions: &[MemoryRegion; MAX_MEMORY_REGIONS],
    count: usize,
    mb_info_addr: usize,
    mb_info_size: usize,
) {
    // Step 1: For each available region, clear bits (mark free).
    for i in 0..count {
        let r = &regions[i];
        if r.region_type != MMAP_TYPE_AVAILABLE {
            continue;
        }

        let base = r.base as usize;
        let length = r.length as usize;
        let end = base + length;

        let first = frame_index((base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)); // round up
        let last = frame_index(end.saturating_sub(1)); // round down to last full frame

        if first > last {
            continue;
        }

        for f in first..=last {
            if f < BITMAP_SIZE * 8 {
                set_free(f);
                FREE_FRAMES += 1;
                TOTAL_FRAMES += 1;
            }
        }
    }

    // Step 2: Re-mark reserved regions as used.

    // First 1 MiB (BIOS, video memory, etc.)
    mark_range_used(0, 0x100000);

    // Kernel: 0x100000 (1 MiB load address) through __bss_end
    let kernel_end = &__bss_end as *const u8 as usize;
    mark_range_used(0x100000, kernel_end);

    // Multiboot2 info structure
    mark_range_used(mb_info_addr, mb_info_addr + mb_info_size);

    // Boot modules (already tracked by modules registry)
    let mod_count = crate::modules::count();
    for i in 0..mod_count {
        if let Some(m) = crate::modules::get(i) {
            mark_range_used(m.start, m.end);
        }
    }
}

/// Allocate a single 4 KiB physical frame.
pub fn alloc() -> Option<PhysFrame> {
    unsafe {
        for byte_idx in 0..BITMAP_SIZE {
            if BITMAP[byte_idx] != 0xFF {
                // At least one free bit in this byte
                for bit in 0..8u8 {
                    if BITMAP[byte_idx] & (1 << bit) == 0 {
                        let frame_idx = byte_idx * 8 + bit as usize;
                        set_used(frame_idx);
                        FREE_FRAMES -= 1;
                        return Some(PhysFrame(frame_idx * PAGE_SIZE));
                    }
                }
            }
        }
    }
    None
}

/// Free a previously allocated physical frame.
pub fn free(frame: PhysFrame) {
    let idx = frame_index(frame.address());
    if idx < BITMAP_SIZE * 8 && is_used(idx) {
        unsafe {
            set_free(idx);
            FREE_FRAMES += 1;
        }
    }
}

/// Number of free 4 KiB frames.
pub fn free_count() -> usize {
    unsafe { FREE_FRAMES }
}

/// Total number of frames that were initially available.
pub fn total_count() -> usize {
    unsafe { TOTAL_FRAMES }
}
