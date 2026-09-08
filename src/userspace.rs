/// User-space support for the Quark microkernel.
///
/// Provides per-task address space creation, user page mapping,
/// and user-mode task launching.

use crate::{elf, paging, pmm, scheduler, syscall};

const PAGE_SIZE: usize = 4096;

/// User-space address constants.
/// User code/data lives in the lower half (below 0x0000_8000_0000_0000).
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const USER_STACK_PAGES: usize = 4; // 16 KiB user stack
/// Highest address a user segment may occupy (exclusive).
pub const USER_ADDR_LIMIT: u64 = paging::USER_ADDR_LIMIT;

/// Registry of user address spaces and the task that created each one.
///
/// `sys_addrspace_map` and `sys_task_start` take a CR3 straight from user
/// space. Without this, CAP_TASK_MGMT let a task reinterpret *any* physical
/// address as a PML4 and have the kernel walk and write to it.
///
/// The third field counts the tasks currently running in the address space.
/// Threads are tasks that share one, so it cannot be destroyed when the first
/// of them exits — only when the last does.
const MAX_ADDRESS_SPACES: usize = crate::task::MAX_TASKS * 2;
static mut ADDRESS_SPACES: [(usize, usize, u32); MAX_ADDRESS_SPACES] =
    [(0, 0, 0); MAX_ADDRESS_SPACES];

/// Record `cr3` as an address space created by `owner`.
/// Returns false if the registry is full.
fn register_address_space(cr3: usize, owner: usize) -> bool {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(ADDRESS_SPACES);
        for slot in table.iter_mut() {
            if slot.0 == 0 {
                *slot = (cr3, owner, 0);
                return true;
            }
        }
    }
    false
}

/// Drop `cr3` from the registry (called when the address space is destroyed).
pub fn unregister_address_space(cr3: usize) {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(ADDRESS_SPACES);
        for slot in table.iter_mut() {
            if slot.0 == cr3 {
                *slot = (0, 0, 0);
            }
        }
    }
}

/// True if `cr3` is an address space that `tid` created.
pub fn is_owned_address_space(tid: usize, cr3: usize) -> bool {
    if cr3 == 0 {
        return false;
    }
    unsafe {
        let table = &*core::ptr::addr_of!(ADDRESS_SPACES);
        table.iter().any(|&(c, owner, _)| c == cr3 && owner == tid)
    }
}

/// Whether `tid` may direct a task into `cr3`.
///
/// The creator may, which is the ordinary case of spawning a child. So may a
/// task already executing there: that is how a thread starts another thread in
/// the address space they share, without being the one that created it.
pub fn may_use_address_space(tid: usize, cr3: usize) -> bool {
    if cr3 == 0 {
        return false;
    }
    if is_owned_address_space(tid, cr3) {
        return true;
    }
    unsafe { crate::scheduler::get_task_mut(tid).map(|t| t.cr3) == Some(cr3) }
}

/// Note that another task is now running in `cr3`.
pub fn addrspace_ref(cr3: usize) {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(ADDRESS_SPACES);
        for slot in table.iter_mut() {
            if slot.0 == cr3 {
                slot.2 = slot.2.saturating_add(1);
                return;
            }
        }
    }
}

/// Note that a task has stopped running in `cr3`.
///
/// Returns true when that was the last one, meaning the caller should destroy
/// it. An address space the registry does not know about — the kernel's own —
/// reports false, so nothing tries to tear it down.
pub fn addrspace_unref(cr3: usize) -> bool {
    unsafe {
        let table = &mut *core::ptr::addr_of_mut!(ADDRESS_SPACES);
        for slot in table.iter_mut() {
            if slot.0 == cr3 {
                slot.2 = slot.2.saturating_sub(1);
                return slot.2 == 0;
            }
        }
    }
    false
}

/// Create a new user address space.
///
/// Allocates a fresh PML4 and copies kernel mappings (upper-half entries,
/// PML4 indices 256–511) from the current address space.
///
/// Returns the physical address of the new PML4.
pub fn create_address_space() -> Option<usize> {
    let frame = pmm::alloc()?;
    let new_pml4_phys = frame.address();

    // Zero the new PML4
    unsafe {
        core::ptr::write_bytes(new_pml4_phys as *mut u8, 0, PAGE_SIZE);
    }

    // Always copy from the kernel's page tables (not the current user's)
    // so new address spaces get a clean identity mapping without any
    // user-space page table entries from the caller.
    let kernel_pml4_phys = paging::kernel_cr3();

    unsafe {
        let kernel_pml4 = paging::table_at(kernel_pml4_phys);
        let new_pml4 = paging::table_at(new_pml4_phys);

        // Copy kernel mappings (upper half: entries 256–511)
        for i in 256..512 {
            new_pml4.entries[i] = kernel_pml4.entries[i];
        }

        // Deep-copy the PDPT for PML4[0] so user-space page tables
        // (PDPT entries beyond the identity mapping) can be added
        // per-address-space without modifying the kernel's shared PDPT.
        // The PDs themselves are shared — user virtual addresses live
        // above 4 GiB (PDPT[4+]) so the identity-mapped PDs (PDPT[0-3])
        // are never modified.
        if kernel_pml4.entries[0].is_present() {
            let kernel_pdpt_phys = kernel_pml4.entries[0].frame_address();
            let kernel_pdpt = paging::table_at(kernel_pdpt_phys);

            let new_pdpt_phys = match pmm::alloc() {
                Some(f) => f.address(),
                None => {
                    // Don't leak the PML4 we just took.
                    pmm::free(pmm::PhysFrame::from_address(new_pml4_phys));
                    return None;
                }
            };
            core::ptr::write_bytes(new_pdpt_phys as *mut u8, 0, PAGE_SIZE);
            let new_pdpt = paging::table_at(new_pdpt_phys);

            // Copy identity mapping entries (shared PDs, no deep copy needed)
            for i in 0..512 {
                if kernel_pdpt.entries[i].is_present() {
                    new_pdpt.entries[i] = kernel_pdpt.entries[i];
                }
            }

            new_pml4.entries[0].set(
                new_pdpt_phys,
                kernel_pml4.entries[0].flags(),
            );
        }
    }

    if !register_address_space(new_pml4_phys, scheduler::current_tid()) {
        unsafe { paging::destroy_address_space(new_pml4_phys) };
        return None;
    }

    Some(new_pml4_phys)
}

/// Map a page in a user address space with USER flag.
pub fn map_user_page(
    pml4_phys: usize,
    virt: usize,
    phys: usize,
    writable: bool,
) -> Result<(), paging::PagingError> {
    let mut flags = paging::PRESENT | paging::USER | paging::OWNED;
    if writable {
        flags |= paging::WRITABLE;
    }
    unsafe { paging::map_page(pml4_phys, virt, phys, flags) }
}

/// Allocate and map a user stack. Returns the top of the stack (for RSP).
pub fn setup_user_stack(pml4_phys: usize) -> Option<u64> {
    let stack_bottom = USER_STACK_TOP as usize - (USER_STACK_PAGES * PAGE_SIZE);
    for i in 0..USER_STACK_PAGES {
        let frame = pmm::alloc()?;
        let virt = stack_bottom + i * PAGE_SIZE;
        map_user_page(pml4_phys, virt, frame.address(), true).ok()?;
        // Zero the page
        unsafe {
            // Switch to the new address space temporarily to zero
            // Actually, since we have identity mapping, we can zero
            // the physical frame directly
            core::ptr::write_bytes(frame.address() as *mut u8, 0, PAGE_SIZE);
        }
    }
    // Subtract 8 so RSP ≡ 8 mod 16 on entry to _start.
    // The x86_64 ABI requires RSP+8 be 16-byte aligned at function entry
    // (as if a `call` had just pushed a return address). Since iretq sets
    // RSP directly (no push), we pre-bias it here.
    Some(USER_STACK_TOP - 8)
}

/// Naked trampoline stub: moves r12/r13/r14 into argument registers
/// and calls enter_user_inner.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user_trampoline() {
    core::arch::naked_asm!(
        "mov rdi, r12",  // entry
        "mov rsi, r13",  // stack
        "mov rdx, r14",  // pml4
        "mov rcx, r15",  // value handed to the entry point
        "call {inner}",
        inner = sym enter_user_inner,
    );
}

/// Inner function called by the trampoline with proper C ABI args.
fn enter_user_inner(entry: u64, stack: u64, pml4: u64, arg: u64) {
    // Switch to the user's address space
    unsafe {
        paging::write_cr3(pml4 as usize);
    }

    // Set up per-CPU kernel stack for syscall re-entry and TSS RSP0
    // for hardware exception handling from ring 3
    let kernel_rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) kernel_rsp, options(nomem, nostack));
        syscall::setup_percpu(kernel_rsp);
        crate::idt::update_tss_rsp0(kernel_rsp);
    }

    // Enter user mode
    unsafe {
        syscall::enter_usermode(entry, stack, arg);
    }
}

/// Boot info page layout at BOOT_INFO_ADDR in init's address space.
/// Contains information about boot modules and framebuffer so init can load them.
#[repr(C)]
pub struct BootInfo {
    pub module_count: u64,
    pub fb_addr: u64,
    pub fb_pitch: u32,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_bpp: u8,
    pub fb_type: u8,
    pub fb_red_pos: u8,
    pub fb_green_pos: u8,
    pub fb_blue_pos: u8,
    _pad: [u8; 3],
    pub modules: [BootModuleDesc; 32],
}

/// Descriptor for a single boot module.
#[repr(C)]
pub struct BootModuleDesc {
    pub phys_start: u64,
    pub phys_end: u64,
    pub name: [u8; 48],
}

/// Boot info page address in user space (above 4 GiB identity mapping).
pub const BOOT_INFO_ADDR: usize = 0x80_4000_0000;

/// Spawn the init process from ELF data (bootstrap only).
///
/// Loads the ELF, creates a task with CAP_ALL, and maps a boot info page
/// at BOOT_INFO_ADDR containing module descriptors and framebuffer info.
pub fn spawn_init(elf_data: &[u8], fb: Option<crate::multiboot2::FramebufferInfo>) -> Option<usize> {
    let (pml4, entry, stack_top) = elf::load_elf(elf_data).ok()?;

    // Spawn a kernel task that will transition to user mode
    let tid = scheduler::spawn(idle_stub);

    // Patch the task
    unsafe {
        let task = scheduler::get_task_mut(tid)?;
        task.cr3 = pml4;
        task.caps = crate::task::CAP_ALL;
        crate::cap::populate_from_bitmask(&mut task.cspace, crate::task::CAP_ALL);
        task.context.rip = enter_user_trampoline as *const () as u64;
        task.context.r12 = entry;
        task.context.r13 = stack_top;
        task.context.r14 = pml4 as u64;
        task.context.r15 = 0;
    }

    // Map boot info page at BOOT_INFO_ADDR
    let info_frame = pmm::alloc()?;
    map_user_page(pml4, BOOT_INFO_ADDR, info_frame.address(), true).ok()?;

    // Fill in boot info
    unsafe {
        let info = info_frame.address() as *mut BootInfo;
        core::ptr::write_bytes(info, 0, 1);

        if let Some(ref fbi) = fb {
            (*info).fb_addr = fbi.addr;
            (*info).fb_pitch = fbi.pitch;
            (*info).fb_width = fbi.width;
            (*info).fb_height = fbi.height;
            (*info).fb_bpp = fbi.bpp;
            (*info).fb_type = fbi.fb_type;
            (*info).fb_red_pos = fbi.red_pos;
            (*info).fb_green_pos = fbi.green_pos;
            (*info).fb_blue_pos = fbi.blue_pos;
        }

        let mod_count = crate::modules::count();
        (*info).module_count = mod_count as u64;

        for i in 0..mod_count.min(32) {
            if let Some(m) = crate::modules::get(i) {
                (*info).modules[i].phys_start = m.start as u64;
                (*info).modules[i].phys_end = m.end as u64;
                // Copy module name
                let name_len = m.name.iter().position(|&b| b == 0).unwrap_or(m.name.len());
                let copy_len = name_len.min(48);
                core::ptr::copy_nonoverlapping(
                    m.name.as_ptr(),
                    (*info).modules[i].name.as_mut_ptr(),
                    copy_len,
                );
            }
        }
    }

    Some(tid)
}

fn idle_stub() {
    loop {
        core::hint::spin_loop();
    }
}
