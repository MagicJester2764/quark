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

const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
const PAGE_SIZE: usize = 4096;

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
    pt.entries[pti].set(phys_addr, flags);

    invlpg(virt_addr);

    Ok(())
}

/// Unmap a 4 KiB virtual page. Returns the physical frame address that was mapped.
///
/// The caller is responsible for freeing the returned frame if desired.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn unmap_page(
    pml4_phys: usize,
    virt_addr: usize,
) -> Result<usize, PagingError> {
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
    pt.entries[pti].clear();

    invlpg(virt_addr);

    Ok(frame_addr)
}
