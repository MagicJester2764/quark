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
/// An owned frame is mapped in exactly one place, or is counted: a `fork`
/// leaves a page in the child that the parent has too, and the frame keeps
/// a count of how many address spaces have it as their own (`pmm::share`).
/// Everything that sets this bit keeps that true, and `pmm::free` gives a
/// frame back to the allocator only when the last of them has let it go.
/// That is what makes freeing on unmap safe.
pub const OWNED: u64 = 1 << 9;

/// With `PRESENT` and `OWNED`: the page was writable, is shared since a
/// `fork`, and is to be copied before it is written ([`own`]). The entry is
/// not `WRITABLE` while this is set, so the write is a fault. (The bit is
/// [`MARKER`]'s, which means something only in an entry that is not
/// present.)
pub const COPY_ON_WRITE: u64 = 1 << 10;

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

/// Whether an entry is a reservation for a page of the program's own that
/// was written out (`memobj::swap_slot`), rather than for a page of a file.
fn is_written_out(raw: u64) -> bool {
    is_marker(raw) && raw & MARKER_OBJECT != 0 && {
        let slot = ((raw >> OBJECT_SHIFT) & 0x7FF) as usize;
        slot != 0 && slot == crate::memobj::swap_slot()
    }
}

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Held for as long as a walk of a program's tables goes on: the walk is one
/// step.
///
/// A system call is preempted wherever a tick finds it, and another thread
/// of the program can be run in between — on this processor, or on another
/// once this one has left the kernel for the task it switched to. An unmap
/// gives back a table it leaves empty, and the directory above it if that
/// is empty too ([`reclaim_empty_tables`]); a walk that had read its way
/// down to one went on from where it had got to. A map filled in a
/// directory that was no longer anybody's — its page was in no table the
/// program could reach, and the program faulted on the page it had just
/// been given — and wrote into a frame that may by then have been somebody
/// else's. Every walk here takes one of these, or is only ever called from
/// one that has (`back`, `own` and `unshare` take one; the fork's copy and
/// reclaim's takes hold the spaces' locks themselves).
///
/// A step is the address space's lock held (`sync::RANK_SPACE`), with
/// interrupts off as every lock has them — unless this processor holds it
/// already, a step begun inside another of the same space's. The kernel's
/// own tables are changed only by the heap as it grows, under the heap's
/// lock, which comes after the spaces': a step of theirs is interrupts off
/// alone, as every step was under the one lock.
enum OneStep {
    Kernel(u64),
    Space { _held: Option<crate::sync::IrqSpinLockGuard<'static, ()>> },
    /// Given up for a wait in the middle (`back_object`'s pager).
    Released,
}

impl OneStep {
    #[inline(always)]
    fn new(pml4_phys: usize) -> Self {
        if pml4_phys == kernel_cr3() {
            OneStep::Kernel(irq_save())
        } else {
            OneStep::Space { _held: space_lock(pml4_phys).lock_unless_held() }
        }
    }
}

impl Drop for OneStep {
    #[inline(always)]
    fn drop(&mut self) {
        if let OneStep::Kernel(flags) = *self {
            irq_restore(flags);
        }
    }
}

/// How many locks the address spaces share: a space's is the one its root
/// hashes to, the same every time, and two spaces share one seldom. A
/// record for each space would be found by a walk of them, at every step.
const SPACE_LOCKS: usize = 64;

static SPACES: [crate::sync::IrqSpinLock<()>; SPACE_LOCKS] =
    [const { crate::sync::IrqSpinLock::new(crate::sync::RANK_SPACE, "an address space's tables", ()) }; SPACE_LOCKS];

/// The lock of the address space rooted at `pml4_phys`: its tables, its
/// reservations, and what is forgotten of them (`tlb::stale`).
pub fn space_lock(pml4_phys: usize) -> &'static crate::sync::IrqSpinLock<()> {
    let mixed = ((pml4_phys >> 12) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    &SPACES[(mixed >> 58) as usize % SPACE_LOCKS]
}

/// The locks of two address spaces, in the order of where they are — or
/// the one, if the two share it: for what walks one and changes the other
/// (a fork's copy, pages moved between them).
pub fn lock_two_spaces(a: usize, b: usize) -> (crate::sync::IrqSpinLockGuard<'static, ()>, Option<crate::sync::IrqSpinLockGuard<'static, ()>>) {
    let (la, lb) = (space_lock(a), space_lock(b));
    if core::ptr::eq(la, lb) {
        return (la.lock(), None);
    }
    let (first, second) = if (la as *const _ as usize) < (lb as *const _ as usize) { (la, lb) } else { (lb, la) };
    let held = first.lock();
    (held, Some(second.lock_second()))
}

/// Why a page could not be backed.
#[derive(Debug)]
pub enum Fault {
    /// Nothing was promised there, or not that access: the task's fault.
    Invalid,
    /// It was promised and there is nothing to give it with: no frame is
    /// free. Something may be done about that (`reclaim.rs`), and whoever
    /// can wait for it does.
    NoMemory,
    /// It was promised and the task may not have it: it is at the limit it
    /// was given. Nothing is to be done about that.
    Limit,
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

/// One past the highest user address. Everything at or above this is the
/// kernel's, not canonical, or the page below 2^47 that is nobody's: a
/// `syscall` in its last two bytes would go back to 2^47, which is not an
/// address, and Intel's `sysret` faults on that in ring 0, after `swapgs`,
/// with the program's stack pointer already loaded — a fault the kernel
/// takes on a stack the program chose (CVE-2012-0217). Linux keeps the same
/// page out of every program. No layout here ever reached it: every stack
/// ends at 0x7FFF_FFFF_F000.
pub const USER_ADDR_LIMIT: u64 = 0x0000_7FFF_FFFF_F000;

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

/// Where the kernel's own map of memory ends: every physical address below
/// this is one the kernel can touch as itself. Four gigabytes as the
/// machine starts (`boot.s`), and as much memory as there is once
/// [`map_all_memory`] has run.
static IDENTITY_END: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(1 << 32);

/// Where the kernel's own map of memory ends.
pub fn identity_end() -> usize {
    IDENTITY_END.load(core::sync::atomic::Ordering::Relaxed)
}

/// Extend the kernel's own map over all of the machine's memory, up to
/// `top`: a gigabyte at a time, as one page where the processor has pages
/// that large and as a directory of two-megabyte ones where it has not.
///
/// The map is the first entry of the top-level table, and every address
/// space is made with a copy of the table under it
/// (`userspace::create_address_space`): so this is done once, at boot,
/// before there is an address space to have missed it. The last two
/// gigabytes of that entry are the kernel's stacks and its heap
/// (`kstack.rs`, `heap.rs`), which is why memory past `pmm::MAX_PHYS` is not
/// used.
///
/// # Safety
/// Once, on the first processor, before the heap, before any address space
/// is made and before another processor is started; `save_kernel_cr3` has
/// run.
pub unsafe fn map_all_memory(top: usize) { unsafe {
    const GIB: usize = 1 << 30;
    let pml4 = table_at(kernel_cr3());
    let pdpt = table_at(pml4.entries[0].frame_address());
    // CPUID 8000_0001, EDX bit 26: pages of a gigabyte.
    let whole = core::arch::x86_64::__cpuid(0x8000_0000).eax >= 0x8000_0001
        && core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 26) != 0;
    let mut end = identity_end();
    for i in end / GIB..top.div_ceil(GIB).min(510) {
        if !pdpt.entries[i].is_present() {
            if whole {
                pdpt.entries[i].set(i * GIB, PRESENT | WRITABLE | HUGE_PAGE);
            } else {
                // A directory, in memory the map already reaches.
                let Some(frame) = pmm::alloc_low() else { break };
                let pd = table_at(frame.address());
                for (j, entry) in pd.entries.iter_mut().enumerate() {
                    entry.set(i * GIB + j * (2 << 20), PRESENT | WRITABLE | HUGE_PAGE);
                }
                pdpt.entries[i].set(frame.address(), PRESENT | WRITABLE);
            }
        }
        end = (i + 1) * GIB;
    }
    IDENTITY_END.store(end, core::sync::atomic::Ordering::Relaxed);
}}

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
/// And say so where the other processors can read it (`percpu::cr3_of`):
/// one that takes a mapping out of an address space asks every processor
/// that has the space loaded to forget it. The two are done with interrupts
/// off, so that no switch comes between them and finds one without the
/// other.
///
/// # Safety
/// The address must point to a valid, identity-mapped PML4 table.
pub unsafe fn write_cr3(addr: usize) { unsafe {
    let flags: u64;
    asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    asm!("mov cr3, {}", in(reg) addr as u64, options(nomem, nostack));
    crate::percpu::note_cr3(addr);
    if flags & (1 << 9) != 0 {
        asm!("sti", options(nomem, nostack));
    }
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

/// Leave the page at `virt` with nothing behind it, the tables down to it made
/// and a page of two megabytes it was in split: the page below a kernel
/// stack (`kstack.rs`), which the kernel wants a fault on.
///
/// # Safety
/// At boot, on the first processor, with interrupts off.
pub unsafe fn leave_unmapped(pml4_phys: usize, virt_addr: usize) -> Result<(), PagingError> { unsafe {
    let (_, _, _, pti) = table_indices(virt_addr);
    let pt = walk_create(pml4_phys, virt_addr, 0)?;
    pt.entries[pti] = PageTableEntry(0);
    core::arch::asm!("invlpg [{}]", in(reg) virt_addr, options(nostack, preserves_flags));
    Ok(())
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
    let _step = OneStep::new(pml4_phys);
    let (_, _, _, pti) = table_indices(virt_addr);
    let pt = walk_create(pml4_phys, virt_addr, flags & USER)?;

    // Replacing a live mapping: if this address space owned the old frame and
    // we are pointing the PTE somewhere else, return the old frame to the PMM
    // rather than leaking it.
    let old = pt.entries[pti];
    if old.is_present() && old.raw() & OWNED != 0 && old.frame_address() != phys_addr {
        pmm::free(pmm::PhysFrame::from_address(old.frame_address()));
    }
    if old.is_present() {
        // What was here is still in the cache of any other processor that
        // has this address space loaded.
        crate::tlb::stale(pml4_phys);
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
        // A step an entry: each starts again from the top.
        let _step = OneStep::new(pml4_phys);
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
        let _step = OneStep::new(pml4_phys);
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
    let _step = OneStep::new(pml4_phys);
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
/// Looking at the entry and filling it are one step, with interrupts off:
/// a system call is preempted wherever a tick finds it, and another thread
/// of the program, run in between, may fault on the same page and be given
/// it. Done in two steps, the second frame replaced the first, which was
/// never freed, and what the other thread had written to it by then was
/// gone. (A page of an object waits for its pager in the middle, and looks
/// again when it has it.)
///
/// # Safety
/// `pml4_phys` must be the current task's address space.
pub unsafe fn back(pml4_phys: usize, virt: usize, write: bool, may_block: bool) -> Result<(), Fault> { unsafe {
    let mut step = OneStep::new(pml4_phys);
    back_one(pml4_phys, virt, write, may_block, &mut step)
}}

unsafe fn back_one(pml4_phys: usize, virt: usize, write: bool, may_block: bool, step: &mut OneStep) -> Result<(), Fault> { unsafe {
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
        return back_object(pml4_phys, virt, raw, may_block, step);
    }
    if !crate::scheduler::current_task_check_mem(1) {
        return Err(Fault::Limit);
    }
    let frame = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
    core::ptr::write_bytes(frame as *mut u8, 0, PAGE_SIZE);
    crate::scheduler::current_task_charge_mem(1);
    pt.entries[pti].set(frame, PRESENT | USER | OWNED | (raw & (WRITABLE | NO_EXECUTE)));
    invlpg(virt & !0xFFF);
    Ok(())
}}

/// Give a reserved page of an object its page: the object's own cached frame
/// for a shared or read-only mapping, a private copy for a private writable
/// one. The object may have to ask its pager, which blocks.
unsafe fn back_object(pml4_phys: usize, virt: usize, raw: u64, may_block: bool, step: &mut OneStep) -> Result<(), Fault> { unsafe {
    let slot = object_slot(raw);
    let page = (raw >> 12) & crate::memobj::MAX_PAGE;
    // Not with the space's lock held: paging in may wait for a pager, and
    // nothing is held across a wait.
    *step = OneStep::Released;
    let frame = crate::memobj::page_in(slot, page, may_block)?;
    *step = OneStep::new(pml4_phys);
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
    if slot == crate::memobj::swap_slot() {
        // A page of the program's own that was written out: it is the
        // program's own again, as it was, and no page of any object. The
        // frame itself if this is the one entry that names the page; a copy
        // if a fork left several that do. Nothing is charged: it was
        // counted as this program's all the while.
        let flags = PRESENT | USER | OWNED | (raw & (WRITABLE | NO_EXECUTE));
        match crate::memobj::swap_take(page) {
            Some(own) => pt.entries[pti].set(own, flags),
            None => {
                let copy = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
                core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
                pt.entries[pti].set(copy, flags);
                crate::memobj::swap_unref(slot, page);
            }
        }
        crate::memobj::unmap_ref(slot, 1);
        invlpg(virt & !0xFFF);
        return Ok(());
    }
    let keep = (raw & NO_EXECUTE) | ((slot as u64) << OBJECT_SHIFT);
    if shared || !writable {
        // The object's page itself, which this address space does not own:
        // the cache's frame, with one more mapping of it to count.
        if shared && writable {
            // Anything written through it has to go back to the file.
            crate::memobj::mapped_writable(slot, page);
        }
        let w = if shared && writable { WRITABLE } else { 0 };
        pt.entries[pti].set(frame, PRESENT | USER | w | keep);
        pmm::mapped(frame);
    } else {
        if !crate::scheduler::current_task_check_mem(1) {
            return Err(Fault::Limit);
        }
        let copy = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
        core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
        crate::scheduler::current_task_charge_mem(1);
        pt.entries[pti].set(copy, PRESENT | USER | WRITABLE | OWNED | keep);
    }
    invlpg(virt & !0xFFF);
    Ok(())
}}

/// What `virt` is in `pml4_phys`, for saying so when a program faults there:
/// the page directory's entry, and the page table's if the directory names
/// a table — a reservation is a non-present entry with `MARKER` at either.
/// Noughts where there is nothing.
pub unsafe fn entries_of(pml4_phys: usize, virt: usize) -> (u64, u64) { unsafe {
    let _step = OneStep::new(pml4_phys);
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);
    let e = table_at(pml4_phys).entries[pml4i];
    if !e.is_present() {
        return (0, 0);
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !e.is_present() || e.is_huge() {
        return (0, 0);
    }
    let pde = table_at(e.frame_address()).entries[pdi];
    if !pde.is_present() || pde.is_huge() {
        return (pde.raw(), 0);
    }
    (pde.raw(), table_at(pde.frame_address()).entries[pti].raw())
}}

/// The word of a program's memory at `virt`, where its page is there for
/// it to read — for saying what a program that faulted was running and where
/// it had been called from; nothing is backed or waited for to read it.
pub unsafe fn peek_user(pml4_phys: usize, virt: usize) -> Option<u64> { unsafe {
    let end = virt.checked_add(7)?;
    if virt & 7 != 0 || !user_range_ok(virt & !0xFFF, 1) || !user_range_ok(end & !0xFFF, 1) {
        return None;
    }
    // The entry looked at and the word read in one step: the page cannot
    // go between the two.
    let _step = OneStep::new(pml4_phys);
    let (_, pte) = entries_of(pml4_phys, virt);
    if pte & (PRESENT | USER) != PRESENT | USER {
        return None;
    }
    let _ua = crate::cpu::UserAccess::begin();
    Some(core::ptr::read_volatile(virt as *const u64))
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
        let done = if marker(pml4_phys, page as usize).is_some() {
            back(pml4_phys, page as usize, write, true)
        } else if write {
            // A page shared since a fork is the writer's own before the
            // kernel writes it, as it would be before the program did.
            own(pml4_phys, page as usize).map(|_| ())
        } else {
            Ok(())
        };
        match done {
            Ok(()) => page += PAGE_SIZE as u64,
            // No frame to give it with: wait for one, if waiting can
            // produce one, and look at the same page again.
            Err(Fault::NoMemory) if crate::reclaim::wait() => {}
            Err(fault) => return Err(fault),
        }
    }
    Ok(())
}}

/// Make the page at `virt` this address space's alone and writable, if it
/// is one shared since a `fork` and waiting to be copied. Whether it was.
///
/// If nobody else has the frame any more — the other side copied its own,
/// or is gone — the page is simply made writable again. Otherwise this
/// address space is given a copy and the frame is one fewer's. Nothing is
/// charged: the page was counted as this address space's when it came to
/// have it.
///
/// It is called for a write that faulted, from ring 3 or from the kernel;
/// before the kernel writes a program's memory where it can say so first
/// ([`back_range`]); and for another address space's page, where the kernel
/// is about to write it through its frame. Where the frame changes, the
/// entry is taken away and every processor with the address space loaded
/// has forgotten it before the copy is put in: break before make.
///
/// Looking at the entry, copying the page and changing the entry are one
/// step, with interrupts off. In two, another thread of the program run in
/// between could do the same: the frame would be given back twice, once
/// more than this address space had it, and the second time it is freed
/// from under whoever it is still shared with.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn own(pml4_phys: usize, virt: usize) -> Result<bool, Fault> { unsafe {
    let _step = OneStep::new(pml4_phys);
    own_one(pml4_phys, virt)
}}

unsafe fn own_one(pml4_phys: usize, virt: usize) -> Result<bool, Fault> { unsafe {
    if (virt as u64) < USER_MIN_ADDR || (virt as u64) >= USER_ADDR_LIMIT {
        return Ok(false);
    }
    let (_, _, _, pti) = table_indices(virt);
    let Some(pt) = leaf_table(pml4_phys, virt) else { return Ok(false) };
    let raw = pt.entries[pti].raw();
    if raw & PRESENT == 0 || raw & OWNED == 0 || raw & COPY_ON_WRITE == 0 {
        return Ok(false);
    }
    let frame = pt.entries[pti].frame_address();
    let flags = (raw & !ADDR_MASK & !COPY_ON_WRITE) | WRITABLE;
    if pmm::shared(frame) == 0 {
        pt.entries[pti].set(frame, flags);
    } else {
        let copy = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
        // The entry goes, and every processor with this address space loaded
        // forgets it, before the copy takes its place. Put straight in, the
        // copy and the frame it was made from were both in use at once: a
        // processor goes on using a translation it remembers until it is
        // told to forget it, whatever the table says by then, so another
        // thread's processor that remembered the old, read-only one found
        // the new one for a store and answered its next load from the old —
        // and the thread read back, from the frame its program no longer
        // had, an older value than it had just written. A missing entry is
        // a fault instead, and the fault waits for this to be done.
        pt.entries[pti] = PageTableEntry(0);
        crate::tlb::stale(pml4_phys);
        crate::tlb::sync();
        if pml4_phys == read_cr3() {
            invlpg(virt & !0xFFF);
        }
        core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
        pt.entries[pti].set(copy, flags);
        pmm::free(pmm::PhysFrame::from_address(frame));
    }
    if pml4_phys == read_cr3() {
        invlpg(virt & !0xFFF);
    }
    Ok(true)
}}

/// Make the page at `virt` this address space's alone, whether or not it
/// was waiting to be copied: for a page about to be given away
/// (`SYS_ADDRSPACE_GIVE`), which must not go on being somebody else's too.
///
/// # Safety
/// As [`own`].
pub unsafe fn unshare(pml4_phys: usize, virt: usize) -> Result<(), Fault> { unsafe {
    let _step = OneStep::new(pml4_phys);
    unshare_one(pml4_phys, virt)
}}

unsafe fn unshare_one(pml4_phys: usize, virt: usize) -> Result<(), Fault> { unsafe {
    if own_one(pml4_phys, virt)? {
        return Ok(());
    }
    let (_, _, _, pti) = table_indices(virt);
    let Some(pt) = leaf_table(pml4_phys, virt) else { return Ok(()) };
    let raw = pt.entries[pti].raw();
    let frame = pt.entries[pti].frame_address();
    if raw & PRESENT == 0 || raw & OWNED == 0 || pmm::shared(frame) == 0 {
        return Ok(());
    }
    // Shared for good, being read-only: a copy is this one's alone.
    let copy = crate::reclaim::frame().ok_or(Fault::NoMemory)?;
    core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
    pt.entries[pti].set(copy, raw & !ADDR_MASK);
    pmm::free(pmm::PhysFrame::from_address(frame));
    crate::tlb::stale(pml4_phys);
    if pml4_phys == read_cr3() {
        invlpg(virt & !0xFFF);
    }
    Ok(())
}}

/// Walk the pages of `pml4_phys` that are there, from `from` up to `to`,
/// for at most `budget` of them, calling `visit` with each one's address and
/// entry. Answers with where it stopped, or `None` if it reached the end.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped user PML4 table;
/// interrupts off.
unsafe fn sweep(
    pml4_phys: usize,
    from: usize,
    to: usize,
    budget: usize,
    mut visit: impl FnMut(usize, &mut PageTableEntry),
) -> Option<usize> { unsafe {
    let mut va = from.max(USER_MIN_ADDR as usize) & !0xFFF;
    let to = to.min(USER_ADDR_LIMIT as usize);
    let mut seen = 0;
    while va < to {
        let (pml4i, pdpti, pdi, pti) = table_indices(va);
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
        let e2 = table_at(e3.frame_address()).entries[pdi];
        if !e2.is_present() || e2.is_huge() {
            va = next_boundary(va, 1 << 21);
            continue;
        }
        let pt = table_at(e2.frame_address());
        for i in pti..512 {
            if va >= to {
                break;
            }
            if pt.entries[i].is_present() {
                if seen == budget {
                    return Some(va);
                }
                seen += 1;
                visit(va, &mut pt.entries[i]);
            }
            va += PAGE_SIZE;
        }
    }
    None
}}

/// Take the page at `va` of program `space`, whose entry is `e`, if it is
/// one that may be taken: what [`take_unused`] says of each kind. Whether
/// it was.
///
/// # Safety
/// As [`sweep`]; `e` is a present entry of that address space.
unsafe fn take(space: u64, word_page: usize, va: usize, e: &mut PageTableEntry) -> bool {
    let raw = e.raw();
    if raw & USER == 0 || va == word_page || crate::scheduler::pinned(space, va as u64) {
        return false;
    }
    let frame = e.frame_address();
    if raw & OWNED != 0 {
        if pmm::shared(frame) != 0 {
            return false;
        }
        let Some(page) = crate::memobj::swap_out(frame) else { return false };
        let slot = crate::memobj::swap_slot();
        // Shared since a fork and nobody else's any more, it is simply
        // writable again when it comes back.
        let write = raw & (WRITABLE | COPY_ON_WRITE) != 0;
        // A private copy of a file's page named the file; what replaces
        // it names where it went.
        crate::memobj::drop_entry(raw);
        crate::memobj::map_ref(slot, 1);
        *e = PageTableEntry(object_marker(slot, page, write, false, raw & NO_EXECUTE == 0));
        true
    } else {
        let slot = object_slot(raw);
        if slot == 0 || raw & WRITABLE != 0 {
            return false;
        }
        let Some(page) = crate::memobj::page_of(slot, frame) else { return false };
        // It names the same object as before: only the mapping goes.
        pmm::unmapped(frame);
        *e = PageTableEntry(object_marker(slot, page, false, false, raw & NO_EXECUTE == 0));
        true
    }
}

/// Take every page from `from` up to `to` that may be taken, used lately
/// or not — a program giving up its own (`SYS_PAGE_OUT`) — looking at no
/// more than `budget` pages that are there. Answers with where it stopped,
/// `None` at the end of the range, and how many were taken.
///
/// # Safety
/// As [`take_unused`].
pub unsafe fn take_range(
    pml4_phys: usize,
    space: u64,
    word_page: usize,
    from: usize,
    to: usize,
    budget: usize,
) -> (Option<usize>, usize) { unsafe {
    let mut taken = 0;
    let next = sweep(pml4_phys, from, to, budget, |va, e| {
        taken += take(space, word_page, va, e) as usize;
    });
    (next, taken)
}}

/// Take pages of program `space` that have not been used since this last
/// looked, to be given up: at most `want` of them, out of at most `budget`
/// looked at, from `from` upwards. Answers with where to go on from next
/// time (`None` at the end of the address space) and how many were taken.
///
/// "Used" is the processor's own mark on an entry, which is cleared here
/// as it is seen: a page is taken the second time round, if nothing has
/// touched it in between.
///
/// What a page is decides what taking it means:
///
/// - **A page of the program's own**, that no other program shares, is
///   moved into the cache of the object memory is written out to
///   (`memobj::swap_out`) and its entry becomes a reservation for it. The
///   frame is still there, and comes straight back if the page is touched
///   before its pager has written it and the cache has given it up.
/// - **A page of a file**, mapped to be read, becomes the reservation it was
///   before it was first touched. The frame is the cache's, which gives it
///   up when nothing maps it.
/// - **Everything else stays**: shared memory, a device, a page of a file
///   mapped to be written through (it is dirty for as long as it is
///   mapped), a page a fork left in two programs, a page the system call
///   some task of the program is in has checked (`scheduler::pinned`), and
///   the page the program is told of signals through (`word_page`), which
///   is written by its frame from wherever a signal is raised.
///
/// The caller tells the other processors (`tlb::stale`): entries have been
/// taken away, and marks cleared that a processor only sets again once it
/// has forgotten the page.
///
/// # Safety
/// As [`sweep`].
pub unsafe fn take_unused(
    pml4_phys: usize,
    space: u64,
    word_page: usize,
    from: usize,
    budget: usize,
    want: usize,
) -> (Option<usize>, usize) { unsafe {
    let mut taken = 0;
    let next = sweep(pml4_phys, from, USER_ADDR_LIMIT as usize, budget, |va, e| {
        let raw = e.raw();
        if raw & USER == 0 {
            return;
        }
        if raw & ACCESSED != 0 {
            *e = PageTableEntry(raw & !ACCESSED);
            return;
        }
        if taken < want {
            taken += take(space, word_page, va, e) as usize;
        }
    });
    (next, taken)
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
        let _step = OneStep::new(pml4_phys);
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
        let _step = OneStep::new(pml4_phys);
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
/// many pages of its own the address space had there, which is what it is
/// no longer charged for: a frame shared since a fork is given up, and goes
/// back to the allocator when the last to have it does.
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
        // A step a table: each looks from the top again, and what it
        // clears, frees and gives back is done before anything else runs.
        let _step = OneStep::new(pml4_phys);
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
                crate::tlb::stale(pml4_phys);
            } else if e.raw() != 0 {
                // A page that was written out is one of its own all the
                // same, and no longer charged for when it goes.
                if is_written_out(e.raw()) {
                    freed += 1;
                }
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
    let _step = OneStep::new(pml4_phys);
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

/// Whether the tables of `pml4_phys`, as they are now, let ring 3 do what
/// it has just faulted trying to do at `virt`: read it, or write it, or run
/// it.
///
/// A fault says what the tables were when the processor looked. By the time
/// the kernel is looking they may say something else: another thread of the
/// program, on another processor, touched the same new page a moment
/// sooner and has been given its memory; or was given leave to write where
/// this one's processor still remembered it could not. There is nothing to
/// do for such a fault but run the instruction again.
///
/// # Safety
/// `pml4_phys` must point to a valid, identity-mapped PML4 table.
pub unsafe fn permits(pml4_phys: usize, virt: usize, write: bool, exec: bool) -> bool { unsafe {
    if (virt as u64) < USER_MIN_ADDR || (virt as u64) >= USER_ADDR_LIMIT {
        return false;
    }
    let _step = OneStep::new(pml4_phys);
    let (pml4i, pdpti, pdi, pti) = table_indices(virt);
    // Every level has to agree: present, for ring 3, writable if it is a
    // write, and not marked as data if it is being run.
    let allows = |e: PageTableEntry| {
        e.is_present()
            && e.raw() & USER != 0
            && (!write || e.raw() & WRITABLE != 0)
            && (!exec || e.raw() & NO_EXECUTE == 0)
    };
    let e = table_at(pml4_phys).entries[pml4i];
    if !allows(e) {
        return false;
    }
    let e = table_at(e.frame_address()).entries[pdpti];
    if !allows(e) {
        return false;
    }
    if e.is_huge() {
        return true;
    }
    let e = table_at(e.frame_address()).entries[pdi];
    if !allows(e) {
        return false;
    }
    if e.is_huge() {
        return true;
    }
    allows(table_at(e.frame_address()).entries[pti])
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
    let _step = OneStep::new(pml4_phys);
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
    let _step = OneStep::new(pml4_phys);
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
    let _step = OneStep::new(pml4_phys);
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
    crate::tlb::stale(pml4_phys);

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
/// A page the source owns becomes a page both own: the same frame, counted
/// once more (`pmm::share`), and — if it could be written — writable by
/// neither until one of them does, at which point that one is given a copy
/// ([`own`]). A page that could not be written is shared for good. A page
/// the source does not own — shared memory, a device, a file's page — is
/// mapped at the same frame, because that is what sharing means and because
/// `OWNED` is what decides who may free a frame. A reservation is copied as
/// a reservation: the promise is inherited, and whoever touches the page
/// first gets a frame for it.
///
/// It was eager for a long time: every page copied, for a child whose first
/// act is nearly always to become another program and throw them away.
///
/// The source's own entries are changed — what was writable is not, until
/// it is written — so whoever calls this tells every processor that has
/// the source loaded (`tlb::stale`) and reloads its own.
///
/// Returns the pages charged to the child, or `None` if anything ran out. On
/// failure the caller destroys the half-built space, which gives back
/// exactly what this took: a table it allocated, and a count on each frame
/// it shared.
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
            let frame = src.entries[i].frame_address();
            if pmm::share(frame) {
                // Both have it. What could be written is to be copied by
                // whichever writes first; what could not is simply shared.
                let both = if raw & (WRITABLE | COPY_ON_WRITE) != 0 {
                    (raw & !WRITABLE) | COPY_ON_WRITE
                } else {
                    raw
                };
                src.entries[i] = PageTableEntry(both);
                dst.entries[i] = PageTableEntry(both);
            } else {
                // More sharers than the count can say: a copy of the
                // child's own, as every page once was.
                let copy = pmm::alloc()?.address();
                core::ptr::copy_nonoverlapping(frame as *const u8, copy as *mut u8, PAGE_SIZE);
                dst.entries[i].set(copy, raw & !ADDR_MASK);
            }
            // A private copy of a file's page still names the file, and the
            // child's entry is one more that does. It was not counted for a
            // long time, and was given back all the same when the child
            // went: an object lost a reference for every such page of every
            // child, and was called idle while its first mapper still had
            // pages of it to touch.
            crate::memobj::map_ref(object_slot(raw), 1);
            pages += 1;
            continue;
        }
        // Shared, or a device, or a page of an object, or a reservation: the
        // same entry, and one more reference to whatever it names.
        dst.entries[i] = src.entries[i];
        let slot = object_slot(raw);
        if slot != 0 {
            crate::memobj::map_ref(slot, 1);
            if raw & PRESENT != 0 {
                // One more mapping of the cache's frame.
                pmm::mapped(src.entries[i].frame_address());
            } else {
                // One more reservation for a page that was written out, if
                // that is what this is.
                crate::memobj::swap_ref(slot, (raw >> 12) & crate::memobj::MAX_PAGE);
                pages += is_written_out(raw) as usize;
            }
            if raw & PRESENT != 0 && raw & WRITABLE != 0 && raw & MARKER_SHARED != 0 {
                // A second shared writable mapping of the same page, which the
                // object counts so that it knows to write back.
                crate::memobj::mapped_writable(slot, (raw >> 12) & crate::memobj::MAX_PAGE);
            }
        }
    }
    Some(pages)
}}
