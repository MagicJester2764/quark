/// Shared memory regions for zero-copy data sharing between tasks.
///
/// Tasks create a shared region (kernel allocates physical pages),
/// grant access to other tasks, and each maps it into their own address space.

use crate::{paging, pmm, scheduler};

const MAX_SHMEM: usize = 32;
const MAX_PAGES_PER_REGION: usize = 16;

struct ShmemRegion {
    in_use: bool,
    pages: [usize; MAX_PAGES_PER_REGION], // physical addresses
    page_count: usize,
    creator: usize,
    /// Bitmask of TIDs with access (bit N = TID N can map).
    access: u64,
}

impl ShmemRegion {
    const fn empty() -> Self {
        ShmemRegion {
            in_use: false,
            pages: [0; MAX_PAGES_PER_REGION],
            page_count: 0,
            creator: 0,
            access: 0,
        }
    }
}

static mut REGIONS: [ShmemRegion; MAX_SHMEM] = {
    const INIT: ShmemRegion = ShmemRegion::empty();
    [INIT; MAX_SHMEM]
};

/// Create a shared memory region. Returns handle (0..31) or u64::MAX on error.
pub fn create(pages: usize) -> u64 {
    if pages == 0 || pages > MAX_PAGES_PER_REGION {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();

    unsafe {
        // Find a free slot
        let handle = match REGIONS.iter().position(|r| !r.in_use) {
            Some(h) => h,
            None => return u64::MAX,
        };

        let region = &mut REGIONS[handle];

        // Allocate physical pages
        for i in 0..pages {
            match pmm::alloc() {
                Some(frame) => {
                    let phys = frame.address();
                    // Zero the frame (identity-mapped)
                    core::ptr::write_bytes(phys as *mut u8, 0, 4096);
                    region.pages[i] = phys;
                }
                None => {
                    // Free already-allocated pages
                    for j in 0..i {
                        pmm::free(pmm::PhysFrame::from_address(region.pages[j]));
                        region.pages[j] = 0;
                    }
                    return u64::MAX;
                }
            }
        }

        region.in_use = true;
        region.page_count = pages;
        region.creator = tid;
        region.access = 1u64 << tid; // creator has access

        handle as u64
    }
}

/// Map a shared memory region into the caller's address space.
/// vaddr must be page-aligned and in user space.
pub fn map(handle: usize, vaddr: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }
    if vaddr & 0xFFF != 0 || vaddr < 0x80_0000_0000 {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let cr3 = paging::read_cr3();

    unsafe {
        let region = &REGIONS[handle];
        if !region.in_use {
            return u64::MAX;
        }

        // Check access
        if region.access & (1u64 << tid) == 0 {
            return u64::MAX;
        }

        // Check end address is in user space
        match (vaddr as u64).checked_add((region.page_count as u64) * 4096) {
            Some(e) if e <= 0x0000_8000_0000_0000 => {}
            _ => return u64::MAX,
        }

        let flags = paging::PRESENT | paging::WRITABLE | paging::USER;
        for i in 0..region.page_count {
            let v = vaddr + i * 4096;
            if paging::map_page(cr3, v, region.pages[i], flags).is_err() {
                return u64::MAX;
            }
        }

        0
    }
}

/// Grant access to a shared memory region to another task.
/// Must be the creator or have CAP_TASK_MGMT.
pub fn grant(handle: usize, target_tid: usize) -> u64 {
    if handle >= MAX_SHMEM || target_tid >= 64 {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();

    unsafe {
        let region = &mut REGIONS[handle];
        if !region.in_use {
            return u64::MAX;
        }

        // Only creator or CAP_TASK_MGMT holders can grant
        if region.creator != tid
            && !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0)
        {
            return u64::MAX;
        }

        region.access |= 1u64 << target_tid;
        0
    }
}

/// Unmap a shared memory region from the caller's address space.
/// Does NOT free physical pages (other tasks may still have it mapped).
pub fn unmap(handle: usize, vaddr: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }
    if vaddr & 0xFFF != 0 || vaddr < 0x80_0000_0000 {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let cr3 = paging::read_cr3();

    unsafe {
        let region = &REGIONS[handle];
        if !region.in_use {
            return u64::MAX;
        }

        // Check access
        if region.access & (1u64 << tid) == 0 {
            return u64::MAX;
        }

        for i in 0..region.page_count {
            let v = vaddr + i * 4096;
            // Ignore NotMapped errors — idempotent unmap
            let _ = paging::unmap_page(cr3, v);
        }

        0
    }
}

/// Destroy a shared memory region, freeing physical pages and reclaiming the handle.
/// Caller must be the creator or have CAP_TASK_MGMT.
/// Callers should unmap first — destroy does NOT walk other tasks' page tables.
pub fn destroy(handle: usize) -> u64 {
    if handle >= MAX_SHMEM {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();

    unsafe {
        let region = &mut REGIONS[handle];
        if !region.in_use {
            return u64::MAX;
        }

        // Only creator or CAP_TASK_MGMT holders can destroy
        if region.creator != tid
            && !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0)
        {
            return u64::MAX;
        }

        for i in 0..region.page_count {
            pmm::free(pmm::PhysFrame::from_address(region.pages[i]));
        }

        *region = ShmemRegion::empty();
        0
    }
}

/// Clean up shared memory regions owned by a dead task.
/// Frees physical pages and reclaims handles for regions created by `tid`.
pub fn cleanup_task(tid: usize) {
    unsafe {
        for i in 0..MAX_SHMEM {
            let region = &mut REGIONS[i];
            if region.in_use && region.creator == tid {
                for j in 0..region.page_count {
                    pmm::free(pmm::PhysFrame::from_address(region.pages[j]));
                }
                *region = ShmemRegion::empty();
            }
        }
    }
}
