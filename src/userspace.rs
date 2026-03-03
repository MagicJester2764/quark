/// User-space support for the Quark microkernel.
///
/// Provides per-task address space creation, user page mapping,
/// and user-mode task launching.

use crate::{paging, pmm, scheduler, syscall};

const PAGE_SIZE: usize = 4096;

/// User-space address constants.
/// User code/data lives in the lower half (below 0x0000_8000_0000_0000).
pub const USER_STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
pub const USER_STACK_PAGES: usize = 4; // 16 KiB user stack
pub const USER_CODE_BASE: u64 = 0x0000_0000_0040_0000; // 4 MiB

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

    let current_pml4_phys = paging::read_cr3();

    unsafe {
        let current_pml4 = paging::table_at(current_pml4_phys);
        let new_pml4 = paging::table_at(new_pml4_phys);

        // Copy kernel mappings (upper half: entries 256–511)
        for i in 256..512 {
            new_pml4.entries[i] = current_pml4.entries[i];
        }

        // Also copy entry 0 (identity-mapped first 4 GiB from boot)
        // so kernel code can still access physical memory
        new_pml4.entries[0] = current_pml4.entries[0];
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
        task.context.rip = enter_user_trampoline as *const () as u64;
        // Store user entry and stack in callee-saved registers for the trampoline
        task.context.r12 = entry;
        task.context.r13 = stack_top;
        task.context.r14 = pml4 as u64;
    }

    Some(tid)
}

/// Trampoline that runs in kernel mode to set up and enter user mode.
fn enter_user_trampoline() {
    let entry: u64;
    let stack: u64;
    let pml4: u64;
    unsafe {
        core::arch::asm!(
            "",
            out("r12") entry,
            out("r13") stack,
            out("r14") pml4,
            options(nomem, nostack)
        );
    }

    // Switch to the user's address space
    unsafe {
        paging::write_cr3(pml4 as usize);
    }

    // Set up per-CPU kernel stack for syscall re-entry
    // Use the current kernel RSP as the kernel stack
    let kernel_rsp: u64;
    unsafe {
        core::arch::asm!("mov {}, rsp", out(reg) kernel_rsp, options(nomem, nostack));
        syscall::setup_percpu(kernel_rsp);
    }

    // Enter user mode
    unsafe {
        syscall::enter_usermode(entry, stack);
    }
}

fn idle_stub() {
    loop {
        core::hint::spin_loop();
    }
}
