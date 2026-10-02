/// Physical memory manager — bitmap-based frame allocator.
///
/// Each bit in the bitmap represents one 4 KiB page frame.
/// Bit = 0 means free, bit = 1 means used.
/// Covers up to 4 GiB of physical memory (131072 bytes = 1048576 bits = 1048576 frames).

use crate::multiboot2::{MemoryRegion, MMAP_TYPE_AVAILABLE, MAX_MEMORY_REGIONS};
use crate::sync::IrqSpinLock;

const PAGE_SIZE: usize = 4096;

/// 4 GiB / 4 KiB = 1048576 frames, 1048576 / 8 = 131072 bytes.
const BITMAP_SIZE: usize = 131072;

unsafe extern "C" {
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

/// A stretch of ordinary memory shorter than this, with memory the firmware
/// keeps for itself on both sides of it, is a scrap ([`scrap`]).
const SCRAP: u64 = 1 << 20;

/// Whether `r`, which the firmware says is ordinary memory, is a scrap left
/// between two stretches the firmware keeps for itself — and so memory the
/// kernel leaves alone.
///
/// What a firmware keeps is what it uses while the machine starts, and it
/// starts again without the memory having been cleared: a restart is not a
/// power cycle. One that reads a page it did not keep finds what the last
/// system left there. OVMF does: a page in the middle of its own megabyte
/// (0x813000) is where a confidential guest's loader would have told it how
/// many processors there are, it gives that page out as ordinary memory, and
/// after a restart it read a program's text there as a count of 116 and
/// waited seventy-one minutes for them to arrive. This kernel gives out the
/// lowest frame that is free, so that page was among the first it used.
///
/// The memory map cannot say which pages a firmware reads and does not
/// keep. It can say where a firmware's own memory is, and a scrap in the
/// middle of that is forty kilobytes not worth the question.
fn scrap(regions: &[MemoryRegion], r: &MemoryRegion) -> bool {
    let end = r.base.saturating_add(r.length);
    let firmware = |o: &&MemoryRegion| o.region_type != MMAP_TYPE_AVAILABLE && o.length != 0;
    r.length < SCRAP
        && regions.iter().filter(firmware).any(|o| o.base.saturating_add(o.length) == r.base)
        && regions.iter().filter(firmware).any(|o| o.base == end)
}

struct PmmInner {
    bitmap: [u8; BITMAP_SIZE],
    total_frames: usize,
    free_frames: usize,
}

impl PmmInner {
    fn frame_index(addr: usize) -> usize {
        addr / PAGE_SIZE
    }

    fn set_used(&mut self, frame_idx: usize) {
        self.bitmap[frame_idx / 8] |= 1 << (frame_idx % 8);
    }

    fn set_free(&mut self, frame_idx: usize) {
        self.bitmap[frame_idx / 8] &= !(1 << (frame_idx % 8));
    }

    fn is_used(&self, frame_idx: usize) -> bool {
        self.bitmap[frame_idx / 8] & (1 << (frame_idx % 8)) != 0
    }

    fn mark_range_used(&mut self, start: usize, end: usize) {
        let first = Self::frame_index(start);
        let last = Self::frame_index(end.saturating_sub(1));
        for i in first..=last {
            if i < BITMAP_SIZE * 8 && !self.is_used(i) {
                self.set_used(i);
                self.free_frames -= 1;
            }
        }
    }
}

static PMM: IrqSpinLock<PmmInner> = IrqSpinLock::new(PmmInner {
    bitmap: [0xFF; BITMAP_SIZE],
    total_frames: 0,
    free_frames: 0,
});

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
) { unsafe {
    let mut pmm = PMM.lock();

    // Step 1: For each available region, clear bits (mark free).
    let mut scraps = 0u64;
    for i in 0..count {
        let r = &regions[i];
        if r.region_type != MMAP_TYPE_AVAILABLE {
            continue;
        }
        if scrap(&regions[..count], r) {
            scraps += r.length;
            continue;
        }

        let base = r.base as usize;
        let length = r.length as usize;
        // A bogus firmware entry must not wrap the end address.
        let end = match base.checked_add(length) {
            Some(e) => e,
            None => continue,
        };

        let first = PmmInner::frame_index((base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1));
        let last = PmmInner::frame_index(end.saturating_sub(1));

        if first > last {
            continue;
        }

        for f in first..=last {
            // Guard on the current bit: overlapping or duplicated entries in
            // the firmware memory map would otherwise inflate both counters
            // and, worse, let free_frames underflow later.
            if f < BITMAP_SIZE * 8 && pmm.is_used(f) {
                pmm.set_free(f);
                pmm.free_frames += 1;
                pmm.total_frames += 1;
            }
        }
    }

    // Step 2: Re-mark reserved regions as used.

    // First 1 MiB (BIOS, video memory, etc.)
    pmm.mark_range_used(0, 0x100000);

    // Kernel: 0x100000 (1 MiB load address) through __bss_end
    let kernel_end = &__bss_end as *const u8 as usize;
    pmm.mark_range_used(0x100000, kernel_end);

    // Multiboot2 info structure
    pmm.mark_range_used(mb_info_addr, mb_info_addr + mb_info_size);

    // Boot modules (already tracked by modules registry)
    let mod_count = crate::modules::count();
    for i in 0..mod_count {
        if let Some(m) = crate::modules::get(i) {
            pmm.mark_range_used(m.start, m.end);
        }
    }
    drop(pmm);
    if scraps != 0 {
        crate::serial::puts(b"Memory: ");
        crate::serial::put_usize((scraps / 1024) as usize);
        crate::serial::puts(b" KiB in scraps between the firmware's own, left alone.\n");
    }
}}

/// Allocate a single 4 KiB physical frame.
///
/// Not before every processor has forgotten the mappings taken away since
/// they were last told (`tlb.rs`): the frame handed out here may be one of
/// those, and a thread on another processor could still reach it.
pub fn alloc() -> Option<PhysFrame> {
    crate::tlb::sync();
    let mut pmm = PMM.lock();
    for byte_idx in 0..BITMAP_SIZE {
        if pmm.bitmap[byte_idx] != 0xFF {
            for bit in 0..8u8 {
                if pmm.bitmap[byte_idx] & (1 << bit) == 0 {
                    let frame_idx = byte_idx * 8 + bit as usize;
                    pmm.set_used(frame_idx);
                    pmm.free_frames -= 1;
                    return Some(PhysFrame(frame_idx * PAGE_SIZE));
                }
            }
        }
    }
    None
}

/// Allocate `count` physically contiguous 4 KiB frames.
/// Returns the first frame on success, or None if no contiguous run is found.
pub fn alloc_contiguous(count: usize) -> Option<PhysFrame> {
    if count == 0 {
        return None;
    }
    if count == 1 {
        return alloc();
    }
    crate::tlb::sync();
    let mut pmm = PMM.lock();
    let max_frame = BITMAP_SIZE * 8;
    let mut run_start = 0;
    let mut run_len = 0;
    for frame_idx in 0..max_frame {
        if pmm.is_used(frame_idx) {
            run_start = frame_idx + 1;
            run_len = 0;
        } else {
            run_len += 1;
            if run_len == count {
                for i in run_start..run_start + count {
                    pmm.set_used(i);
                    pmm.free_frames -= 1;
                }
                return Some(PhysFrame(run_start * PAGE_SIZE));
            }
        }
    }
    None
}

/// Free a previously allocated physical frame.
pub fn free(frame: PhysFrame) {
    let mut pmm = PMM.lock();
    let idx = PmmInner::frame_index(frame.address());
    if idx < BITMAP_SIZE * 8 && pmm.is_used(idx) {
        pmm.set_free(idx);
        pmm.free_frames += 1;
    }
}

// ---------------------------------------------------------------------------
// Frame ownership
// ---------------------------------------------------------------------------

/// Number of frames the bitmap covers.
const MAX_FRAMES: usize = BITMAP_SIZE * 8;

/// Owner of each frame handed to user space by `sys_phys_alloc`, stored as
/// `tid + 1` so that 0 means "not owned by any task" and the whole table lands
/// in .bss rather than .data.
///
/// `sys_phys_free` takes a raw physical address from user space. Without this
/// the kernel could not distinguish a task's own frames from the kernel's, so a
/// CAP_PHYS_ALLOC holder could feed the allocator any address at all — and
/// frames were never reclaimed when their owner died.
///
/// Tracking is per frame rather than per allocation because callers allocate a
/// page at a time (init maps ELF images page by page), so any fixed table of
/// (base, count) reservations is exhausted almost immediately.
struct FrameOwners {
    table: [u8; MAX_FRAMES],
}

static FRAME_OWNER: IrqSpinLock<FrameOwners> = IrqSpinLock::new(FrameOwners {
    table: [0u8; MAX_FRAMES],
});

/// Record that `owner` holds the `count` frames starting at `base`.
pub fn set_owner(base: usize, count: usize, owner: usize) {
    if owner >= 0xFF {
        return;
    }
    let mut owners = FRAME_OWNER.lock();
    for i in 0..count {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        if idx < MAX_FRAMES {
            owners.table[idx] = owner as u8 + 1;
        }
    }
}

/// True if every frame in `[base, base + count)` is owned by `owner`.
pub fn owns_range(base: usize, count: usize, owner: usize) -> bool {
    if owner >= 0xFF {
        return false;
    }
    let want = owner as u8 + 1;
    let owners = FRAME_OWNER.lock();
    (0..count).all(|i| {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        idx < MAX_FRAMES && owners.table[idx] == want
    })
}

/// Drop the ownership record for `[base, base + count)`.
pub fn clear_owner(base: usize, count: usize) {
    let mut owners = FRAME_OWNER.lock();
    for i in 0..count {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        if idx < MAX_FRAMES {
            owners.table[idx] = 0;
        }
    }
}

/// Free every frame still owned by `owner`. Returns the number reclaimed.
/// Called when a task is reaped so its `sys_phys_alloc` frames are not leaked.
pub fn release_task_frames(owner: usize) -> usize {
    if owner >= 0xFF {
        return 0;
    }
    let want = owner as u8 + 1;
    let mut reclaimed = 0;

    // Collect under the ownership lock, free outside it: pmm::free takes the
    // PMM lock and nesting the two would risk a deadlock.
    let mut idx = 0;
    while idx < MAX_FRAMES {
        let mut batch = [0usize; 64];
        let mut n = 0;
        {
            let mut owners = FRAME_OWNER.lock();
            while idx < MAX_FRAMES && n < batch.len() {
                if owners.table[idx] == want {
                    owners.table[idx] = 0;
                    batch[n] = idx * PAGE_SIZE;
                    n += 1;
                }
                idx += 1;
            }
        }
        for &addr in batch.iter().take(n) {
            free(PhysFrame::from_address(addr));
        }
        reclaimed += n;
    }
    reclaimed
}

/// Number of free 4 KiB frames.
pub fn free_count() -> usize {
    PMM.lock().free_frames
}

/// Total number of frames that were initially available.
#[allow(dead_code)] // memory statistics API
pub fn total_count() -> usize {
    PMM.lock().total_frames
}
