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
use crate::grow::Grow;
use crate::task::MAX_TASKS;

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

/// Tasks, as a set: their ids, in a small array that grows (`grow.rs`).
/// Who may map a region and who has it mapped were bits of a word, one for
/// each task: as many tasks as a word has bits.
struct Tasks {
    ids: Grow<u16>,
    len: usize,
}

impl Tasks {
    const fn new() -> Self {
        Tasks { ids: Grow::new(0), len: 0 }
    }

    fn at(&self, tid: usize) -> Option<usize> {
        (0..self.len).find(|&i| self.ids.get(i).is_some_and(|&t| t as usize == tid))
    }

    fn contains(&self, tid: usize) -> bool {
        self.at(tid).is_some()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `tid` is one of them. False if there was no room for it.
    fn add(&mut self, tid: usize) -> bool {
        if self.contains(tid) {
            return true;
        }
        match self.ids.ensure(self.len, 4, MAX_TASKS) {
            Ok(slot) => {
                *slot = tid as u16;
                self.len += 1;
                true
            }
            Err(_) => false,
        }
    }

    fn remove(&mut self, tid: usize) {
        if let Some(i) = self.at(tid) {
            self.len -= 1;
            let last = self.ids.get(self.len).copied().unwrap_or(0);
            if let Some(slot) = self.ids.get_mut(i) {
                *slot = last;
            }
        }
    }

    /// `with` in `tid`'s place: no room is needed for it.
    fn replace(&mut self, tid: usize, with: usize) {
        if self.contains(with) {
            self.remove(tid);
        } else if let Some(slot) = self.at(tid).and_then(|i| self.ids.get_mut(i)) {
            *slot = with as u16;
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }
}

struct ShmemRegion {
    /// The contiguous runs this region is assembled from, in order.
    runs: [Run; MAX_RUNS],
    run_count: usize,
    page_count: usize,
    creator: usize,
    /// The tasks that may map it, for the older interface that names a
    /// region by its handle. A region made as a descriptor has none of these:
    /// holding the descriptor is the permission.
    access: Tasks,
    /// Descriptors that name this region, in any program's table or on their
    /// way down a stream.
    ///
    /// One count for all of them, because a descriptor belongs to a table and
    /// not to a task. It was a count per task, beside a count of its own for
    /// descriptors in flight — which belong to no task — and the first of
    /// those could not survive a table two threads share: neither of them is
    /// "the" holder, and whichever died first took the region with it.
    fd_refs: u32,
    /// Made by `SYS_MEMFD_CREATE`: it lives as long as a descriptor names it
    /// or somebody has it mapped, whoever made it. A region made by handle
    /// goes with the task that made it, as it always has.
    by_fd: bool,
    /// The tasks that have the region mapped.
    mapped: Tasks,
    /// Set when destroy was requested while the region was still mapped.
    /// The last task to unmap frees the frames and releases the handle.
    pending_destroy: bool,
}

impl ShmemRegion {
    const fn empty() -> Self {
        ShmemRegion {
            runs: [Run { base: 0, pages: 0 }; MAX_RUNS],
            run_count: 0,
            page_count: 0,
            creator: 0,
            access: Tasks::new(),
            fd_refs: 0,
            by_fd: false,
            mapped: Tasks::new(),
            pending_destroy: false,
        }
    }
}

/// Every region, by its handle: made when one is created, given back when
/// its frames are (`table.rs`). Linux's System V limit is 4096 and its
/// POSIX shared memory has no count at all; this has the machine's memory.
/// There were 256, a fixed array of them spent whether used or not.
static mut REGIONS: crate::table::Table<ShmemRegion> = crate::table::Table::new(crate::table::MOST);

/// The shared regions' lock (`sync::RANK_SHMEM`): the table and every region
/// in it.
static LOCK: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(crate::sync::RANK_SHMEM, "the shared regions", ());



/// Borrow the region table. Callers hold [`LOCK`].
#[inline(always)]
unsafe fn regions() -> &'static mut crate::table::Table<ShmemRegion> { unsafe {
    &mut *core::ptr::addr_of_mut!(REGIONS)
}}

/// Region `handle`, if there is one.
///
/// # Safety
/// [`LOCK`] held.
unsafe fn region(handle: usize) -> Option<&'static mut ShmemRegion> {
    unsafe { regions().get(handle) }
}

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

/// Release a region's frames, and the region. Interrupts must be off.
unsafe fn release(handle: usize) {
    unsafe {
        let Some(region) = region(handle) else { return };
        let creator = region.creator;
        let freed = release_frames(region);
        // Refund the creator's quota.
        scheduler::uncharge_task_mem(creator, freed);
        regions().empty(handle);
    }
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

/// Create a shared memory region named by its handle, which its creator may
/// map and may grant to others. Returns the handle, or u64::MAX.
pub fn create(pages: usize) -> u64 {
    create_inner(pages, false)
}

/// Create a region for a descriptor to name. It starts with one reference —
/// the descriptor the caller is about to be given — and nobody may map it
/// except through a descriptor.
pub fn create_fd(pages: usize) -> u64 {
    create_inner(pages, true)
}

fn create_inner(pages: usize, by_fd: bool) -> u64 {
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

    if !crate::reclaim::may_make() {
        return u64::MAX;
    }

    // Claim the slot before allocating. pmm::alloc takes a lock that re-enables
    // interrupts on release, so a preempting task used to be able to pick the
    // same "free" handle and scribble over this region.
    let held = LOCK.lock();
    let handle = unsafe { regions().lowest_free(0).filter(|&h| regions().fill_at(h, ShmemRegion::empty()).is_ok()) };
    let Some(handle) = handle else {
        drop(held);
        return u64::MAX;
    };
    unsafe {
        let Some(region) = region(handle) else {
            drop(held);
            return u64::MAX;
        };
        region.creator = tid;
        region.by_fd = by_fd;
        if by_fd {
            region.fd_refs = 1;
        } else if !region.access.add(tid) {
            regions().empty(handle);
            drop(held);
            return u64::MAX;
        }
        region.page_count = pages;
    }
    drop(held);

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
        match pmm::alloc_contiguous(ask, false) {
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
        let held = LOCK.lock();
        unsafe {
            if let Some(region) = region(handle) {
                region.runs = runs;
                region.run_count = run_count;
            }
            release(handle);
        }
        drop(held);
        return false;
    }
    // Zero every run (identity-mapped) so nothing leaks from a previous owner.
    for r in &runs[..run_count] {
        unsafe { core::ptr::write_bytes(r.base as *mut u8, 0, r.pages * 4096) };
    }
    let held = LOCK.lock();
    unsafe {
        if let Some(region) = region(handle) {
            region.runs = runs;
            region.run_count = run_count;
            region.page_count = pages;
        }
    }
    drop(held);
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
    if pages == 0 || pages > MAX_PAGES_PER_REGION {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }

    let held = LOCK.lock();
    let old_pages = unsafe {
        let Some(region) = region(handle) else {
            drop(held);
            return u64::MAX;
        };
        // One descriptor names it — the caller's, which the system call
        // checked — and nothing has it mapped. A second descriptor is one that
        // has been somewhere, or is on its way.
        if region.pending_destroy
            || !region.mapped.is_empty()
            || !region.by_fd
            || region.fd_refs != 1
        {
            drop(held);
            return u64::MAX;
        }
        if region.page_count == pages {
            drop(held);
            return pages as u64;
        }
        let old = region.page_count;
        // Let go of the old frames before asking for new ones: on a machine
        // with just enough memory, holding both is the difference between a
        // resize that works and one that does not.
        release_frames(region);
        region.page_count = pages;
        // Whoever resizes it pays for it from here on: the task that made it
        // may be a thread that has since gone.
        let was = region.creator;
        region.creator = tid;
        (old, was)
    };
    drop(held);
    let (old_pages, was) = old_pages;
    scheduler::uncharge_task_mem(was, old_pages);

    if !scheduler::current_task_check_mem(pages) {
        // Put it back the way it was found, so a refused resize does not also
        // destroy the memory the caller already had.
        let held = LOCK.lock();
        unsafe {
            if let Some(region) = region(handle) {
                region.page_count = 0;
            }
        }
        drop(held);
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
    map_inner(handle, vaddr, false)
}

/// Map a region the caller holds a descriptor for. The descriptor is the
/// permission, and the system call has already found it in the caller's table.
pub fn map_held(handle: usize, vaddr: usize) -> u64 {
    map_inner(handle, vaddr, true)
}

fn map_inner(handle: usize, vaddr: usize, held: bool) -> u64 {
    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let cr3 = paging::read_cr3();

    let lock = LOCK.lock();
    let result = unsafe {
        let Some(region) = region(handle).filter(|r| !r.pending_destroy) else {
            drop(lock);
            return u64::MAX;
        };

        // Check access, and that there is room to say it is mapped.
        if !held && !region.access.contains(tid) {
            drop(lock);
            return u64::MAX;
        }
        let was = region.mapped.contains(tid);
        if !region.mapped.add(tid) {
            drop(lock);
            return u64::MAX;
        }

        let page_count = region.page_count;
        if !paging::user_range_ok(vaddr, page_count) {
            drop(lock);
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
                    if !was {
                        region.mapped.remove(tid);
                    }
                    drop(lock);
                    return u64::MAX;
                }
            };
            if paging::map_page(cr3, v, phys, pte_flags).is_err() {
                for j in 0..i {
                    let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                }
                if !was {
                    region.mapped.remove(tid);
                }
                drop(lock);
                return u64::MAX;
            }
        }
        page_count as u64
    };
    drop(lock);
    result
}

/// Nothing can reach the region any more: no task may map it by handle and
/// no descriptor names it. Interrupts must be off.
fn unreachable(r: &ShmemRegion) -> bool {
    r.access.is_empty() && r.fd_refs == 0
}

/// One more descriptor names this region.
pub fn fd_retain(handle: usize) -> bool {
    let held = LOCK.lock();
    let ok = unsafe {
        match region(handle) {
            Some(r) if !r.pending_destroy => {
                r.fd_refs += 1;
                true
            }
            _ => false,
        }
    };
    drop(held);
    ok
}

/// One fewer. The last one frees the region, unless somebody has it mapped:
/// closing a descriptor governs the right to map, not mappings that already
/// exist, so the frames then go when the last mapper unmaps.
pub fn fd_release(handle: usize) {
    let held = LOCK.lock();
    unsafe {
        if let Some(r) = region(handle).filter(|r| r.fd_refs > 0) {
            r.fd_refs -= 1;
            if unreachable(r) {
                if r.mapped.is_empty() {
                    release(handle);
                } else {
                    r.pending_destroy = true;
                }
            }
        }
    }
    drop(held);
}

/// Grant access to a shared memory region to another task.
/// Must be the creator or have CAP_TASK_MGMT.
pub fn grant(handle: usize, target_tid: usize) -> u64 {
    if target_tid >= MAX_TASKS {
        return u64::MAX;
    }

    let tid = scheduler::current_tid();
    let has_mgmt = crate::cap::task_has_task_mgmt(tid, 0);

    let held = LOCK.lock();
    let result = unsafe {
        let Some(region) = region(handle) else {
            drop(held);
            return u64::MAX;
        };
        if region.pending_destroy || region.by_fd {
            // A descriptor's region is reached by holding a descriptor, and
            // handed on by handing one on.
            u64::MAX
        } else if region.creator != tid && !has_mgmt {
            // Only creator or CAP_TASK_MGMT holders can grant
            u64::MAX
        } else if region.access.add(target_tid) {
            0
        } else {
            u64::MAX
        }
    };
    drop(held);
    result
}

/// Unmap a shared memory region from the caller's address space.
///
/// Frees the physical pages only if this was the last mapping and the region
/// was already marked for destruction.
pub fn unmap(handle: usize, vaddr: usize) -> u64 {
    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let cr3 = paging::read_cr3();

    let held = LOCK.lock();
    let result = unsafe {
        let Some(region) = region(handle) else {
            drop(held);
            return u64::MAX;
        };
        // Whoever may map it may unmap it, and so may whoever has it mapped:
        // the right to map can have gone since.
        if !region.access.contains(tid) && !region.mapped.contains(tid) {
            drop(held);
            return u64::MAX;
        }
        if !paging::user_range_ok(vaddr, region.page_count) {
            drop(held);
            return u64::MAX;
        }
        // Not under a call another thread of the program is in and has
        // checked, which may be waiting to copy to it (`SYS_MUNMAP`).
        let end = (vaddr + region.page_count * 4096) as u64;
        if scheduler::pinned_by_another(crate::userspace::space_of(cr3), vaddr as u64, end) {
            drop(held);
            return u64::MAX;
        }

        for i in 0..region.page_count {
            // Ignore NotMapped errors — idempotent unmap. Never free the
            // frame: it belongs to the region, not to this address space.
            let _ = paging::unmap_page(cr3, vaddr + i * 4096);
        }
        region.mapped.remove(tid);

        if region.pending_destroy && region.mapped.is_empty() {
            release(handle);
        }
        0
    };
    drop(held);
    result
}

/// Destroy a shared memory region.
///
/// Caller must be the creator or hold CAP_TASK_MGMT. If other tasks still have
/// the region mapped, the frames are not released yet — the region is marked
/// `pending_destroy` and the last task to unmap reclaims it.
pub fn destroy(handle: usize) -> u64 {
    let tid = scheduler::current_tid();
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    let has_mgmt = crate::cap::task_has_task_mgmt(tid, 0);

    let held = LOCK.lock();
    let result = unsafe {
        match region(handle) {
            None => u64::MAX,
            Some(region) if region.creator != tid && !has_mgmt => u64::MAX,
            // A descriptor's region ends when its descriptors do.
            Some(region) if region.by_fd => u64::MAX,
            Some(region) => {
                // No further mappings may be created.
                region.pending_destroy = true;
                region.access.clear();
                if region.mapped.is_empty() {
                    release(handle);
                }
                0
            }
        }
    };
    drop(held);
    result
}

/// Clean up shared memory for a dead task.
///
/// A mapping belongs to an address space, and is recorded against the task
/// that made it. If that task was a thread and its program lives on, so does
/// the mapping: it is handed to `survivor`, a task still running there.
/// Dropping it instead let the region be freed while a live address space
/// still mapped its frames.
///
/// With nobody left in the address space, the task's mapping bit goes
/// everywhere, and any region it made by handle is retired. A region another
/// live task still has mapped stays until that task unmaps it.
pub fn cleanup_task(tid: usize, survivor: Option<usize>) {
    if tid >= MAX_TASKS {
        return;
    }
    let held = LOCK.lock();
    unsafe {
        let mut at = 0;
        while let Some(handle) = regions().next_used(at) {
            at = handle + 1;
            let Some(region) = region(handle) else { continue };
            match survivor {
                Some(s) => region.mapped.replace(tid, s),
                None => region.mapped.remove(tid),
            }
            region.access.remove(tid);

            if region.creator == tid {
                if !region.by_fd {
                    region.pending_destroy = true;
                    region.access.clear();
                }
                // The quota it was charged to has gone with the task. A
                // program that still uses the region carries it from here;
                // with no program left there is nobody to refund, and the
                // number must not be left to name whoever takes it next.
                region.creator = survivor.unwrap_or(usize::MAX);
            }
            if unreachable(region) {
                region.pending_destroy = true;
            }
            if region.pending_destroy && region.mapped.is_empty() {
                release(handle);
            }
        }
    }
    drop(held);
}
