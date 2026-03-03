#![no_std]
#![no_main]

use libquark::syscall;

const PAGE_SIZE: usize = 4096;
const BOOT_INFO_ADDR: usize = 0x80_4000_0000;

/// Boot info page layout — matches kernel's BootInfo struct.
#[repr(C)]
struct BootInfo {
    module_count: u64,
    modules: [BootModuleDesc; 32],
}

#[repr(C)]
struct BootModuleDesc {
    phys_start: u64,
    phys_end: u64,
    name: [u8; 48],
}

/// ELF64 header (minimal fields we need).
#[repr(C, packed)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
}

#[repr(C, packed)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

const PT_LOAD: u32 = 1;
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    syscall::sys_write(b"[init] Starting init process.\n");

    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    let mod_count = info.module_count as usize;

    syscall::sys_write(b"[init] Boot modules: ");
    print_dec(mod_count);
    syscall::sys_write(b"\n");

    // Pass 1: Spawn nameserver first so it gets a well-known TID (2)
    for i in 0..mod_count {
        let m = &info.modules[i];
        let name = module_name(&m.name);
        if starts_with(name, b"nameserver") {
            try_spawn(m, name);
            break;
        }
    }

    // Pass 2: Spawn remaining modules (skip init and nameserver)
    for i in 0..mod_count {
        let m = &info.modules[i];
        let name = module_name(&m.name);

        if starts_with(name, b"init") || starts_with(name, b"nameserver") {
            continue;
        }

        try_spawn(m, name);
    }

    syscall::sys_write(b"[init] All modules loaded. Entering idle loop.\n");

    // Idle yield loop
    loop {
        syscall::sys_yield();
    }
}

/// Check if a module is a valid ELF, spawn it, and grant capabilities as needed.
fn try_spawn(m: &BootModuleDesc, name: &[u8]) {
    let phys_start = m.phys_start as usize;
    let mod_size = (m.phys_end - m.phys_start) as usize;
    if mod_size < 4 {
        return;
    }
    // Quick ELF magic check via first 4 bytes mapped temporarily
    let temp_check: usize = 0x81_0000_0000;
    if syscall::sys_map_phys(phys_start, temp_check, 1).is_err() {
        return;
    }
    let magic = unsafe { core::slice::from_raw_parts(temp_check as *const u8, 4) };
    if magic != ELF_MAGIC {
        return;
    }

    syscall::sys_write(b"[init] Loading module: ");
    syscall::sys_write(name);
    syscall::sys_write(b"\n");

    match spawn_module(m) {
        Ok(tid) => {
            // Grant I/O port and IRQ capabilities to keyboard driver
            if starts_with(name, b"keyboard") {
                let _ = syscall::sys_grant_ioport(tid);
                let _ = syscall::sys_grant_irq(tid, 1);
            }
        }
        Err(()) => {
            syscall::sys_write(b"[init]   FAILED to spawn module.\n");
        }
    }
}

/// Spawn a user-space task from an ELF boot module.
fn spawn_module(m: &BootModuleDesc) -> Result<usize, ()> {
    let phys_start = m.phys_start as usize;
    let phys_end = m.phys_end as usize;
    let mod_size = phys_end - phys_start;
    let mod_pages = (mod_size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Map the module's physical pages into init's address space temporarily
    // Use an address above 4 GiB so it doesn't conflict with identity mapping
    let temp_base: usize = 0x82_0000_0000;
    syscall::sys_map_phys(phys_start, temp_base, mod_pages)?;

    let elf_data = unsafe { core::slice::from_raw_parts(temp_base as *const u8, mod_size) };

    // Validate ELF magic
    if elf_data.len() < 64 || elf_data[0..4] != ELF_MAGIC {
        return Err(());
    }

    let hdr = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    let entry = hdr.e_entry;
    let phoff = hdr.e_phoff as usize;
    let phentsize = hdr.e_phentsize as usize;
    let phnum = hdr.e_phnum as usize;

    // Create new address space and task
    let cr3 = syscall::sys_addrspace_create()?;
    let tid = syscall::sys_task_create()?;

    // Load PT_LOAD segments
    for i in 0..phnum {
        let offset = phoff + i * phentsize;
        if offset + phentsize > elf_data.len() {
            break;
        }
        let phdr = unsafe { &*(elf_data.as_ptr().add(offset) as *const Elf64Phdr) };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;
        let file_offset = phdr.p_offset as usize;
        let writable = phdr.p_flags & 2 != 0;

        let vaddr_page_start = vaddr & !0xFFF;
        let vaddr_end = vaddr + memsz;
        let pages = (vaddr_end - vaddr_page_start + PAGE_SIZE - 1) / PAGE_SIZE;

        let file_start = vaddr;
        let file_end = vaddr + filesz;

        for p in 0..pages {
            let page_vaddr = vaddr_page_start + p * PAGE_SIZE;

            // Allocate a physical frame
            let frame = syscall::sys_phys_alloc(1)?;

            // Map into init's space temporarily to copy data
            let temp_page: usize = 0x83_0000_0000 + p * PAGE_SIZE;
            syscall::sys_map_phys(frame, temp_page, 1)?;

            // Zero the page
            unsafe {
                core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE);
            }

            // Copy file data
            let page_end = page_vaddr + PAGE_SIZE;
            if file_start < page_end && file_end > page_vaddr {
                let copy_vstart = file_start.max(page_vaddr);
                let copy_vend = file_end.min(page_end);
                let copy_len = copy_vend - copy_vstart;
                let dst_offset = copy_vstart - page_vaddr;
                let src_offset = file_offset + (copy_vstart - vaddr);

                if src_offset + copy_len <= elf_data.len() {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            elf_data.as_ptr().add(src_offset),
                            (temp_page + dst_offset) as *mut u8,
                            copy_len,
                        );
                    }
                }
            }

            // Map frame into the new task's address space
            let flags: u64 = if writable { 1 } else { 0 };
            syscall::sys_addrspace_map(cr3, page_vaddr, frame, 1, flags)?;
        }
    }

    // Set up user stack (4 pages at 0x7FFF_FFFF_F000)
    let stack_top: usize = 0x7FFF_FFFF_F000;
    let stack_pages: usize = 4;
    let stack_bottom = stack_top - stack_pages * PAGE_SIZE;
    for p in 0..stack_pages {
        let frame = syscall::sys_phys_alloc(1)?;
        // Zero the stack frame
        let temp_page: usize = 0x84_0000_0000 + p * PAGE_SIZE;
        syscall::sys_map_phys(frame, temp_page, 1)?;
        unsafe {
            core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE);
        }
        syscall::sys_addrspace_map(cr3, stack_bottom + p * PAGE_SIZE, frame, 1, 1)?;
    }

    // Start the task
    syscall::sys_task_start(tid, entry, stack_top as u64, cr3)?;

    syscall::sys_write(b"[init]   Spawned TID ");
    print_dec(tid);
    syscall::sys_write(b"\n");

    Ok(tid)
}

fn module_name(name: &[u8; 48]) -> &[u8] {
    let len = name.iter().position(|&b| b == 0).unwrap_or(48);
    &name[..len]
}

fn starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack[..needle.len()] == *needle
}

fn print_dec(val: usize) {
    if val == 0 {
        syscall::sys_write(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    syscall::sys_write(&out[..i]);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    syscall::sys_write(b"[init] PANIC!\n");
    loop {
        core::hint::spin_loop();
    }
}
