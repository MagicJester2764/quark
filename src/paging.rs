/// x86-64 4-level page table management.
///
/// Provides types for page table entries and tables, plus functions to
/// map/unmap 4 KiB virtual pages. Relies on identity mapping (phys == virt)
/// to access page table memory directly.

use crate::pmm;
use core::arch::asm;

// Page table entry flags
pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
pub const WRITE_THROUGH: u64 = 1 << 3;
pub const CACHE_DISABLE: u64 = 1 << 4;
pub const ACCESSED: u64 = 1 << 5;
pub const DIRTY: u64 = 1 << 6;
pub const HUGE_PAGE: u64 = 1 << 7;
pub const GLOBAL: u64 = 1 << 8;
pub const NO_EXECUTE: u64 = 1 << 63;

/// PTE available bit 9: this address space *owns* the mapped frame and is
/// responsible for returning it to the PMM when the page is unmapped or the
/// address space is destroyed.
///
/// Set for anonymous memory (`sys_mmap`, ELF segments, stacks, boot info).
/// Deliberately NOT set for device MMIO (`sys_map_phys`), shared memory
/// (`shmem::map`), or frames supplied by another task (`sys_addrspace_map`) —
/// freeing those would hand device addresses or still-shared frames back to
/// the frame allocator.
pub const OWNED: u64 = 1 << 9;

/// Highest canonical user address (exclusive). Everything at or above this is
/// kernel/non-canonical and must never be mapped on behalf of user space.
pub const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Lowest virtual address a user address space may map into.
///
/// `userspace::create_address_space` deep-copies only PML4[0]'s PDPT; the page
/// directories and tables below it stay shared with the kernel. Mapping under
/// PML4[0] would therefore write PTEs into tables every address space shares
/// and promote the intermediate entries to USER, exposing the kernel identity
/// map to ring 3 system-wide. PML4[1] starts at 512 GiB.
pub const USER_MIN_ADDR: u64 = 0x80_0000_0000;

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const PAGE_SIZE: usize = 4096;

/// Validate that `[virt, virt + pages * 4096)` is a legal range for a user
/// mapping: page-aligned, non-empty, no overflow, and entirely inside the
/// per-address-space user window.
pub fn user_range_ok(virt: usize, pages: usize) -> bool {
    if pages == 0 || virt & 0xFFF != 0 {
        return false;
    }
    let len = match (pages as u64).checked_mul(PAGE_SIZE as u64) {
        Some(l) => l,
        None => return false,
    };
    let end = match (virt as u64).checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    virt as u64 >= USER_MIN_ADDR && end <= USER_ADDR_LIMIT
}

#[derive(Debug)]
pub enum PagingError {
    /// Encountered a 2 MiB huge page — must be split before 4 KiB mapping.
    HugePageConflict,
    /// Physical frame allocator is out of memory.
    OutOfFrames,
    /// Page is not mapped.
    NotMapped,
}

/// A single page table entry (PTE/PDE/PDPE/PML4E).
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        PageTableEntry(0)
    }

    pub fn is_present(&self) -> bool {
        self.0 & PRESENT != 0
    }

    pub fn is_huge(&self) -> bool {
        self.0 & HUGE_PAGE != 0
    }

    pub fn frame_address(&self) -> usize {
        (self.0 & ADDR_MASK) as usize
    }

    pub fn set(&mut self, addr: usize, flags: u64) {
        self.0 = (addr as u64 & ADDR_MASK) | flags;
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn raw(&self) -> u64 {
        self.0
    }

    /// Return only the flag bits (low 12 bits + NX), excluding the address.
    pub fn flags(&self) -> u64 {
        self.0 & !ADDR_MASK
    }
}

/// A page table: 512 entries, 4 KiB aligned.
#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

impl PageTable {
    /// Zero out all entries.
    pub fn clear(&mut self) {
        for e in self.entries.iter_mut() {
            e.clear();
        }
    }
}

static mut KERNEL_CR3: usize = 0;

/// Save the kernel's CR3 during boot. Must be called before creating
/// any user address spaces so `create_address_space` can always copy
/// from the kernel's clean page tables.
pub fn save_kernel_cr3() {
    unsafe { KERNEL_CR3 = read_cr3(); }
}

/// Return the kernel's CR3 (saved during boot).
pub fn kernel_cr3() -> usize {
    unsafe { KERNEL_CR3 }
}

/// Read the current CR3 value (physical address of PML4).
pub fn read_cr3() -> usize {
    let val: u64;
    unsafe {
        asm!("mov {}, cr3", out(reg) val, options(nomem, nostack));
    }
    val as usize
}

/// Load a new PML4 physical address into CR3.
///
/// # Safety
/// The address must point to a valid, identity-mapped PML4 table.
pub unsafe fn write_cr3(addr: usize) {
    asm!("mov cr3, {}", in(reg) addr as u64, options(nomem, nostack));
}

/// Invalidate the TLB entry for a virtual address.
pub fn invlpg(vaddr: usize) {
    unsafe {
        asm!("invlpg [{}]", in(reg) vaddr, options(nostack));
    }
}

/// Extract page table indices from a virtual address.
fn table_indices(vaddr: usize) -> (usize, usize, usize, usize) {
    let pml4_idx = (vaddr >> 39) & 0x1FF;
    let pdpt_idx = (vaddr >> 30) & 0x1FF;
    let pd_idx = (vaddr >> 21) & 0x1FF;
    let pt_idx = (vaddr >> 12) & 0x1FF;
    (pml4_idx, pdpt_idx, pd_idx, pt_idx)
}

/// Access a page table at a physical address via identity mapping.
///
/// # Safety
/// The address must be identity-mapped and point to a valid PageTable.
pub unsafe fn table_at(phys: usize) -> &'static mut PageTable {
    &mut *(phys as *mut PageTable)
}

/// Allocate a new zeroed page table from the PMM.
fn alloc_table() -> Result<usize, PagingError> {
    let frame = pmm::alloc().ok_or(PagingError::OutOfFrames)?;
    let addr = frame.address();
    // Zero the new table (identity-mapped, so we can write directly)
    unsafe {
        core::ptr::write_bytes(addr as *mut u8, 0, PAGE_SIZE);
    }
    Ok(addr)
}

/// Map a 4 KiB virtual page to a physical frame.
///
/// Walks the 4-level page table hierarchy, allocating intermediate tables
/// as needed. If a 2 MiB huge page is encountered at the PD level, it is
/// split into 512 individual 4 KiB pages preserving the original mapping.
/// 1 GiB huge pages at the PDPT level are not split (returns error).
///
/// When the requested flags include USER, intermediate entries are promoted
/// to include USER so user-mode page walks succeed.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn map_page(
    pml4_phys: usize,
    virt_addr: usize,
    phys_addr: usize,
    flags: u64,
) -> Result<(), PagingError> {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt_addr);
    let user = flags & USER;

    // Level 4: PML4
    let pml4 = table_at(pml4_phys);
    if !pml4.entries[pml4i].is_present() {
        let new_table = alloc_table()?;
        pml4.entries[pml4i].set(new_table, PRESENT | WRITABLE | user);
    } else if user != 0 && pml4.entries[pml4i].raw() & USER == 0 {
        pml4.entries[pml4i] = PageTableEntry(pml4.entries[pml4i].raw() | USER);
    }

    // Level 3: PDPT
    let pdpt_phys = pml4.entries[pml4i].frame_address();
    let pdpt = table_at(pdpt_phys);
    if pdpt.entries[pdpti].is_present() && pdpt.entries[pdpti].is_huge() {
        return Err(PagingError::HugePageConflict);
    }
    if !pdpt.entries[pdpti].is_present() {
        let new_table = alloc_table()?;
        pdpt.entries[pdpti].set(new_table, PRESENT | WRITABLE | user);
    } else if user != 0 && pdpt.entries[pdpti].raw() & USER == 0 {
        pdpt.entries[pdpti] = PageTableEntry(pdpt.entries[pdpti].raw() | USER);
    }

    // Level 2: PD
    let pd_phys = pdpt.entries[pdpti].frame_address();
    let pd = table_at(pd_phys);
    if pd.entries[pdi].is_present() && pd.entries[pdi].is_huge() {
        // Split 2 MiB huge page into 512 × 4 KiB pages preserving the mapping
        let huge_phys = pd.entries[pdi].frame_address();
        let huge_flags = pd.entries[pdi].raw() & !ADDR_MASK & !HUGE_PAGE;
        let new_pt = alloc_table()?;
        let pt = table_at(new_pt);
        for j in 0..512 {
            pt.entries[j].set(huge_phys + j * PAGE_SIZE, huge_flags);
        }
        pd.entries[pdi].set(new_pt, PRESENT | WRITABLE | user);
    }
    if !pd.entries[pdi].is_present() {
        let new_table = alloc_table()?;
        pd.entries[pdi].set(new_table, PRESENT | WRITABLE | user);
    } else if user != 0 && pd.entries[pdi].raw() & USER == 0 {
        pd.entries[pdi] = PageTableEntry(pd.entries[pdi].raw() | USER);
    }

    // Level 1: PT
    let pt_phys = pd.entries[pdi].frame_address();
    let pt = table_at(pt_phys);

    // Replacing a live mapping: if this address space owned the old frame and
    // we are pointing the PTE somewhere else, return the old frame to the PMM
    // rather than leaking it.
    let old = pt.entries[pti];
    if old.is_present() && old.raw() & OWNED != 0 && old.frame_address() != phys_addr {
        pmm::free(pmm::PhysFrame::from_address(old.frame_address()));
    }

    pt.entries[pti].set(phys_addr, flags);

    invlpg(virt_addr);

    Ok(())
}

/// Look up the leaf PTE flags for `virt` in the address space rooted at
/// `pml4_phys`. Returns `None` if any level along the walk is absent.
///
/// Huge pages are reported with their own flags; the caller only cares about
/// PRESENT/USER/WRITABLE, which are meaningful at every level.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn walk_flags(pml4_phys: usize, virt: usize) -> Option<u64> {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);

    // A page is user-accessible only if USER is set at *every* level of the
    // walk, and writable only if WRITABLE is set at every level. Track both
    // as running ANDs rather than trusting the leaf entry alone.
    let mut user = true;
    let mut writable = true;

    macro_rules! descend {
        ($entry:expr) => {{
            let e = $entry;
            if !e.is_present() {
                return None;
            }
            user &= e.raw() & USER != 0;
            writable &= e.raw() & WRITABLE != 0;
            e
        }};
    }

    let e = descend!(table_at(pml4_phys).entries[pml4i]);

    let e = descend!(table_at(e.frame_address()).entries[pdpti]);
    if e.is_huge() {
        return Some(synth_flags(user, writable));
    }

    let e = descend!(table_at(e.frame_address()).entries[pdi]);
    if e.is_huge() {
        return Some(synth_flags(user, writable));
    }

    let _ = descend!(table_at(e.frame_address()).entries[pti]);
    Some(synth_flags(user, writable))
}

/// Build a flags word carrying just the effective PRESENT/USER/WRITABLE bits
/// produced by a page-table walk.
fn synth_flags(user: bool, writable: bool) -> u64 {
    PRESENT
        | if user { USER } else { 0 }
        | if writable { WRITABLE } else { 0 }
}

/// Resolve `virt` to its backing physical address in `pml4_phys`.
///
/// Returns `None` if the page is not mapped. Used to key futexes on physical
/// memory so a futex word inside a shared-memory region is the same object to
/// every task that maps it.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn translate(pml4_phys: usize, virt: usize) -> Option<usize> {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);

    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() {
        return None;
    }
    if e.is_huge() {
        return Some(e.frame_address() + (virt & 0x3FFF_FFFF));
    }
    let e = table_at(e.frame_address()).entries[pdi];
    if !e.is_present() {
        return None;
    }
    if e.is_huge() {
        return Some(e.frame_address() + (virt & 0x1F_FFFF));
    }
    let e = table_at(e.frame_address()).entries[pti];
    if !e.is_present() {
        return None;
    }
    Some(e.frame_address() + (virt & 0xFFF))
}

/// Check that every page of `[addr, addr + len)` is currently mapped, present,
/// and user-accessible in the address space rooted at `pml4_phys` — and
/// writable too when `write` is set.
///
/// The kernel dereferences user pointers directly (it runs on the faulting
/// task's CR3), so without this a bad pointer faults *inside* the kernel,
/// often with a lock held and interrupts disabled. Range-checking the address
/// alone is not enough; the pages have to actually be there.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn user_range_accessible(
    pml4_phys: usize,
    addr: u64,
    len: u64,
    write: bool,
) -> bool {
    if len == 0 {
        return true;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if end > USER_ADDR_LIMIT {
        return false;
    }

    let first = addr & !0xFFF;
    let last = (end - 1) & !0xFFF;
    let mut page = first;
    loop {
        let flags = match walk_flags(pml4_phys, page as usize) {
            Some(f) => f,
            None => return false,
        };
        if flags & USER == 0 {
            return false;
        }
        if write && flags & WRITABLE == 0 {
            return false;
        }
        if page == last {
            return true;
        }
        page += PAGE_SIZE as u64;
    }
}

/// Destroy a user address space, freeing all user page tables and mapped frames.
///
/// - PML4[0]: Free the deep-copied PDPT frame only (PD children are kernel-shared).
/// - PML4[1..255]: Recursively free entire subtree (user-only).
/// - PML4[256..511]: Skip (kernel upper-half, shared).
/// - Free the PML4 frame itself.
///
/// # Safety
/// `pml4_phys` must be a valid user address space (not the kernel CR3).
pub unsafe fn destroy_address_space(pml4_phys: usize) {
    if pml4_phys == 0 || pml4_phys == kernel_cr3() {
        return;
    }

    let pml4 = table_at(pml4_phys);

    // PML4[0]: free only the deep-copied PDPT frame, not its children
    if pml4.entries[0].is_present() {
        let pdpt_phys = pml4.entries[0].frame_address();
        pmm::free(pmm::PhysFrame::from_address(pdpt_phys));
    }

    // PML4[1..255]: user-space entries — free entire subtrees
    for i in 1..256 {
        if pml4.entries[i].is_present() {
            let pdpt_phys = pml4.entries[i].frame_address();
            free_pdpt_tree(pdpt_phys);
        }
    }

    // PML4[256..511]: kernel upper-half — skip

    // Free the PML4 frame itself
    pmm::free(pmm::PhysFrame::from_address(pml4_phys));
}

unsafe fn free_pdpt_tree(pdpt_phys: usize) {
    let pdpt = table_at(pdpt_phys);
    for i in 0..512 {
        if pdpt.entries[i].is_present() && !pdpt.entries[i].is_huge() {
            let pd_phys = pdpt.entries[i].frame_address();
            free_pd_tree(pd_phys);
        }
    }
    pmm::free(pmm::PhysFrame::from_address(pdpt_phys));
}

unsafe fn free_pd_tree(pd_phys: usize) {
    let pd = table_at(pd_phys);
    for i in 0..512 {
        if pd.entries[i].is_present() {
            if pd.entries[i].is_huge() {
                // 2M huge page leaf — nothing to recurse, but don't free
                // (these shouldn't appear in user space normally)
                continue;
            }
            let pt_phys = pd.entries[i].frame_address();
            free_pt_leaves(pt_phys);
        }
    }
    pmm::free(pmm::PhysFrame::from_address(pd_phys));
}

unsafe fn free_pt_leaves(pt_phys: usize) {
    let pt = table_at(pt_phys);
    for i in 0..512 {
        // Only return frames this address space owns. Device MMIO mapped via
        // sys_map_phys, shared-memory pages, and frames handed over by another
        // task are all mapped without OWNED — freeing them would push device
        // addresses into the frame allocator or double-free shared pages.
        if pt.entries[i].is_present() && pt.entries[i].raw() & OWNED != 0 {
            let frame_phys = pt.entries[i].frame_address();
            pmm::free(pmm::PhysFrame::from_address(frame_phys));
        }
    }
    pmm::free(pmm::PhysFrame::from_address(pt_phys));
}

/// Unmap a 4 KiB virtual page. Returns `(frame_address, pte_flags)`.
///
/// The caller is responsible for freeing the returned frame if desired — and
/// must only do so when `flags & OWNED != 0`, otherwise it would be handing
/// the PMM a device address or a still-shared page. See [`unmap_page_owned`]
/// for the common case.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn unmap_page(
    pml4_phys: usize,
    virt_addr: usize,
) -> Result<(usize, u64), PagingError> {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt_addr);

    let pml4 = table_at(pml4_phys);
    if !pml4.entries[pml4i].is_present() {
        return Err(PagingError::NotMapped);
    }

    let pdpt = table_at(pml4.entries[pml4i].frame_address());
    if !pdpt.entries[pdpti].is_present() || pdpt.entries[pdpti].is_huge() {
        return Err(PagingError::NotMapped);
    }

    let pd = table_at(pdpt.entries[pdpti].frame_address());
    if !pd.entries[pdi].is_present() || pd.entries[pdi].is_huge() {
        return Err(PagingError::NotMapped);
    }

    let pt = table_at(pd.entries[pdi].frame_address());
    if !pt.entries[pti].is_present() {
        return Err(PagingError::NotMapped);
    }

    let frame_addr = pt.entries[pti].frame_address();
    let flags = pt.entries[pti].flags();
    pt.entries[pti].clear();

    invlpg(virt_addr);

    // Release page tables that just became empty. Restricted to the per-address
    // -space user window: the tables under PML4[0] are shared with the kernel
    // and must never be freed here.
    if (virt_addr as u64) >= USER_MIN_ADDR {
        reclaim_empty_tables(pml4_phys, virt_addr);
    }

    Ok((frame_addr, flags))
}

/// True if every entry in the table at `phys` is absent.
unsafe fn table_is_empty(phys: usize) -> bool {
    table_at(phys).entries.iter().all(|e| !e.is_present())
}

/// Walk back up from a just-cleared PTE, freeing each level that is now empty.
unsafe fn reclaim_empty_tables(pml4_phys: usize, virt_addr: usize) {
    let (pml4i, pdpti, pdi, _) = table_indices(virt_addr);

    let pml4 = table_at(pml4_phys);
    if !pml4.entries[pml4i].is_present() {
        return;
    }
    let pdpt_phys = pml4.entries[pml4i].frame_address();
    let pdpt = table_at(pdpt_phys);
    if !pdpt.entries[pdpti].is_present() || pdpt.entries[pdpti].is_huge() {
        return;
    }
    let pd_phys = pdpt.entries[pdpti].frame_address();
    let pd = table_at(pd_phys);
    if !pd.entries[pdi].is_present() || pd.entries[pdi].is_huge() {
        return;
    }
    let pt_phys = pd.entries[pdi].frame_address();

    if !table_is_empty(pt_phys) {
        return;
    }
    pd.entries[pdi].clear();
    pmm::free(pmm::PhysFrame::from_address(pt_phys));

    if !table_is_empty(pd_phys) {
        return;
    }
    pdpt.entries[pdpti].clear();
    pmm::free(pmm::PhysFrame::from_address(pd_phys));

    if !table_is_empty(pdpt_phys) {
        return;
    }
    pml4.entries[pml4i].clear();
    pmm::free(pmm::PhysFrame::from_address(pdpt_phys));
}

/// Unmap a 4 KiB page and return its frame to the PMM, but only if this
/// address space owned it. Returns `true` if a frame was actually freed.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn unmap_page_owned(pml4_phys: usize, virt_addr: usize) -> bool {
    match unmap_page(pml4_phys, virt_addr) {
        Ok((frame_addr, flags)) if flags & OWNED != 0 => {
            pmm::free(pmm::PhysFrame::from_address(frame_addr));
            true
        }
        _ => false,
    }
}
