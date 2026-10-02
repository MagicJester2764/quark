//! Physical memory manager — a bitmap frame allocator.
//!
//! One bit a frame, 0 for free, and beside it a byte a frame for who owns
//! it. Both are as long as the machine's memory makes them: they are found
//! room for at boot, in the memory they describe, where they used to be two
//! arrays compiled for four gigabytes.
//!
//! Frames are given out from both ends, and which end is the caller's to
//! say:
//!
//! - **Ordinary memory comes from the top** ([`alloc`]): a program's pages,
//!   page tables, the kernel's heap. Nothing cares where those are.
//! - **Memory a device will be told the address of comes from the bottom**
//!   ([`alloc_low`], [`alloc_contiguous`] with `low`), below four
//!   gigabytes: a network card's ring and a disk controller's table are
//!   registers thirty-two bits wide. On a machine with more memory than
//!   that, a driver started after the first four gigabytes had been used
//!   would be handed frames its device cannot reach.
//!
//! The first also keeps the kernel out of the firmware's way. What a
//! firmware reads when the machine starts is low — it has to be, to be
//! where the processor starts — and memory that was never used is memory
//! that still holds nothing (see [`scrap`] for what happens otherwise).

use crate::multiboot2::{MemoryRegion, MMAP_TYPE_AVAILABLE, MAX_MEMORY_REGIONS};
use crate::sync::IrqSpinLock;

const PAGE_SIZE: usize = 4096;

/// Where memory a device with thirty-two address lines can reach ends.
const LOW_END: usize = 1 << 32;

/// The most memory the kernel uses. Its own map of memory is the first
/// entry of the top-level page table — 512 GiB — and the last gigabyte of
/// that is the kernel's heap (`heap.rs`).
pub const MAX_PHYS: u64 = 511 << 30;

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
    /// The bitmap, and how many frames it has a bit for. Nothing until
    /// [`init`] has found it room.
    bitmap: *mut u8,
    frames: usize,
    /// For each frame, how many address spaces besides one have it as
    /// their own: nought for nearly every frame, and more for a page a
    /// `fork` left in two of them until one writes to it.
    shares: *mut u8,
    total_frames: usize,
    free_frames: usize,
    /// No byte of the bitmap above this one has a free frame in it, and
    /// none below that one: where each end's search begins.
    top: usize,
    bottom: usize,
}

// The bitmap is the allocator's alone, behind its lock.
unsafe impl Send for PmmInner {}

impl PmmInner {
    fn frame_index(addr: usize) -> usize {
        addr / PAGE_SIZE
    }

    fn bits(&mut self) -> &mut [u8] {
        if self.bitmap.is_null() {
            return &mut [];
        }
        unsafe { core::slice::from_raw_parts_mut(self.bitmap, self.frames.div_ceil(8)) }
    }

    fn sharers(&mut self) -> &mut [u8] {
        if self.shares.is_null() {
            return &mut [];
        }
        unsafe { core::slice::from_raw_parts_mut(self.shares, self.frames) }
    }

    fn set_used(&mut self, frame_idx: usize) {
        self.bits()[frame_idx / 8] |= 1 << (frame_idx % 8);
    }

    fn set_free(&mut self, frame_idx: usize) {
        self.bits()[frame_idx / 8] &= !(1 << (frame_idx % 8));
        self.top = self.top.max(frame_idx / 8);
        self.bottom = self.bottom.min(frame_idx / 8);
    }

    fn is_used(&mut self, frame_idx: usize) -> bool {
        self.bits()[frame_idx / 8] & (1 << (frame_idx % 8)) != 0
    }

    fn mark_range_used(&mut self, start: usize, end: usize) {
        let first = Self::frame_index(start);
        let last = Self::frame_index(end.saturating_sub(1));
        for i in first..=last {
            if i < self.frames && !self.is_used(i) {
                self.set_used(i);
                self.free_frames -= 1;
            }
        }
    }

    /// The highest free frame, taken.
    fn take_high(&mut self) -> Option<PhysFrame> {
        let mut byte = self.top;
        let bits = self.bits();
        if bits.is_empty() {
            return None;
        }
        byte = byte.min(bits.len() - 1);
        loop {
            if bits[byte] != 0xFF {
                // The highest clear bit of the byte. A frame past the end
                // of memory has a bit that is never cleared.
                let bit = 7 - (!bits[byte]).leading_zeros() as usize;
                bits[byte] |= 1 << bit;
                self.top = byte;
                self.free_frames -= 1;
                return Some(PhysFrame((byte * 8 + bit) * PAGE_SIZE));
            }
            if byte == 0 {
                self.top = 0;
                return None;
            }
            byte -= 1;
        }
    }

    /// The lowest free frame below four gigabytes, taken.
    fn take_low(&mut self) -> Option<PhysFrame> {
        let limit = self.frames.min(LOW_END / PAGE_SIZE).div_ceil(8);
        let from = self.bottom;
        let bits = self.bits();
        for byte in from..limit {
            if bits[byte] != 0xFF {
                let bit = (!bits[byte]).trailing_zeros() as usize;
                let frame = byte * 8 + bit;
                // The last byte below the line may have frames above it.
                if frame >= LOW_END / PAGE_SIZE {
                    break;
                }
                bits[byte] |= 1 << bit;
                self.bottom = byte;
                self.free_frames -= 1;
                return Some(PhysFrame(frame * PAGE_SIZE));
            }
        }
        self.bottom = self.bottom.max(limit.saturating_sub(1));
        None
    }

    /// `count` free frames in a row, taken: the lowest such run below four
    /// gigabytes for `low`, and the highest anywhere otherwise.
    fn take_run(&mut self, count: usize, low: bool) -> Option<PhysFrame> {
        let frames = if low { self.frames.min(LOW_END / PAGE_SIZE) } else { self.frames };
        let mut run = 0;
        let mut found = None;
        if low {
            for frame in 0..frames {
                run = if self.is_used(frame) { 0 } else { run + 1 };
                if run == count {
                    found = Some(frame + 1 - count);
                    break;
                }
            }
        } else {
            for frame in (0..frames).rev() {
                run = if self.is_used(frame) { 0 } else { run + 1 };
                if run == count {
                    found = Some(frame);
                    break;
                }
            }
        }
        let start = found?;
        for frame in start..start + count {
            self.set_used(frame);
        }
        self.free_frames -= count;
        Some(PhysFrame(start * PAGE_SIZE))
    }
}

static PMM: IrqSpinLock<PmmInner> = IrqSpinLock::new(PmmInner {
    bitmap: core::ptr::null_mut(),
    frames: 0,
    shares: core::ptr::null_mut(),
    total_frames: 0,
    free_frames: 0,
    top: 0,
    bottom: 0,
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
    let regions = &regions[..count];
    let kernel_end = &__bss_end as *const u8 as usize;
    let usable = |r: &&MemoryRegion| r.region_type == MMAP_TYPE_AVAILABLE && !scrap(regions, r);

    // How much memory there is: where the last of it ends.
    let top = regions
        .iter()
        .filter(usable)
        .map(|r| r.base.saturating_add(r.length))
        .max()
        .unwrap_or(0)
        .min(MAX_PHYS) as usize;
    let frames = top / PAGE_SIZE;

    // Room for a bit a frame and then two bytes a frame — who owns it, and
    // how many share it — in memory the boot map reaches, below four
    // gigabytes, that nothing else is in: not the first megabyte, the
    // kernel, what the bootloader passed or a module.
    let bitmap_len = frames.div_ceil(8);
    let room = (bitmap_len + 2 * frames + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let in_the_way = |start: usize, end: usize| -> Option<usize> {
        let hits = |a: usize, b: usize| (start < b && a < end).then_some(b);
        let mut past = hits(0, 0x100000)
            .or(hits(0x100000, kernel_end))
            .or(hits(mb_info_addr, mb_info_addr + mb_info_size));
        for i in 0..crate::modules::count() {
            if let Some(m) = crate::modules::get(i) {
                past = past.or(hits(m.start, m.end));
            }
        }
        past
    };
    let mut place = None;
    'regions: for r in regions.iter().filter(usable) {
        let end = (r.base.saturating_add(r.length) as usize).min(LOW_END);
        let mut at = (r.base as usize + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        while at.checked_add(room).is_some_and(|stop| stop <= end) {
            match in_the_way(at, at + room) {
                Some(past) => at = (past + PAGE_SIZE - 1) & !(PAGE_SIZE - 1),
                None => {
                    place = Some(at);
                    break 'regions;
                }
            }
        }
    }
    let Some(place) = place else {
        panic!("no room below four gigabytes for the table of frames");
    };
    core::ptr::write_bytes(place as *mut u8, 0xFF, bitmap_len);
    core::ptr::write_bytes((place + bitmap_len) as *mut u8, 0, 2 * frames);
    {
        let mut owners = FRAME_OWNER.lock();
        owners.table = (place + bitmap_len) as *mut u8;
        owners.frames = frames;
    }

    let mut pmm = PMM.lock();
    pmm.bitmap = place as *mut u8;
    pmm.frames = frames;
    pmm.shares = (place + bitmap_len + frames) as *mut u8;

    // Step 1: For each available region, clear bits (mark free).
    let mut scraps = 0u64;
    for r in regions {
        if r.region_type != MMAP_TYPE_AVAILABLE {
            continue;
        }
        if scrap(regions, r) {
            scraps += r.length;
            continue;
        }

        let base = r.base as usize;
        let length = r.length as usize;
        // A bogus firmware entry must not wrap the end address.
        let end = match base.checked_add(length) {
            Some(e) => e.min(top),
            None => continue,
        };

        let first = PmmInner::frame_index((base + PAGE_SIZE - 1) & !(PAGE_SIZE - 1));
        let last = PmmInner::frame_index(end.saturating_sub(1));

        if first > last || end == 0 {
            continue;
        }

        for f in first..=last {
            // Guard on the current bit: overlapping or duplicated entries in
            // the firmware memory map would otherwise inflate both counters
            // and, worse, let free_frames underflow later.
            if f < frames && pmm.is_used(f) {
                pmm.set_free(f);
                pmm.free_frames += 1;
                pmm.total_frames += 1;
            }
        }
    }
    pmm.top = bitmap_len.saturating_sub(1);
    pmm.bottom = 0;

    // Step 2: Re-mark reserved regions as used.

    // First 1 MiB (BIOS, video memory, etc.)
    pmm.mark_range_used(0, 0x100000);

    // Kernel: 0x100000 (1 MiB load address) through __bss_end
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
    // And the tables themselves.
    pmm.mark_range_used(place, place + room);
    drop(pmm);
    if scraps != 0 {
        crate::serial::puts(b"Memory: ");
        crate::serial::put_usize((scraps / 1024) as usize);
        crate::serial::puts(b" KiB in scraps between the firmware's own, left alone.\n");
    }
}}

/// Where the machine's memory ends: the address after the last frame the
/// allocator knows of.
pub fn top_of_memory() -> usize {
    PMM.lock().frames * PAGE_SIZE
}

/// Allocate a single 4 KiB physical frame: ordinary memory, from the top.
///
/// Not before every processor has forgotten the mappings taken away since
/// they were last told (`tlb.rs`): the frame handed out here may be one of
/// those, and a thread on another processor could still reach it.
pub fn alloc() -> Option<PhysFrame> {
    crate::tlb::sync();
    PMM.lock().take_high()
}

/// Allocate a single frame below four gigabytes: one a device will be told
/// the address of. `None` when there is none there, whatever is free above.
pub fn alloc_low() -> Option<PhysFrame> {
    crate::tlb::sync();
    PMM.lock().take_low()
}

/// Allocate `count` physically contiguous 4 KiB frames: below four
/// gigabytes for `low`, and from the top otherwise.
/// Returns the first frame on success, or None if no contiguous run is found.
pub fn alloc_contiguous(count: usize, low: bool) -> Option<PhysFrame> {
    if count == 0 {
        return None;
    }
    if count == 1 {
        return if low { alloc_low() } else { alloc() };
    }
    crate::tlb::sync();
    PMM.lock().take_run(count, low)
}

/// One more address space has `frame` as its own: a `fork` has left a page
/// in the child that the parent has too. False if that many have it already
/// that the count cannot say one more, and the caller must copy instead.
pub fn share(frame: usize) -> bool {
    let mut pmm = PMM.lock();
    let idx = PmmInner::frame_index(frame);
    match pmm.sharers().get_mut(idx) {
        Some(count) if *count < u8::MAX => {
            *count += 1;
            true
        }
        _ => false,
    }
}

/// How many address spaces besides one have `frame` as their own.
pub fn shared(frame: usize) -> u8 {
    let mut pmm = PMM.lock();
    let idx = PmmInner::frame_index(frame);
    pmm.sharers().get(idx).copied().unwrap_or(0)
}

/// Give a frame back: whoever had it has it no longer.
///
/// It goes back to the allocator if nobody else has it, and is one fewer's
/// if somebody does ([`share`]). There is one way back for a frame and this
/// is it, so that nothing can free one out from under whoever it is shared
/// with by not knowing that it was.
pub fn free(frame: PhysFrame) {
    let mut pmm = PMM.lock();
    let idx = PmmInner::frame_index(frame.address());
    if idx >= pmm.frames {
        return;
    }
    if pmm.sharers()[idx] > 0 {
        pmm.sharers()[idx] -= 1;
        return;
    }
    if pmm.is_used(idx) {
        pmm.set_free(idx);
        pmm.free_frames += 1;
    }
}

// ---------------------------------------------------------------------------
// Frame ownership
// ---------------------------------------------------------------------------

/// Owner of each frame handed to user space by `sys_phys_alloc`, stored as
/// `tid + 1` so that 0 means "not owned by any task".
///
/// `sys_phys_free` takes a raw physical address from user space. Without this
/// the kernel could not distinguish a task's own frames from the kernel's, so a
/// CAP_PHYS_ALLOC holder could feed the allocator any address at all — and
/// frames were never reclaimed when their owner died.
///
/// Tracking is per frame rather than per allocation because callers allocate a
/// page at a time (init maps ELF images page by page), so any fixed table of
/// (base, count) reservations is exhausted almost immediately.
///
/// And how many each task owns, so that one that owns none — nearly every
/// task — is not looked for through a byte for every frame of the machine
/// when it is reaped.
struct FrameOwners {
    table: *mut u8,
    frames: usize,
    owned: [u32; 256],
}

unsafe impl Send for FrameOwners {}

impl FrameOwners {
    fn bytes(&mut self) -> &mut [u8] {
        if self.table.is_null() {
            return &mut [];
        }
        unsafe { core::slice::from_raw_parts_mut(self.table, self.frames) }
    }
}

static FRAME_OWNER: IrqSpinLock<FrameOwners> = IrqSpinLock::new(FrameOwners {
    table: core::ptr::null_mut(),
    frames: 0,
    owned: [0; 256],
});

/// Record that `owner` holds the `count` frames starting at `base`.
pub fn set_owner(base: usize, count: usize, owner: usize) {
    if owner >= 0xFF {
        return;
    }
    let mut owners = FRAME_OWNER.lock();
    for i in 0..count {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        let Some(&was) = owners.bytes().get(idx) else { continue };
        if was != 0 {
            owners.owned[was as usize] = owners.owned[was as usize].saturating_sub(1);
        }
        owners.bytes()[idx] = owner as u8 + 1;
        owners.owned[owner + 1] += 1;
    }
}

/// True if every frame in `[base, base + count)` is owned by `owner`.
pub fn owns_range(base: usize, count: usize, owner: usize) -> bool {
    if owner >= 0xFF {
        return false;
    }
    let want = owner as u8 + 1;
    let mut owners = FRAME_OWNER.lock();
    (0..count).all(|i| {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        owners.bytes().get(idx) == Some(&want)
    })
}

/// Drop the ownership record for `[base, base + count)`.
pub fn clear_owner(base: usize, count: usize) {
    let mut owners = FRAME_OWNER.lock();
    for i in 0..count {
        let idx = PmmInner::frame_index(base + i * PAGE_SIZE);
        let Some(&was) = owners.bytes().get(idx) else { continue };
        if was != 0 {
            owners.owned[was as usize] = owners.owned[was as usize].saturating_sub(1);
            owners.bytes()[idx] = 0;
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
    loop {
        let mut batch = [0usize; 64];
        let mut n = 0;
        {
            let mut owners = FRAME_OWNER.lock();
            if owners.owned[want as usize] == 0 {
                break;
            }
            let frames = owners.frames;
            while idx < frames && n < batch.len() {
                if owners.bytes()[idx] == want {
                    owners.bytes()[idx] = 0;
                    owners.owned[want as usize] -= 1;
                    batch[n] = idx * PAGE_SIZE;
                    n += 1;
                }
                idx += 1;
            }
            if n == 0 {
                // Looked at every frame and found none: the count was wrong,
                // and is put right rather than looked for again.
                owners.owned[want as usize] = 0;
                break;
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
pub fn total_count() -> usize {
    PMM.lock().total_frames
}
