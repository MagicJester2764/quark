/// Shared memory regions for zero-copy data sharing between tasks.
///
/// Tasks create a shared region (kernel allocates physical pages),
/// grant access to other tasks, and each maps it into their own address space.
///
/// Destroying a region only returns its frames to the PMM once nobody has it
/// mapped. Freeing them while another task still held a mapping left that task
/// with live PTEs pointing at frames the allocator would hand out again as page
/// tables or kernel heap — a direct route from ring 3 to arbitrary kernel
/// memory. Regions destroyed while still mapped are therefore marked
/// `pending_destroy` and reclaimed by the last unmapper.

use crate::{paging, pmm, scheduler};
use crate::task::MAX_TASKS;

/// Regions in the system.
///
/// Thirty-two was half a region per task. Linux's System V limit is 4096 and
/// its POSIX shared memory has no count at all; macOS's 32 is a legacy knob
/// nothing modern uses. This is a fixed array like everything else in this
/// kernel, so the number is what it costs. With the run list below a region is
/// about 304 bytes, so 256 of them is roughly seventy-six kilobytes — most of
/// it the runs, which is the price of not needing one contiguous span.
const MAX_SHMEM: usize = 256;

/// Pages one region may hold.
///
/// Sixteen once, which was fine for passing a buffer between two services and
/// useless for the thing shared memory is most obviously for: a window. Then
/// 1024, which is exactly a 1280x800 window and therefore one buffer and not
/// two. A 1920x1080 buffer is 2025 pages, so this is two of them with room.
const MAX_PAGES_PER_REGION: usize = 4096;

/// Contiguous runs one region may be assembled from.
///
/// A region used to be a single run, which made the page limit a promise the
/// allocator could not keep: 4096 contiguous pages is sixteen megabytes in one
/// piece, on a machine with a hundred and twenty-eight. Sixteen runs covers any
/// real allocation and costs 256 bytes per region, against the eight kilobytes
/// an array of frame addresses would have — which is why the original chose a
/// single run.
const MAX_RUNS: usize = 16;

#[derive(Clone, Copy)]
struct Run {
    base: usize,
    pages: usize,
}

/// `access`/`mapped` are TID bitmasks, one bit per task.
const _: () = assert!(MAX_TASKS <= u64::BITS as usize);

struct ShmemRegion {
    in_use: bool,
    /// The contiguous runs this region is assembled from, in order.
    runs: [Run; MAX_RUNS],
    run_count: usize,
    page_count: usize,
    creator: usize,
    /// Bitmask of TIDs with access (bit N = TID N can map).
    access: u64,
    /// Bitmask of TIDs that currently have the region mapped.
    mapped: u64,
    /// Set when destroy was requested while the region was still mapped.
    /// The last task to unmap frees the frames and releases the handle.
    pending_destroy: bool,
    /// Descriptors for this region sitting in a stream's queue, sent but not
    /// yet received.
    ///
    /// The access mask is keyed by task, and a queued descriptor belongs to no
    /// task yet — the receiver is not decided until it calls recv. Without a
    /// count that belongs to nobody, a sender that passes a region and then
    /// closes its own copy drops the last reference and the region is freed
    /// under the descriptor still travelling towards its peer.
    in_flight: u32,
}

impl ShmemRegion {
    const fn empty() -> Self {
        ShmemRegion {
            in_use: false,
            runs: [Run { base: 0, pages: 0 }; MAX_RUNS],
            run_count: 0,
            page_count: 0,
            creator: 0,
            access: 0,
            mapped: 0,
            pending_destroy: false,
            in_flight: 0,
        }
    }
}

static mut REGIONS: [ShmemRegion; MAX_SHMEM] = {
    const INIT: ShmemRegion = ShmemRegion::empty();
    [INIT; MAX_SHMEM]
};

/// Save RFLAGS and disable interrupts. Returns saved flags.
#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

/// Restore RFLAGS (re-enabling interrupts if they were enabled before).
#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Borrow the region table. Callers must already hold interrupts off.
#[inline(always)]
unsafe fn regions() -> &'static mut [ShmemRegion; MAX_SHMEM] { unsafe {
    &mut *core::ptr::addr_of_mut!(REGIONS)
}}

impl ShmemRegion {
    /// Physical address of the region's `index`-th page.
    ///
    /// Walks the runs rather than dividing, because they are unequal. Sixteen
    /// at most, and every caller walks a region in order anyway.
    fn frame_at(&self, index: usize) -> Option<usize> {
        let mut seen = 0;
        for r in &self.runs[..self.run_count] {
            if index < seen + r.pages {
                return Some(r.base + (index - seen) * 4096);
            }
            seen += r.pages;
        }
        None
    }
}

/// Release a region's frames and reset the slot. Interrupts must be off.
unsafe fn release(region: &mut ShmemRegion) {
    let creator = region.creator;
    let freed = unsafe { release_frames(region) };
    // Refund the creator's quota.
    scheduler::uncharge_task_mem(creator, freed);
    *region = ShmemRegion::empty();
}

/// Return a region's frames to the allocator and empty its run list, leaving
/// the slot claimed. Returns how many pages went back. Interrupts must be off.
///
/// Split out from `release` for `resize`, which gives the frames up and takes
/// new ones without the slot ever ceasing to exist — the handle stays valid
/// throughout, because a descriptor already names it.
unsafe fn release_frames(region: &mut ShmemRegion) -> usize {
    let mut freed = 0;
    for r in &region.runs[..region.run_count] {
        for j in 0..r.pages {
            pmm::free(pmm::PhysFrame::from_address(r.base + j * 4096));
            freed += 1;
        }
    }
    region.runs = [Run { base: 0, pages: 0 }; MAX_RUNS];
    region.run_count = 0;
    freed
}

/// Create a shared memory region. Returns handle (0..31) or u64::MAX on error.
pub fn create(pages: usize) -> u64 {
    if pages == 0 || pages > MAX_PAGES_PER_REGION {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    // Shared pages are charged to their creator, so a task cannot sidestep its
    // memory limit by parking allocations in shared regions.
    if !scheduler::current_task_check_mem(pages) {
        return u64::MAX;
    }

    // Claim the slot before allocating. pmm::alloc takes a lock that re-enables
    // interrupts on release, so a preempting task used to be able to pick the
    // same "free" handle and scribble over this region.
    let flags = irq_save();
    let handle = unsafe {
        match regions().iter().position(|r| !r.in_use) {
            Some(h) => h,
            None => {
                irq_restore(flags);
                return u64::MAX;
            }
        }
    };
    unsafe {
        let region = &mut regions()[handle];
        *region = ShmemRegion::empty();
        region.in_use = true;
        region.creator = tid;
        region.access = 1u64 << tid;
        region.page_count = pages;
    }
    irq_restore(flags);

    if !fill(handle, pages) {
        return u64::MAX;
    }

    scheduler::current_task_charge_mem(pages);
    handle as u64
}

/// Give a claimed region its frames. False if the machine has not got them, in
/// which case the slot is released and the handle is no longer valid.
///
/// The caller has already claimed the slot and set `page_count`; this is only
/// the allocation, which is the part `resize` needs to do again.
fn fill(handle: usize, pages: usize) -> bool {
    // Take the largest contiguous runs the allocator will give, halving the
    // request whenever it refuses. A fresh machine satisfies this in one run;
    // a fragmented one in several, which is the entire point of a run list.
    let mut want = pages;
    let mut got = 0usize;
    let mut runs = [Run { base: 0, pages: 0 }; MAX_RUNS];
    let mut run_count = 0usize;
    while got < pages && run_count < MAX_RUNS && want > 0 {
        let ask = want.min(pages - got);
        match pmm::alloc_contiguous(ask) {
            Some(frame) => {
                runs[run_count] = Run { base: frame.address(), pages: ask };
                run_count += 1;
                got += ask;
            }
            None => want /= 2,
        }
    }
    if got < pages {
        // Hand back what was taken. `release` frees by the run list, so give
        // it the partial one rather than leaking it.
        let flags = irq_save();
        unsafe {
            let region = &mut regions()[handle];
            region.runs = runs;
            region.run_count = run_count;
            release(region);
        }
        irq_restore(flags);
        return false;
    }
    // Zero every run (identity-mapped) so nothing leaks from a previous owner.
    for r in &runs[..run_count] {
        unsafe { core::ptr::write_bytes(r.base as *mut u8, 0, r.pages * 4096) };
    }
    let flags = irq_save();
    unsafe {
        let region = &mut regions()[handle];
        region.runs = runs;
        region.run_count = run_count;
        region.page_count = pages;
    }
    irq_restore(flags);
    true
}

/// Give an existing region a new size, in pages.
///
/// This is `ftruncate` on a memory descriptor, and it is deliberately narrow:
/// only while nobody has the region mapped, nobody else has been admitted to
/// it, and no descriptor for it is travelling. That is the whole of how a libc
/// uses it — `memfd_create` then `ftruncate` then `mmap`, before the descriptor
/// has been anywhere — and outside that window growing a region would change
/// what is behind somebody else's live mapping.
pub fn resize(handle: usize, pages: usize) -> u64 {
    if handle >= MAX_SHMEM || pages == 0 || pages > MAX_PAGES_PER_REGION {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }

    let flags = irq_save();
    let old_pages = unsafe {
        let region = &mut regions()[handle];
        if !region.in_use
            || region.pending_destroy
            || region.mapped != 0
            || region.in_flight != 0
            || region.access != 1u64 << tid
            || region.creator != tid
        {
            irq_restore(flags);
            return u64::MAX;
        }
        if region.page_count == pages {
            irq_restore(flags);
            return pages as u64;
        }
        let old = region.page_count;
        // Let go of the old frames before asking for new ones: on a machine
        // with just enough memory, holding both is the difference between a
        // resize that works and one that does not.
        release_frames(region);
        region.page_count = pages;
        old
    };
    irq_restore(flags);
    scheduler::uncharge_task_mem(tid, old_pages);

    if !scheduler::current_task_check_mem(pages) {
        // Put it back the way it was found, so a refused resize does not also
        // destroy the memory the caller already had.
        let flags = irq_save();
        unsafe { regions()[handle].page_count = 0 };
        irq_restore(flags);
        let _ = fill(handle, old_pages);
        return u64::MAX;
    }
    if !fill(handle, pages) {
        return u64::MAX;
    }
    scheduler::current_task_charge_mem(pages);
    pages as u64
}

/// Map a shared memory region into the caller's address space.
/// vaddr must be page-aligned and in user space.
/// Map a region into the caller. Returns the number of pages, or `u64::MAX`.
pub fn map(handle: usize, vaddr: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let cr3 = paging::read_cr3();

    let flags = irq_save();
    let result = unsafe {
        let region = &mut regions()[handle];
        if !region.in_use || region.pending_destroy {
            irq_restore(flags);
            return u64::MAX;
        }

        // Check access
        if region.access & (1u64 << tid) == 0 {
            irq_restore(flags);
            return u64::MAX;
        }

        let page_count = region.page_count;
        if !paging::user_range_ok(vaddr, page_count) {
            irq_restore(flags);
            return u64::MAX;
        }

        // No OWNED bit: these frames belong to the region, not to this address
        // space. munmap and address-space teardown must not free them.
        let pte_flags = paging::PRESENT | paging::WRITABLE | paging::USER;
        for i in 0..page_count {
            let v = vaddr + i * 4096;
            let phys = match region.frame_at(i) {
                Some(p) => p,
                None => {
                    for j in 0..i {
                        let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                    }
                    irq_restore(flags);
                    return u64::MAX;
                }
            };
            if paging::map_page(cr3, v, phys, pte_flags).is_err() {
                for j in 0..i {
                    let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                }
                irq_restore(flags);
                return u64::MAX;
            }
        }
        region.mapped |= 1u64 << tid;
        page_count as u64
    };
    irq_restore(flags);
    result
}

/// Admit `tid` to a region, with no check on the caller.
///
/// This is not `grant`: it is reachable only when a *descriptor* for the region
/// changes hands — received over a stream, or duplicated into a task by
/// somebody already holding `TaskMgmt` over it. In both cases the transfer was
/// asked for by one side and chosen by the other, which is more than `grant`
/// requires of anybody.
pub fn add_access(handle: usize, tid: usize) -> bool {
    if handle >= MAX_SHMEM || tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let r = &mut regions()[handle];
        if r.in_use && !r.pending_destroy {
            r.access |= 1u64 << tid;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    ok
}

/// Drop one task's descriptor reference to a region.
///
/// `tid` is passed rather than taken from the current task because this is
/// reached from the reaper as well as from a task closing its own descriptor,
/// and the reaper is not the task whose descriptors it is releasing.
pub fn close_ref(handle: usize, tid: usize) {
    if handle >= MAX_SHMEM || tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let r = &mut regions()[handle];
        if r.in_use {
            r.access &= !(1u64 << tid);
            if r.access == 0 && r.in_flight == 0 {
                if r.mapped == 0 {
                    release(r);
                } else {
                    // Somebody still has it mapped. Revocation governs the
                    // right to map, not mappings that already exist, so the
                    // frames go when the last mapper unmaps.
                    r.pending_destroy = true;
                }
            }
        }
    }
    irq_restore(flags);
}

/// Take a reference held by nobody, for a descriptor in flight.
pub fn hold_in_flight(handle: usize) -> bool {
    if handle >= MAX_SHMEM {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let r = &mut regions()[handle];
        if r.in_use && !r.pending_destroy {
            r.in_flight += 1;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    ok
}

/// Release an in-flight reference — the descriptor arrived, or was dropped
/// with the stream that was carrying it.
pub fn drop_in_flight(handle: usize) {
    if handle >= MAX_SHMEM {
        return;
    }
    let flags = irq_save();
    unsafe {
        let r = &mut regions()[handle];
        if r.in_use && r.in_flight > 0 {
            r.in_flight -= 1;
            if r.in_flight == 0 && r.access == 0 && r.mapped == 0 {
                release(r);
            }
        }
    }
    irq_restore(flags);
}

/// Grant access to a shared memory region to another task.
/// Must be the creator or have CAP_TASK_MGMT.
pub fn grant(handle: usize, target_tid: usize) -> u64 {
    if handle >= MAX_SHMEM || target_tid >= MAX_TASKS {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let has_mgmt = crate::cap::task_has_task_mgmt(tid, 0);

    let flags = irq_save();
    let result = unsafe {
        let region = &mut regions()[handle];
        if !region.in_use || region.pending_destroy {
            u64::MAX
        } else if region.creator != tid && !has_mgmt {
            // Only creator or CAP_TASK_MGMT holders can grant
            u64::MAX
        } else {
            region.access |= 1u64 << target_tid;
            0
        }
    };
    irq_restore(flags);
    result
}

/// Unmap a shared memory region from the caller's address space.
///
/// Frees the physical pages only if this was the last mapping and the region
/// was already marked for destruction.
pub fn unmap(handle: usize, vaddr: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let cr3 = paging::read_cr3();

    let flags = irq_save();
    let result = unsafe {
        let region = &mut regions()[handle];
        if !region.in_use {
            irq_restore(flags);
            return u64::MAX;
        }
        if region.access & (1u64 << tid) == 0 {
            irq_restore(flags);
            return u64::MAX;
        }
        if !paging::user_range_ok(vaddr, region.page_count) {
            irq_restore(flags);
            return u64::MAX;
        }

        for i in 0..region.page_count {
            // Ignore NotMapped errors — idempotent unmap. Never free the
            // frame: it belongs to the region, not to this address space.
            let _ = paging::unmap_page(cr3, vaddr + i * 4096);
        }
        region.mapped &= !(1u64 << tid);

        if region.pending_destroy && region.mapped == 0 {
            release(region);
        }
        0
    };
    irq_restore(flags);
    result
}

/// Destroy a shared memory region.
///
/// Caller must be the creator or hold CAP_TASK_MGMT. If other tasks still have
/// the region mapped, the frames are not released yet — the region is marked
/// `pending_destroy` and the last task to unmap reclaims it.
pub fn destroy(handle: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let has_mgmt = crate::cap::task_has_task_mgmt(tid, 0);

    let flags = irq_save();
    let result = unsafe {
        let region = &mut regions()[handle];
        if !region.in_use {
            u64::MAX
        } else if region.creator != tid && !has_mgmt {
            u64::MAX
        } else {
            // No further mappings may be created.
            region.pending_destroy = true;
            region.access = 0;
            if region.mapped == 0 {
                release(region);
            }
            0
        }
    };
    irq_restore(flags);
    result
}

/// Clean up shared memory for a dead task.
///
/// The task's address space is being torn down, so drop its mapping bit
/// everywhere, then retire any region it created. A region another live task
/// still has mapped stays alive until that task unmaps it.
pub fn cleanup_task(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        for region in regions().iter_mut() {
            if !region.in_use {
                continue;
            }
            region.mapped &= !(1u64 << tid);
            region.access &= !(1u64 << tid);

            if region.creator == tid {
                region.pending_destroy = true;
                region.access = 0;
            }
            if region.pending_destroy && region.mapped == 0 {
                release(region);
            }
        }
    }
    irq_restore(flags);
}
