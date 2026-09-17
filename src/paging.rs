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
/// Set for anonymous memory (`sys_mmap`, ELF segments, stacks, boot info), and
/// for pages another task moved here with `sys_addrspace_give`, which leave
/// the giver as they arrive. Deliberately NOT set for device MMIO
/// (`sys_map_phys`), shared memory (`shmem::map`), or frames another task
/// still holds (`sys_addrspace_map`) — freeing those would hand device
/// addresses or still-shared frames back to the frame allocator.
///
/// An owned frame is mapped in exactly one place. Everything that sets this
/// bit keeps that true, and it is what makes freeing on unmap safe.
pub const OWNED: u64 = 1 << 9;

/// A reservation: a non-present entry with this bit set stands for memory a
/// mapping promised and nothing has touched yet. In a page table it reserves
/// one page; in a page directory, the 512 under it. Its `WRITABLE` and
/// `NO_EXECUTE` bits say what the page will be once it is backed.
///
/// An entry carrying it is not empty. Everything that decides whether a table
/// can be freed, or whether an address is free to map, has to look at the
/// whole entry and not just `PRESENT`.
pub const MARKER: u64 = 1 << 10;
/// With `MARKER`: the page is a page of a memory object, named by the slot in
/// bits 52–62 and the page index in bits 12–51, rather than fresh zeroes.
pub const MARKER_OBJECT: u64 = 1 << 11;
/// With `MARKER_OBJECT`: the object's page itself is to be mapped, not a copy.
pub const MARKER_SHARED: u64 = 1 << 6;
/// Where an object marker keeps its object's slot.
pub const OBJECT_SHIFT: u64 = 52;

/// A reservation for anonymous memory.
pub fn marker_entry(writable: bool, exec: bool) -> u64 {
    MARKER | if writable { WRITABLE } else { 0 } | if exec { 0 } else { NO_EXECUTE }
}

fn is_marker(raw: u64) -> bool {
    raw & PRESENT == 0 && raw & MARKER != 0
}

/// Why a page could not be backed.
#[derive(Debug)]
pub enum Fault {
    /// Nothing was promised there, or not that access: the task's fault.
    Invalid,
    /// It was promised and there is nothing to give it with.
    NoMemory,
    /// A page of an object that cannot be had: past the end of the file, or
    /// its pager failed or has gone. SIGBUS, as on Linux.
    Bus,
}

/// The object slot an entry names, present or reserved; 0 for none.
pub fn object_slot(raw: u64) -> usize {
    let named = if raw & PRESENT != 0 { true } else { raw & MARKER != 0 && raw & MARKER_OBJECT != 0 };
    if named { ((raw >> OBJECT_SHIFT) & 0x7FF) as usize } else { 0 }
}

/// The reservation for page `page` of the object in `slot`.
pub fn object_marker(slot: usize, page: u64, write: bool, shared: bool, exec: bool) -> u64 {
    MARKER
        | MARKER_OBJECT
        | ((slot as u64) << OBJECT_SHIFT)
        | (page << 12)
        | if write { WRITABLE } else { 0 }
        | if shared { MARKER_SHARED } else { 0 }
        | if exec { 0 } else { NO_EXECUTE }
}

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
    /// Something is already mapped or reserved there.
    AlreadyMapped,
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
pub unsafe fn write_cr3(addr: usize) { unsafe {
    asm!("mov cr3, {}", in(reg) addr as u64, options(nomem, nostack));
}}

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
pub unsafe fn table_at(phys: usize) -> &'static mut PageTable { unsafe {
    &mut *(phys as *mut PageTable)
}}

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

/// The page table that maps `virt`, made if it is not there.
///
/// Absent tables are allocated. A 2 MiB huge page is split into 512 pages that
/// map what it mapped, and a 2 MiB reservation into 512 page reservations. A
/// 1 GiB huge page cannot be split (`HugePageConflict`). With `user`, every
/// entry on the way is given `USER`, so a user-mode walk succeeds.
unsafe fn walk_create(
    pml4_phys: usize,
    virt_addr: usize,
    user: u64,
) -> Result<&'static mut PageTable, PagingError> { unsafe {
    let (_, _, pdi, _) = table_indices(virt_addr);
    let pd = walk_create_pd(pml4_phys, virt_addr, user)?;
    let pde = pd.entries[pdi].raw();
    if pde & PRESENT != 0 && pde & HUGE_PAGE != 0 {
        // Split 2 MiB huge page into 512 × 4 KiB pages preserving the mapping
        let huge_phys = pd.entries[pdi].frame_address();
        let huge_flags = pde & !ADDR_MASK & !HUGE_PAGE;
        let new_pt = alloc_table()?;
        let pt = table_at(new_pt);
        for j in 0..512 {
            pt.entries[j].set(huge_phys + j * PAGE_SIZE, huge_flags);
        }
        pd.entries[pdi].set(new_pt, PRESENT | WRITABLE | user);
    } else if is_marker(pde) {
        // A 2 MiB reservation: the same promise, one page at a time.
        let new_pt = alloc_table()?;
        let pt = table_at(new_pt);
        for e in pt.entries.iter_mut() {
            *e = PageTableEntry(pde);
        }
        pd.entries[pdi].set(new_pt, PRESENT | WRITABLE | USER);
    }
    if !pd.entries[pdi].is_present() {
        let new_table = alloc_table()?;
        pd.entries[pdi].set(new_table, PRESENT | WRITABLE | user);
    } else if user != 0 && pd.entries[pdi].raw() & USER == 0 {
        pd.entries[pdi] = PageTableEntry(pd.entries[pdi].raw() | USER);
    }

    // Level 1: PT
    Ok(table_at(pd.entries[pdi].frame_address()))
}}

/// The page directory that holds `virt`'s entry, made if it is not there.
unsafe fn walk_create_pd(
    pml4_phys: usize,
    virt_addr: usize,
    user: u64,
) -> Result<&'static mut PageTable, PagingError> { unsafe {
    let (pml4i, pdpti, _, _) = table_indices(virt_addr);

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
    Ok(table_at(pdpt.entries[pdpti].frame_address()))
}}

/// Map a 4 KiB virtual page to a physical frame.
///
/// Walks the 4-level page table hierarchy, allocating intermediate tables
/// as needed (see [`walk_create`]). Mapping over a reservation replaces it.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn map_page(
    pml4_phys: usize,
    virt_addr: usize,
    phys_addr: usize,
    flags: u64,
) -> Result<(), PagingError> { unsafe {
    let (_, _, _, pti) = table_indices(virt_addr);
    let pt = walk_create(pml4_phys, virt_addr, flags & USER)?;

    // Replacing a live mapping: if this address space owned the old frame and
    // we are pointing the PTE somewhere else, return the old frame to the PMM
    // rather than leaking it.
    let old = pt.entries[pti];
    if old.is_present() && old.raw() & OWNED != 0 && old.frame_address() != phys_addr {
        pmm::free(pmm::PhysFrame::from_address(old.frame_address()));
    }
    crate::memobj::drop_entry(old.raw());

    pt.entries[pti].set(phys_addr, flags);

    invlpg(virt_addr);

    Ok(())
}}

/// Reserve `pages` pages from `virt` with reservation `entry`, which must be a
/// marker. A whole 2 MiB is reserved in its page-directory entry, so a large
/// reservation costs almost nothing until it is touched. Everything in the
/// range must be free ([`range_is_free`]); on failure, what was reserved is
/// left for the caller to clear.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped user PML4 table.
pub unsafe fn reserve_range(
    pml4_phys: usize,
    virt: usize,
    pages: usize,
    entry: u64,
) -> Result<(), PagingError> { unsafe {
    const TWO_MIB: usize = 512 * PAGE_SIZE;
    let end = virt + pages * PAGE_SIZE;
    let mut va = virt;
    while va < end {
        let (_, _, pdi, pti) = table_indices(va);
        if va & (TWO_MIB - 1) == 0 && end - va >= TWO_MIB {
            // The whole page-directory entry, which must be unused.
            let pd = walk_create_pd(pml4_phys, va, USER)?;
            if pd.entries[pdi].raw() != 0 {
                return Err(PagingError::AlreadyMapped);
            }
            pd.entries[pdi] = PageTableEntry(entry);
            va += TWO_MIB;
        } else {
            let pt = walk_create(pml4_phys, va, USER)?;
            if pt.entries[pti].raw() != 0 {
                return Err(PagingError::AlreadyMapped);
            }
            pt.entries[pti] = PageTableEntry(entry);
            va += PAGE_SIZE;
        }
    }
    Ok(())
}}

/// Reserve `pages` pages from `virt` for pages `first..` of the object in
/// `slot`. Each page's entry names its own page, so there is no 2 MiB form.
/// The range must be free; on failure, what was reserved is left for the
/// caller to clear, and is counted as mapped until it is.
///
/// # Safety
/// As [`reserve_range`].
pub unsafe fn reserve_object(
    pml4_phys: usize,
    virt: usize,
    pages: usize,
    slot: usize,
    first: u64,
    write: bool,
    shared: bool,
    exec: bool,
) -> Result<(), PagingError> { unsafe {
    for i in 0..pages {
        let va = virt + i * PAGE_SIZE;
        let (_, _, _, pti) = table_indices(va);
        let pt = walk_create(pml4_phys, va, USER)?;
        if pt.entries[pti].raw() != 0 {
            return Err(PagingError::AlreadyMapped);
        }
        pt.entries[pti] = PageTableEntry(object_marker(slot, first + i as u64, write, shared, exec));
        crate::memobj::map_ref(slot, 1);
    }
    Ok(())
}}

/// The reservation covering `virt`, if the page is reserved and not backed.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn marker(pml4_phys: usize, virt: usize) -> Option<u64> { unsafe {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);
    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() || e.is_huge() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdi];
    if is_marker(e.raw()) {
        return Some(e.raw());
    }
    if !e.is_present() || e.is_huge() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pti];
    is_marker(e.raw()).then_some(e.raw())
}}

/// Give the reserved page at `virt` its memory: a zeroed frame, charged to the
/// current task, mapped as its reservation said. `write` is whether the access
/// that wants it is a write.
///
/// # Safety
/// `pml4_phys` must be the current task's address space; interrupts off, so
/// nothing else changes its tables in between.
pub unsafe fn back(pml4_phys: usize, virt: usize, write: bool, may_block: bool) -> Result<(), Fault> { unsafe {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);
    if (virt as u64) < USER_MIN_ADDR || (virt as u64) >= USER_ADDR_LIMIT {
        return Err(Fault::Invalid);
    }
    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return Err(Fault::Invalid);
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() || e.is_huge() {
        return Err(Fault::Invalid);
    }
    let pd = table_at(e.frame_address());
    let pde = pd.entries[pdi].raw();
    if is_marker(pde) {
        if pde & MARKER_OBJECT != 0 || (write && pde & WRITABLE == 0) {
            return Err(Fault::Invalid);
        }
        // Split the 2 MiB reservation; the page wanted is backed below.
        let new_pt = alloc_table().map_err(|_| Fault::NoMemory)?;
        let pt = table_at(new_pt);
        for entry in pt.entries.iter_mut() {
            *entry = PageTableEntry(pde);
        }
        pd.entries[pdi].set(new_pt, PRESENT | WRITABLE | USER);
    } else if pde & PRESENT == 0 || pde & HUGE_PAGE != 0 {
        return Err(Fault::Invalid);
    }
    let pt = table_at(pd.entries[pdi].frame_address());
    let raw = pt.entries[pti].raw();
    if !is_marker(raw) || (write && raw & WRITABLE == 0) {
        return Err(Fault::Invalid);
    }
    if raw & MARKER_OBJECT != 0 {
        return back_object(pml4_phys, virt, raw, may_block);
    }
    if !crate::scheduler::current_task_check_mem(1) {
        return Err(Fault::NoMemory);
    }
    let frame = pmm::alloc().ok_or(Fault::NoMemory)?.address();
    core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE);
    crate::scheduler::current_task_charge_mem(1);
    pt.entries[pti].set(frame, PRESENT | USER | OWNED | (raw & (WRITABLE | NO_EXECUTE)));
    invlpg(virt & !0xFFF);
    Ok(())
}}

/// Give a reserved page of an object its page: the object's own cached frame
/// for a shared or read-only mapping, a private copy for a private writable
/// one. The object may have to ask its pager, which blocks.
unsafe fn back_object(pml4_phys: usize, virt: usize, raw: u64, may_block: bool) -> Result<(), Fault> { unsafe {
    let slot = object_slot(raw);
    let page = (raw >> 12) & crate::memobj::MAX_PAGE;
    let frame = crate::memobj::page_in(slot, page, may_block)?;
    // Paging in may have blocked, and a thread sharing the address space may
    // have changed the entry meanwhile. If it has, this fault is over; the
    // instruction runs again and meets whatever is there now.
    let (_, _, _, pti) = table_indices(virt);
    let Some(pt) = leaf_table(pml4_phys, virt) else { return Ok(()) };
    if pt.entries[pti].raw() != raw {
        return Ok(());
    }
    let writable = raw & WRITABLE != 0;
    let shared = raw & MARKER_SHARED != 0;
    let keep = (raw & NO_EXECUTE) | ((slot as u64) << OBJECT_SHIFT);
    if shared || !writable {
        // The object's page itself, which this address space does not own.
        if shared && writable {
            // Anything written through it has to go back to the file.
            crate::memobj::mapped_writable(slot, page);
        }
        let w = if shared && writable { WRITABLE } else { 0 };
        pt.entries[pti].set(frame, PRESENT | USER | w | keep);
    } else {
        if !crate::scheduler::current_task_check_mem(1) {
            return Err(Fault::NoMemory);
        }
        let copy = pmm::alloc().ok_or(Fault::NoMemory)?.address();
        core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
        crate::scheduler::current_task_charge_mem(1);
        pt.entries[pti].set(copy, PRESENT | USER | WRITABLE | OWNED | keep);
    }
    invlpg(virt & !0xFFF);
    Ok(())
}}

/// The page table holding `virt`'s entry, if there is one.
unsafe fn leaf_table(pml4_phys: usize, virt: usize) -> Option<&'static mut PageTable> { unsafe {
    let (pml4i, pdpti, pdi, _) = table_indices(virt);
    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() || e.is_huge() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdi];
    if !e.is_present() || e.is_huge() {
        return None;
    }
    Some(table_at(e.frame_address()))
}}

/// Back every reserved page of `[addr, addr + len)`, so that the kernel can
/// copy to or from it. Pages that are not reserved are left for the caller's
/// own check. A page of an object may block while its pager fills it.
///
/// # Safety
/// As [`back`].
pub unsafe fn back_range(pml4_phys: usize, addr: u64, len: u64, write: bool) -> Result<(), Fault> { unsafe {
    if len == 0 {
        return Ok(());
    }
    let Some(end) = addr.checked_add(len) else {
        return Err(Fault::Invalid);
    };
    let mut page = addr & !0xFFF;
    while page < end {
        if marker(pml4_phys, page as usize).is_some() {
            back(pml4_phys, page as usize, write, true)?;
        }
        page += PAGE_SIZE as u64;
    }
    Ok(())
}}

/// The objects mapped shared, and so writable through to their files, in
/// `[virt, virt + pages)`: up to `out.len()` distinct slots, and how many.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn shared_objects_in(pml4_phys: usize, virt: usize, pages: usize, out: &mut [usize]) -> usize { unsafe {
    let end = virt.saturating_add(pages.saturating_mul(PAGE_SIZE));
    let mut va = virt;
    let mut n = 0;
    while va < end {
        let Some(pt) = leaf_table(pml4_phys, va) else {
            va = next_boundary(va, 1 << 21);
            continue;
        };
        let (_, _, _, pti) = table_indices(va);
        let raw = pt.entries[pti].raw();
        let slot = object_slot(raw);
        // Present and not the address space's own: the object's frame, which
        // a private writable mapping never maps.
        let shared = slot != 0 && raw & PRESENT != 0 && raw & OWNED == 0;
        if shared && !out[..n].contains(&slot) && n < out.len() {
            out[n] = slot;
            n += 1;
        }
        va += PAGE_SIZE;
    }
    n
}}

/// The next address after `va` that is a multiple of `size`.
fn next_boundary(va: usize, size: usize) -> usize {
    (va | (size - 1)).wrapping_add(1)
}

/// Whether nothing is mapped or reserved anywhere in `[virt, virt + pages)`.
/// Absent tables are stepped over whole, so a large range costs little.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn range_is_free(pml4_phys: usize, virt: usize, pages: usize) -> bool { unsafe {
    let end = virt + pages * PAGE_SIZE;
    let mut va = virt;
    while va < end {
        let (pml4i, pdpti, pdi, pti) = table_indices(va);
        let e4 = table_at(pml4_phys).entries[pml4i];
        if e4.raw() == 0 {
            va = next_boundary(va, 1 << 39);
            continue;
        }
        if !e4.is_present() {
            return false;
        }
        let e3 = table_at(e4.frame_address()).entries[pdpti];
        if e3.raw() == 0 {
            va = next_boundary(va, 1 << 30);
            continue;
        }
        if !e3.is_present() || e3.is_huge() {
            return false;
        }
        let e2 = table_at(e3.frame_address()).entries[pdi];
        if e2.raw() == 0 {
            va = next_boundary(va, 1 << 21);
            continue;
        }
        if !e2.is_present() || e2.is_huge() {
            return false;
        }
        if table_at(e2.frame_address()).entries[pti].raw() != 0 {
            return false;
        }
        va += PAGE_SIZE;
    }
    true
}}

/// Clear `[virt, virt + pages)` of mappings and reservations alike, freeing
/// the frames this address space owns and the tables left empty. Returns how
/// many frames went back to the allocator.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped user PML4 table, and the
/// range must be in the user window.
pub unsafe fn clear_range(pml4_phys: usize, virt: usize, pages: usize) -> usize { unsafe {
    const TWO_MIB: usize = 512 * PAGE_SIZE;
    let end = virt + pages * PAGE_SIZE;
    let mut va = virt;
    let mut freed = 0;
    while va < end {
        let (pml4i, pdpti, pdi, _) = table_indices(va);
        let e4 = table_at(pml4_phys).entries[pml4i];
        if !e4.is_present() {
            va = next_boundary(va, 1 << 39);
            continue;
        }
        let e3 = table_at(e4.frame_address()).entries[pdpti];
        if !e3.is_present() || e3.is_huge() {
            va = next_boundary(va, 1 << 30);
            continue;
        }
        let pd = table_at(e3.frame_address());
        let pde = pd.entries[pdi].raw();
        let chunk_end = next_boundary(va, TWO_MIB).min(end);
        if is_marker(pde) {
            if va & (TWO_MIB - 1) == 0 && chunk_end - va == TWO_MIB {
                pd.entries[pdi].clear();
                reclaim_empty_tables(pml4_phys, va);
                va = chunk_end;
                continue;
            }
            // Part of it goes: it has to be a page table first.
            if walk_create(pml4_phys, va, USER).is_err() {
                // No memory for the table: the reservation stays whole.
                va = chunk_end;
                continue;
            }
        } else if pde & PRESENT == 0 || pde & HUGE_PAGE != 0 {
            va = chunk_end;
            continue;
        }
        let pt = table_at(pd.entries[pdi].frame_address());
        while va < chunk_end {
            let (_, _, _, pti) = table_indices(va);
            let e = pt.entries[pti];
            if e.is_present() {
                if e.raw() & OWNED != 0 {
                    pmm::free(pmm::PhysFrame::from_address(e.frame_address()));
                    freed += 1;
                }
                pt.entries[pti].clear();
                invlpg(va);
            } else if e.raw() != 0 {
                pt.entries[pti].clear();
            }
            crate::memobj::drop_entry(e.raw());
            va += PAGE_SIZE;
        }
        reclaim_empty_tables(pml4_phys, va - PAGE_SIZE);
    }
    freed
}}

/// Look up the leaf PTE flags for `virt` in the address space rooted at
/// `pml4_phys`. Returns `None` if any level along the walk is absent.
///
/// Huge pages are reported with their own flags; the caller only cares about
/// PRESENT/USER/WRITABLE, which are meaningful at every level.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn walk_flags(pml4_phys: usize, virt: usize) -> Option<u64> { unsafe {
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
}}

/// Build a flags word carrying just the effective PRESENT/USER/WRITABLE bits
/// produced by a page-table walk.
fn synth_flags(user: bool, writable: bool) -> u64 {
    PRESENT
        | if user { USER } else { 0 }
        | if writable { WRITABLE } else { 0 }
}

/// The raw flags of the 4 KiB page mapped at `virt`, or `None` if there is no
/// such page — including when `virt` falls inside a huge page.
///
/// Unlike [`walk_flags`] this reports the leaf entry itself, so it carries
/// `OWNED`, which is what decides whether the page is the address space's to
/// give away.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn leaf_flags(pml4_phys: usize, virt: usize) -> Option<u64> { unsafe {
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);

    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() || e.is_huge() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pdi];
    if !e.is_present() || e.is_huge() {
        return None;
    }
    let e = table_at(e.frame_address()).entries[pti];
    if !e.is_present() {
        return None;
    }
    Some(e.flags())
}}

/// Resolve `virt` to its backing physical address in `pml4_phys`.
///
/// Returns `None` if the page is not mapped. Used to key futexes on physical
/// memory so a futex word inside a shared-memory region is the same object to
/// every task that maps it.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn translate(pml4_phys: usize, virt: usize) -> Option<usize> { unsafe {
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
}}

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
) -> bool { unsafe {
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
}}

/// Destroy a user address space, freeing all user page tables and mapped frames.
///
/// - PML4[0]: Free the deep-copied PDPT frame only (PD children are kernel-shared).
/// - PML4[1..255]: Recursively free entire subtree (user-only).
/// - PML4[256..511]: Skip (kernel upper-half, shared).
/// - Free the PML4 frame itself.
///
/// # Safety
/// `pml4_phys` must be a valid user address space (not the kernel CR3).
pub unsafe fn destroy_address_space(pml4_phys: usize) { unsafe {
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
}}

unsafe fn free_pdpt_tree(pdpt_phys: usize) { unsafe {
    let pdpt = table_at(pdpt_phys);
    for i in 0..512 {
        if pdpt.entries[i].is_present() && !pdpt.entries[i].is_huge() {
            let pd_phys = pdpt.entries[i].frame_address();
            free_pd_tree(pd_phys);
        }
    }
    pmm::free(pmm::PhysFrame::from_address(pdpt_phys));
}}

unsafe fn free_pd_tree(pd_phys: usize) { unsafe {
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
}}

unsafe fn free_pt_leaves(pt_phys: usize) { unsafe {
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
        // Mapped or reserved, a page of an object is one reference fewer.
        crate::memobj::drop_entry(pt.entries[i].raw());
    }
    pmm::free(pmm::PhysFrame::from_address(pt_phys));
}}

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
) -> Result<(usize, u64), PagingError> { unsafe {
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
    crate::memobj::drop_entry(pt.entries[pti].raw());
    pt.entries[pti].clear();

    invlpg(virt_addr);

    // Release page tables that just became empty. Restricted to the per-address
    // -space user window: the tables under PML4[0] are shared with the kernel
    // and must never be freed here.
    if (virt_addr as u64) >= USER_MIN_ADDR {
        reclaim_empty_tables(pml4_phys, virt_addr);
    }

    Ok((frame_addr, flags))
}}

/// True if every entry in the table at `phys` is zero: neither mapped nor
/// reserved.
unsafe fn table_is_empty(phys: usize) -> bool { unsafe {
    table_at(phys).entries.iter().all(|e| e.raw() == 0)
}}

/// Walk back up from a just-cleared PTE, freeing each level that is now empty.
unsafe fn reclaim_empty_tables(pml4_phys: usize, virt_addr: usize) { unsafe {
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
}}

/// Unmap a 4 KiB page and return its frame to the PMM, but only if this
/// address space owned it. Returns `true` if a frame was actually freed.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn unmap_page_owned(pml4_phys: usize, virt_addr: usize) -> bool { unsafe {
    match unmap_page(pml4_phys, virt_addr) {
        Ok((frame_addr, flags)) if flags & OWNED != 0 => {
            pmm::free(pmm::PhysFrame::from_address(frame_addr));
            true
        }
        _ => false,
    }
}}

/// Copy the user half of an address space, for a fork.
///
/// A page the source owns becomes a page of its own with the same bytes; a
/// page it does not own — shared memory, a device, a file's page — is mapped
/// at the same frame, because that is what sharing means and because `OWNED`
/// is what decides who may free a frame. A reservation is copied as a
/// reservation: the promise is inherited, and whoever touches the page first
/// gets a frame for it.
///
/// Eager, and not copy-on-write. A frame here has no reference count, so
/// sharing one writable between two address spaces would need one; the
/// immediate use of `fork` is a child that immediately execs, where
/// copy-on-write saves all of the copying and none of the correctness.
///
/// Returns the pages charged to the child, or `None` if anything ran out. On
/// failure the caller destroys the half-built space, which frees exactly what
/// this made: everything it allocated carries `OWNED`.
///
/// # Safety
/// Both must be valid, identity-mapped PML4 tables, and `dst_pml4` must be a
/// fresh space from `create_address_space`.
pub unsafe fn copy_user_space(src_pml4: usize, dst_pml4: usize) -> Option<usize> { unsafe {
    let src = table_at(src_pml4);
    let dst = table_at(dst_pml4);
    let mut pages = 0usize;
    // PML4[1..256] is user space: [`USER_MIN_ADDR`] upwards, and the stack at
    // the top of it. PML4[0] is the kernel's and is already shared by
    // `create_address_space`; 256 and above is the kernel's upper half.
    for i in 1..256 {
        if !src.entries[i].is_present() {
            continue;
        }
        let src_pdpt = src.entries[i].frame_address();
        let flags = src.entries[i].raw() & (PRESENT | WRITABLE | USER);
        let new_pdpt = pmm::alloc()?.address();
        core::ptr::write_bytes(new_pdpt as *mut u8, 0, PAGE_SIZE);
        dst.entries[i].set(new_pdpt, flags);
        pages += copy_pdpt(src_pdpt, new_pdpt)?;
    }
    Some(pages)
}}

unsafe fn copy_pdpt(src_phys: usize, dst_phys: usize) -> Option<usize> { unsafe {
    let src = table_at(src_phys);
    let dst = table_at(dst_phys);
    let mut pages = 0usize;
    for i in 0..512 {
        let raw = src.entries[i].raw();
        if raw == 0 {
            continue;
        }
        if !src.entries[i].is_present() || src.entries[i].is_huge() {
            // A gigabyte reservation, or a huge mapping nothing here makes.
            dst.entries[i] = src.entries[i];
            crate::memobj::map_ref(object_slot(raw), 1);
            continue;
        }
        let new_pd = pmm::alloc()?.address();
        core::ptr::write_bytes(new_pd as *mut u8, 0, PAGE_SIZE);
        dst.entries[i].set(new_pd, raw & (PRESENT | WRITABLE | USER));
        pages += copy_pd(src.entries[i].frame_address(), new_pd)?;
    }
    Some(pages)
}}

unsafe fn copy_pd(src_phys: usize, dst_phys: usize) -> Option<usize> { unsafe {
    let src = table_at(src_phys);
    let dst = table_at(dst_phys);
    let mut pages = 0usize;
    for i in 0..512 {
        let raw = src.entries[i].raw();
        if raw == 0 {
            continue;
        }
        if !src.entries[i].is_present() || src.entries[i].is_huge() {
            // A two-megabyte reservation is one entry and no table under it.
            dst.entries[i] = src.entries[i];
            crate::memobj::map_ref(object_slot(raw), 1);
            continue;
        }
        let new_pt = pmm::alloc()?.address();
        core::ptr::write_bytes(new_pt as *mut u8, 0, PAGE_SIZE);
        dst.entries[i].set(new_pt, raw & (PRESENT | WRITABLE | USER));
        pages += copy_pt(src.entries[i].frame_address(), new_pt)?;
    }
    Some(pages)
}}

unsafe fn copy_pt(src_phys: usize, dst_phys: usize) -> Option<usize> { unsafe {
    let src = table_at(src_phys);
    let dst = table_at(dst_phys);
    let mut pages = 0usize;
    for i in 0..512 {
        let raw = src.entries[i].raw();
        if raw == 0 {
            continue;
        }
        if raw & PRESENT != 0 && raw & OWNED != 0 {
            // The child's own copy of the page, and the only one it may free.
            let frame = pmm::alloc()?.address();
            core::ptr::copy_nonoverlapping(
                src.entries[i].frame_address() as *const u8,
                frame as *mut u8,
                PAGE_SIZE,
            );
            dst.entries[i].set(frame, raw & !ADDR_MASK);
            pages += 1;
            continue;
        }
        // Shared, or a device, or a page of an object, or a reservation: the
        // same entry, and one more reference to whatever it names.
        dst.entries[i] = src.entries[i];
        let slot = object_slot(raw);
        if slot != 0 {
            crate::memobj::map_ref(slot, 1);
            if raw & PRESENT != 0 && raw & WRITABLE != 0 && raw & MARKER_SHARED != 0 {
                // A second shared writable mapping of the same page, which the
                // object counts so that it knows to write back.
                crate::memobj::mapped_writable(slot, (raw >> 12) & crate::memobj::MAX_PAGE);
            }
        }
    }
    Some(pages)
}}
