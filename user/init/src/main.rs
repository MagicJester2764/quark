#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::syscall;

const PAGE_SIZE: usize = 4096;
const BOOT_INFO_ADDR: usize = 0x80_4000_0000;
const FILE_BUF_BASE: usize = 0x82_0000_0000;
const ROOTFS_BASE: usize = 0x85_0000_0000;

// ---------------------------------------------------------------------------
// Boot info structures (matches kernel's BootInfo)
// ---------------------------------------------------------------------------

#[repr(C)]
struct BootInfo {
    module_count: u64,
    fb_addr: u64,
    fb_pitch: u32,
    fb_width: u32,
    fb_height: u32,
    fb_bpp: u8,
    fb_type: u8,
    fb_red_pos: u8,
    fb_green_pos: u8,
    fb_blue_pos: u8,
    _pad: [u8; 3],
    modules: [BootModuleDesc; 32],
}

#[repr(C)]
struct BootModuleDesc {
    phys_start: u64,
    phys_end: u64,
    name: [u8; 48],
}

// ---------------------------------------------------------------------------
// ELF64 structures
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Minimal FAT32 reader (read-only, root directory only)
// ---------------------------------------------------------------------------

struct Bpb {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_size_32: u32,
    root_cluster: u32,
}

fn parse_bpb(data: &[u8]) -> Bpb {
    Bpb {
        bytes_per_sector: read_u16(data, 11) as u32,
        sectors_per_cluster: data[13] as u32,
        reserved_sectors: read_u16(data, 14) as u32,
        num_fats: data[16] as u32,
        fat_size_32: read_u32(data, 36),
        root_cluster: read_u32(data, 44),
    }
}

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn fat_offset(bpb: &Bpb) -> usize {
    (bpb.reserved_sectors * bpb.bytes_per_sector) as usize
}

fn data_region_offset(bpb: &Bpb) -> usize {
    ((bpb.reserved_sectors + bpb.num_fats * bpb.fat_size_32) * bpb.bytes_per_sector) as usize
}

fn cluster_data_offset(bpb: &Bpb, cluster: u32) -> usize {
    data_region_offset(bpb)
        + ((cluster - 2) as usize)
            * (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize
}

fn fat_next(rootfs: &[u8], bpb: &Bpb, cluster: u32) -> Option<u32> {
    let off = fat_offset(bpb) + (cluster as usize) * 4;
    let next = read_u32(rootfs, off) & 0x0FFF_FFFF;
    if next >= 0x0FFF_FFF8 {
        None
    } else {
        Some(next)
    }
}

const MAX_DIR_ENTRIES: usize = 32;

struct RootDirEntry {
    name: [u8; 11],
    first_cluster: u32,
    file_size: u32,
}

fn scan_root_dir(rootfs: &[u8], bpb: &Bpb) -> ([RootDirEntry; MAX_DIR_ENTRIES], usize) {
    let mut entries: [RootDirEntry; MAX_DIR_ENTRIES] = unsafe { core::mem::zeroed() };
    let mut count = 0;
    let cluster_bytes = (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize;
    let mut cluster = bpb.root_cluster;

    loop {
        let base = cluster_data_offset(bpb, cluster);
        let num_entries = cluster_bytes / 32;

        for i in 0..num_entries {
            if count >= MAX_DIR_ENTRIES {
                return (entries, count);
            }

            let off = base + i * 32;
            let first_byte = rootfs[off];

            // End of directory
            if first_byte == 0x00 {
                return (entries, count);
            }
            // Deleted entry
            if first_byte == 0xE5 {
                continue;
            }

            let attr = rootfs[off + 11];
            if attr & 0x0F == 0x0F { continue; } // LFN
            if attr & 0x08 != 0 { continue; }     // volume label
            if attr & 0x10 != 0 { continue; }     // subdirectory

            let mut name = [0u8; 11];
            name.copy_from_slice(&rootfs[off..off + 11]);

            let cluster_hi = read_u16(rootfs, off + 20) as u32;
            let cluster_lo = read_u16(rootfs, off + 26) as u32;

            entries[count] = RootDirEntry {
                name,
                first_cluster: (cluster_hi << 16) | cluster_lo,
                file_size: read_u32(rootfs, off + 28),
            };
            count += 1;
        }

        match fat_next(rootfs, bpb, cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }

    (entries, count)
}

// ---------------------------------------------------------------------------
// File reading: assemble file data from cluster chain into contiguous buffer
// ---------------------------------------------------------------------------

fn read_file_to_buffer<'a>(
    rootfs: &[u8],
    bpb: &Bpb,
    first_cluster: u32,
    file_size: u32,
) -> Result<&'a [u8], ()> {
    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Allocate physical pages and map at FILE_BUF_BASE
    for p in 0..pages_needed {
        let frame = syscall::sys_phys_alloc(1)?;
        syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1)?;
    }

    let cluster_bytes = (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize;
    let mut cluster = first_cluster;
    let mut copied = 0usize;

    while copied < size {
        let src_off = cluster_data_offset(bpb, cluster);
        let chunk = cluster_bytes.min(size - copied);

        unsafe {
            core::ptr::copy_nonoverlapping(
                rootfs.as_ptr().add(src_off),
                (FILE_BUF_BASE + copied) as *mut u8,
                chunk,
            );
        }

        copied += chunk;

        if copied < size {
            match fat_next(rootfs, bpb, cluster) {
                Some(next) => cluster = next,
                None => break,
            }
        }
    }

    Ok(unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) })
}

// ---------------------------------------------------------------------------
// ELF spawning (takes pre-mapped byte slice)
// ---------------------------------------------------------------------------

fn spawn_elf(elf_data: &[u8]) -> Result<usize, ()> {
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
        let temp_page: usize = 0x84_0000_0000 + p * PAGE_SIZE;
        syscall::sys_map_phys(frame, temp_page, 1)?;
        unsafe {
            core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE);
        }
        syscall::sys_addrspace_map(cr3, stack_bottom + p * PAGE_SIZE, frame, 1, 1)?;
    }

    // Start the task
    syscall::sys_task_start(tid, entry, stack_top as u64, cr3)?;

    Ok(tid)
}

// ---------------------------------------------------------------------------
// Rootfs loading
// ---------------------------------------------------------------------------

fn load_from_rootfs(rootfs_phys: usize, rootfs_size: usize) {
    syscall::sys_write(b"[init] Mounting FAT32 rootfs\n");

    // Map the entire rootfs image
    let rootfs_pages = (rootfs_size + PAGE_SIZE - 1) / PAGE_SIZE;
    if syscall::sys_map_phys(rootfs_phys, ROOTFS_BASE, rootfs_pages).is_err() {
        syscall::sys_write(b"[init] Failed to map rootfs\n");
        return;
    }

    let rootfs = unsafe { core::slice::from_raw_parts(ROOTFS_BASE as *const u8, rootfs_size) };
    let bpb = parse_bpb(rootfs);
    let (entries, count) = scan_root_dir(rootfs, &bpb);

    syscall::sys_write(b"[init] Files on rootfs: ");
    print_dec(count);
    syscall::sys_write(b"\n");

    // Pass 1: find and spawn NAMESRVR.ELF first (guarantees TID 2)
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"NAMESRVR" && &e.name[8..11] == b"ELF" {
            syscall::sys_write(b"[init] Loading NAMESRVR.ELF\n");
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn_elf(data) {
                    Ok(tid) => {
                        syscall::sys_write(b"[init]   Spawned TID ");
                        print_dec(tid);
                        syscall::sys_write(b"\n");
                    }
                    Err(()) => {
                        syscall::sys_write(b"[init]   FAILED to spawn\n");
                    }
                }
            }
            break;
        }
    }

    // Pass 2: spawn CONSOLE.ELF (needs framebuffer info from boot info)
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..7] == b"CONSOLE" && &e.name[8..11] == b"ELF" {
            syscall::sys_write(b"[init] Loading CONSOLE.ELF\n");
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn_elf(data) {
                    Ok(tid) => {
                        // Grant MAP_PHYS so console can map the framebuffer
                        let _ = syscall::sys_grant_cap(tid, syscall::CAP_MAP_PHYS);
                        // Send framebuffer info via IPC
                        send_fb_info(tid);
                        syscall::sys_write(b"[init]   Console spawned TID ");
                        print_dec(tid);
                        syscall::sys_write(b"\n");
                    }
                    Err(()) => {
                        syscall::sys_write(b"[init]   FAILED to spawn console\n");
                    }
                }
            }
            break;
        }
    }

    // Pass 3: spawn remaining .ELF files
    for i in 0..count {
        let e = &entries[i];

        // Skip nameserver and console (already spawned)
        if &e.name[0..8] == b"NAMESRVR" {
            continue;
        }
        if &e.name[0..7] == b"CONSOLE" && &e.name[8..11] == b"ELF" {
            continue;
        }

        // Only spawn .ELF files
        if &e.name[8..11] != b"ELF" {
            continue;
        }

        syscall::sys_write(b"[init] Loading ");
        print_fat_name(&e.name);
        syscall::sys_write(b"\n");

        if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
            match spawn_elf(data) {
                Ok(tid) => {
                    // Grant I/O port and IRQ capabilities to keyboard driver
                    if &e.name[0..8] == b"KEYBOARD" {
                        let _ = syscall::sys_grant_ioport(tid);
                        let _ = syscall::sys_grant_irq(tid, 1);
                    }
                    syscall::sys_write(b"[init]   Spawned TID ");
                    print_dec(tid);
                    syscall::sys_write(b"\n");
                }
                Err(()) => {
                    syscall::sys_write(b"[init]   FAILED to spawn\n");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    syscall::sys_write(b"[init] Starting init process.\n");

    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    let mod_count = info.module_count as usize;

    // Find rootfs module
    let mut found = false;
    for i in 0..mod_count {
        let m = &info.modules[i];
        let name = module_name(&m.name);
        if starts_with(name, b"rootfs") {
            let phys = m.phys_start as usize;
            let size = (m.phys_end - m.phys_start) as usize;
            load_from_rootfs(phys, size);
            found = true;
            break;
        }
    }

    if !found {
        syscall::sys_write(b"[init] ERROR: rootfs module not found!\n");
    }

    syscall::sys_write(b"[init] All programs loaded. Entering idle loop.\n");

    loop {
        syscall::sys_yield();
    }
}

// ---------------------------------------------------------------------------
// Framebuffer info handoff to console server
// ---------------------------------------------------------------------------

fn send_fb_info(console_tid: usize) {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };

    // Pack framebuffer info into one IPC message
    // data[0] = physical address
    // data[1] = (width << 32) | height
    // data[2] = (pitch << 32) | bpp
    // data[3] = (red_pos << 16) | (green_pos << 8) | blue_pos
    let msg = Message {
        sender: 0,
        tag: 100, // TAG_FB_INIT
        data: [
            info.fb_addr,
            ((info.fb_width as u64) << 32) | (info.fb_height as u64),
            ((info.fb_pitch as u64) << 32) | (info.fb_bpp as u64),
            ((info.fb_red_pos as u64) << 16) | ((info.fb_green_pos as u64) << 8) | (info.fb_blue_pos as u64),
            0,
            0,
        ],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(console_tid, &msg, &mut reply).is_err() {
        syscall::sys_write(b"[init] Failed to send FB info to console\n");
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn print_fat_name(name: &[u8; 11]) {
    // Print base name (trim trailing spaces)
    let base_len = name[0..8]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    syscall::sys_write(&name[..base_len]);
    let ext_len = name[8..11]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    if ext_len > 0 {
        syscall::sys_write(b".");
        syscall::sys_write(&name[8..8 + ext_len]);
    }
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
