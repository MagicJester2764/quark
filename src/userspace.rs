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
pub const USER_CODE_BASE: u64 = 0x0000_0080_0000_0000; // 512 GiB (PML4[1])

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

            let new_pdpt_phys = pmm::alloc()?.address();
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

    Some(new_pml4_phys)
}

/// Map a page in a user address space with USER flag.
pub fn map_user_page(
    pml4_phys: usize,
    virt: usize,
    phys: usize,
    writable: bool,
) -> Result<(), paging::PagingError> {
    let mut flags = paging::PRESENT | paging::USER;
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
    Some(USER_STACK_TOP)
}

/// Load user code bytes into a user address space at USER_CODE_BASE.
/// Returns the entry point address.
pub fn load_user_code(pml4_phys: usize, code: &[u8]) -> Option<u64> {
    let pages_needed = (code.len() + PAGE_SIZE - 1) / PAGE_SIZE;
    for i in 0..pages_needed {
        let frame = pmm::alloc()?;
        let virt = USER_CODE_BASE as usize + i * PAGE_SIZE;
        map_user_page(pml4_phys, virt, frame.address(), false).ok()?;

        // Copy code to the physical frame (identity-mapped)
        let offset = i * PAGE_SIZE;
        let remaining = code.len() - offset;
        let copy_len = remaining.min(PAGE_SIZE);
        unsafe {
            core::ptr::copy_nonoverlapping(
                code.as_ptr().add(offset),
                frame.address() as *mut u8,
                copy_len,
            );
            // Zero remainder of page
            if copy_len < PAGE_SIZE {
                core::ptr::write_bytes(
                    (frame.address() + copy_len) as *mut u8,
                    0,
                    PAGE_SIZE - copy_len,
                );
            }
        }
    }
    Some(USER_CODE_BASE)
}

/// Spawn a user-mode task from raw code bytes.
/// Creates an address space, loads code, sets up stack, and enters ring 3.
pub fn spawn_user_task(code: &[u8]) -> Option<usize> {
    let pml4 = create_address_space()?;
    let entry = load_user_code(pml4, code)?;
    let stack_top = setup_user_stack(pml4)?;

    // Spawn a kernel task that will transition to user mode
    let tid = scheduler::spawn(idle_stub);

    // Patch the task to use the new address space and jump to usermode
    unsafe {
        let task = scheduler::get_task_mut(tid)?;
        task.cr3 = pml4;
        task.context.rip = enter_user_trampoline as *const () as u64;
        // Store user entry and stack in callee-saved registers for the trampoline
        task.context.r12 = entry;
        task.context.r13 = stack_top;
        task.context.r14 = pml4 as u64;
    }

    Some(tid)
}

/// Naked trampoline stub: moves r12/r13/r14 into argument registers
/// and calls enter_user_inner.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_user_trampoline() {
    core::arch::naked_asm!(
        "mov rdi, r12",  // entry
        "mov rsi, r13",  // stack
        "mov rdx, r14",  // pml4
        "call {inner}",
        inner = sym enter_user_inner,
    );
}

/// Inner function called by the trampoline with proper C ABI args.
fn enter_user_inner(entry: u64, stack: u64, pml4: u64) {
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
        syscall::enter_usermode(entry, stack);
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
