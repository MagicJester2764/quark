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

const MAX_SHMEM: usize = 32;
const MAX_PAGES_PER_REGION: usize = 16;

/// `access`/`mapped` are TID bitmasks, one bit per task.
const _: () = assert!(MAX_TASKS <= u64::BITS as usize);

struct ShmemRegion {
    in_use: bool,
    pages: [usize; MAX_PAGES_PER_REGION], // physical addresses
    page_count: usize,
    creator: usize,
    /// Bitmask of TIDs with access (bit N = TID N can map).
    access: u64,
    /// Bitmask of TIDs that currently have the region mapped.
    mapped: u64,
    /// Set when destroy was requested while the region was still mapped.
    /// The last task to unmap frees the frames and releases the handle.
    pending_destroy: bool,
}

impl ShmemRegion {
    const fn empty() -> Self {
        ShmemRegion {
            in_use: false,
            pages: [0; MAX_PAGES_PER_REGION],
            page_count: 0,
            creator: 0,
            access: 0,
            mapped: 0,
            pending_destroy: false,
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
unsafe fn regions() -> &'static mut [ShmemRegion; MAX_SHMEM] {
    &mut *core::ptr::addr_of_mut!(REGIONS)
}

/// Release a region's frames and reset the slot. Interrupts must be off.
unsafe fn release(region: &mut ShmemRegion) {
    let mut freed = 0;
    for j in 0..region.page_count {
        if region.pages[j] != 0 {
            pmm::free(pmm::PhysFrame::from_address(region.pages[j]));
            freed += 1;
        }
    }
    // Refund the creator's quota.
    scheduler::uncharge_task_mem(region.creator, freed);
    *region = ShmemRegion::empty();
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

    // Allocate backing frames. On failure, roll the whole region back.
    for i in 0..pages {
        match pmm::alloc() {
            Some(frame) => {
                let phys = frame.address();
                // Zero the frame (identity-mapped) so it cannot leak whatever
                // the previous owner left behind.
                unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096) };
                let flags = irq_save();
                unsafe { regions()[handle].pages[i] = phys };
                irq_restore(flags);
            }
            None => {
                let flags = irq_save();
                unsafe { release(&mut regions()[handle]) };
                irq_restore(flags);
                return u64::MAX;
            }
        }
    }

    scheduler::current_task_charge_mem(pages);
    handle as u64
}

/// Map a shared memory region into the caller's address space.
/// vaddr must be page-aligned and in user space.
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
            if paging::map_page(cr3, v, region.pages[i], pte_flags).is_err() {
                for j in 0..i {
                    let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                }
                irq_restore(flags);
                return u64::MAX;
            }
        }
        region.mapped |= 1u64 << tid;
        0
    };
    irq_restore(flags);
    result
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
