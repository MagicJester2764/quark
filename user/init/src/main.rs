#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{println, syscall, vfs};

const PAGE_SIZE: usize = 4096;
const BOOT_INFO_ADDR: usize = 0x80_4000_0000;
const FILE_BUF_BASE: usize = 0x82_0000_0000;
const BOOT_IMG_BASE: usize = 0x85_0000_0000;
const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;

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

struct SpawnInfo {
    tid: usize,
    entry: u64,
    stack_top: u64,
    cr3: usize,
}

impl SpawnInfo {
    fn start(&self) -> Result<(), ()> {
        syscall::sys_task_start(self.tid, self.entry, self.stack_top, self.cr3)
    }
}

/// Load an ELF into a new task but do NOT start it.
/// Call info.start() after wiring fds / granting caps.
fn load_elf(elf_data: &[u8]) -> Result<SpawnInfo, ()> {
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

    Ok(SpawnInfo { tid, entry, stack_top: stack_top as u64, cr3 })
}

// ---------------------------------------------------------------------------
// Program arguments
// ---------------------------------------------------------------------------

const ARGS_PAGE_ADDR: usize = 0x80_8000_0000;
const ARGS_TEMP_PAGE: usize = 0x88_0000_0000;

/// Write program arguments into the child task's address space.
/// `args` is a list of byte slices (argv[0], argv[1], ...).
/// Allocates one physical page, writes the args layout, maps into child's CR3.
fn set_args(info: &SpawnInfo, args: &[&[u8]]) -> Result<(), ()> {
    let frame = syscall::sys_phys_alloc(1)?;
    syscall::sys_map_phys(frame, ARGS_TEMP_PAGE, 1)?;

    let base = ARGS_TEMP_PAGE as *mut u8;
    unsafe {
        // Zero the page
        core::ptr::write_bytes(base, 0, PAGE_SIZE);

        // Write argc
        *(base as *mut u64) = args.len() as u64;

        let mut offset = 8usize;
        for arg in args {
            // Write arg length
            if offset + 8 + arg.len() > PAGE_SIZE {
                break; // Out of space
            }
            *(base.add(offset) as *mut u64) = arg.len() as u64;
            offset += 8;
            // Write arg bytes
            core::ptr::copy_nonoverlapping(arg.as_ptr(), base.add(offset), arg.len());
            offset += arg.len();
        }
    }

    // Map into child's address space
    syscall::sys_addrspace_map(info.cr3, ARGS_PAGE_ADDR, frame, 1, 0)?; // read-only
    Ok(())
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

fn fat_name_to_buf(name: &[u8; 11], buf: &mut [u8; 16]) -> usize {
    let base_len = name[0..8]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    let mut pos = 0;
    for i in 0..base_len {
        if pos < buf.len() {
            buf[pos] = name[i];
            pos += 1;
        }
    }
    let ext_len = name[8..11]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    if ext_len > 0 {
        if pos < buf.len() {
            buf[pos] = b'.';
            pos += 1;
        }
        for i in 0..ext_len {
            if pos < buf.len() {
                buf[pos] = name[8 + i];
                pos += 1;
            }
        }
    }
    pos
}

/// Check if a FAT 8.3 name is an essential boot service (loaded from boot image).
fn is_essential_elf(name: &[u8; 11]) -> bool {
    if &name[8..11] != b"ELF" { return false; }
    let base = &name[0..8];
    base == b"NAMESRVR" || base == b"CONSOLE " || base == b"KEYBOARD"
        || base == b"DISK    " || base == b"INPUT   " || base == b"VFS     "
        || base == b"NET     "
}

/// Mint a cap in a temporary slot, grant it to a child task, then delete it.
/// Uses slot 14 as a scratch slot for minting.
fn mint_and_grant(tid: usize, dest_slot: usize, cap_type: u64, param0: u64, param1: u64) {
    const SCRATCH_SLOT: usize = 14;
    let _ = syscall::sys_cap_mint(SCRATCH_SLOT, cap_type, param0, param1);
    let _ = syscall::sys_cap_grant(tid, SCRATCH_SLOT, dest_slot);
    let _ = syscall::sys_cap_delete(SCRATCH_SLOT);
}

/// Grant capabilities based on FAT 8.3 name using fine-grained object capabilities.
fn grant_caps_by_name(name: &[u8; 11], tid: usize) {
    let base = &name[0..8];
    if base == b"KEYBOARD" {
        // IoPort(0x60, 0x64), Irq(1)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_IOPORT, 0x60, 0x64);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_IRQ, 1, 0);
        // Also grant old-style for backward compat during transition
        let _ = syscall::sys_grant_ioport(tid);
        let _ = syscall::sys_grant_irq(tid, 1);
    } else if base == b"DISK    " {
        // IoPort(0x1F0, 0x1F7), IoPort(0x3F6, 0x3F6), Irq(14), PhysRange(0, 4G)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_IOPORT, 0x1F0, 0x1F7);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_IOPORT, 0x3F6, 0x3F6);
        mint_and_grant(tid, 2, syscall::CAP_TYPE_IRQ, 14, 0);
        mint_and_grant(tid, 3, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        // Old-style compat
        let _ = syscall::sys_grant_ioport(tid);
        let _ = syscall::sys_grant_irq(tid, 14);
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_MAP_PHYS);
    } else if base == b"VFS     " {
        // PhysAlloc(256), PhysRange(0, 4G)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_PHYS_ALLOC, 256, 0);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS);
    } else if base == b"NET     " {
        // IoPort(0xC000, 0xC0FF), Irq(0xFF wildcard), PhysAlloc(64), PhysRange(0, 4G)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_IOPORT, 0, 0xFFFF);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_IRQ, 0xFF, 0);
        mint_and_grant(tid, 2, syscall::CAP_TYPE_PHYS_ALLOC, 64, 0);
        mint_and_grant(tid, 3, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        let _ = syscall::sys_grant_cap(tid,
            syscall::CAP_IOPORT | syscall::CAP_IRQ | syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS);
    } else if base == b"INPUT   " {
        // TaskMgmt(0)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_TASK_MGMT);
    } else if base == b"SHELL   " {
        // TaskMgmt(0), PhysAlloc(64), PhysRange(0, 4G), IoPort(ACPI shutdown ports)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_PHYS_ALLOC, 64, 0);
        mint_and_grant(tid, 2, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        mint_and_grant(tid, 3, syscall::CAP_TYPE_IOPORT, 0x604, 0x604);
        mint_and_grant(tid, 4, syscall::CAP_TYPE_IOPORT, 0xB004, 0xB004);
        let _ = syscall::sys_grant_cap(tid,
            syscall::CAP_TASK_MGMT | syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS | syscall::CAP_IOPORT);
    } else if base == b"SHUTDOWN" {
        // TaskMgmt(0) for signaling tasks, IoPort(0x604,0xB004) for ACPI power-off
        mint_and_grant(tid, 0, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_IOPORT, 0x604, 0x604);
        mint_and_grant(tid, 2, syscall::CAP_TYPE_IOPORT, 0xB004, 0xB004);
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_TASK_MGMT | syscall::CAP_IOPORT);
    } else if base == b"LOGIN   " {
        // TaskMgmt(0), PhysAlloc(64), PhysRange(0, 4G), SetUid, IoPort(ACPI shutdown ports)
        mint_and_grant(tid, 0, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        mint_and_grant(tid, 1, syscall::CAP_TYPE_PHYS_ALLOC, 64, 0);
        mint_and_grant(tid, 2, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        mint_and_grant(tid, 3, syscall::CAP_TYPE_SET_UID, 0, 0);
        mint_and_grant(tid, 4, syscall::CAP_TYPE_IOPORT, 0x604, 0x604);
        mint_and_grant(tid, 5, syscall::CAP_TYPE_IOPORT, 0xB004, 0xB004);
        let _ = syscall::sys_grant_cap(tid,
            syscall::CAP_TASK_MGMT | syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS | syscall::CAP_SET_UID | syscall::CAP_IOPORT);
    }
}

/// Look up a named service via the nameserver.
fn lookup_service(name: &[u8]) -> Option<usize> {
    let mut buf = [0u8; 24];
    let len = name.len().min(24);
    buf[..len].copy_from_slice(&name[..len]);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        Some(reply.tag as usize)
    } else {
        None
    }
}

/// Look up a service, retrying with yields between attempts.
fn lookup_service_with_retry(name: &[u8], max_attempts: usize) -> Option<usize> {
    for _ in 0..max_attempts {
        if let Some(tid) = lookup_service(name) {
            return Some(tid);
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    }
    None
}

// (Disk-based FAT32 reader removed — init now uses VFS for disk files)

// ---------------------------------------------------------------------------
// Framebuffer info handoff to console server
// ---------------------------------------------------------------------------

fn send_fb_info(console_tid: usize) {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };

    // Query the kernel console's current cursor position so the
    // user-space console server can continue where the kernel left off.
    let (row, col) = syscall::sys_console_pos();

    // Pack framebuffer info into one IPC message
    // data[0] = physical address
    // data[1] = (width << 32) | height
    // data[2] = (pitch << 32) | bpp
    // data[3] = (red_pos << 16) | (green_pos << 8) | blue_pos
    // data[4] = (cursor_row << 32) | cursor_col
    let msg = Message {
        sender: 0,
        tag: 100, // TAG_FB_INIT
        data: [
            info.fb_addr,
            ((info.fb_width as u64) << 32) | (info.fb_height as u64),
            ((info.fb_pitch as u64) << 32) | (info.fb_bpp as u64),
            ((info.fb_red_pos as u64) << 16) | ((info.fb_green_pos as u64) << 8) | (info.fb_blue_pos as u64),
            ((row as u64) << 32) | (col as u64),
            0,
        ],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(console_tid, &msg, &mut reply).is_err() {
        println!("[init] Failed to send FB info to console");
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Load essential services from boot image
// ---------------------------------------------------------------------------

struct BootContext {
    console_pipe: usize, // pipe handle for stdout/stderr
    input_tid: usize,
    vfs_spawn: Option<SpawnInfo>,
}

fn load_essentials_from_boot_image(rootfs_phys: usize, rootfs_size: usize) -> BootContext {
    println!("[init] Mounting boot image");

    // Map the entire rootfs image
    let rootfs_pages = (rootfs_size + PAGE_SIZE - 1) / PAGE_SIZE;
    if syscall::sys_map_phys(rootfs_phys, BOOT_IMG_BASE, rootfs_pages).is_err() {
        println!("[init] Failed to map boot image");
        return BootContext { console_pipe: 0, input_tid: 0, vfs_spawn: None };
    }

    let rootfs = unsafe { core::slice::from_raw_parts(BOOT_IMG_BASE as *const u8, rootfs_size) };
    let bpb = parse_bpb(rootfs);
    let (entries, count) = scan_root_dir(rootfs, &bpb);

    println!("[init] Files on boot image: {}", count);

    // Pass 1: find and spawn NAMESRVR.ELF first (guarantees TID 2)
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"NAMESRVR" && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        let _ = set_args(&info, &[b"nameserver"]);
                        let _ = info.start();
                        println!("[init] Spawned nameserver (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn nameserver"),
                }
            }
            break;
        }
    }

    // Pass 2: spawn CONSOLE.ELF (needs framebuffer info from boot info)
    let mut console_pipe: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"CONSOLE " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        // Console: PhysRange(0, 4G) for framebuffer mapping
                        mint_and_grant(info.tid, 0, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
                        let _ = syscall::sys_grant_cap(info.tid, syscall::CAP_MAP_PHYS);
                        let _ = set_args(&info, &[b"console"]);
                        // Create console pipe and set fds BEFORE starting console
                        // to avoid race where console reaches main loop before fd 0 is set
                        if let Ok(pipe) = syscall::sys_pipe_create() {
                            console_pipe = pipe;
                            let _ = syscall::sys_pipe_fd_set(info.tid, 0, pipe, false);
                            let my_tid = syscall::sys_getpid() as usize;
                            let _ = syscall::sys_pipe_fd_set(my_tid, 1, pipe, true);
                            let _ = syscall::sys_pipe_fd_set(my_tid, 2, pipe, true);
                        }
                        let _ = info.start();
                        send_fb_info(info.tid);
                        println!("[init] Spawned console (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn console"),
                }
            }
            break;
        }
    }

    // Pass 3: spawn essential ELFs (KEYBOARD, DISK) — skip INPUT and non-essentials
    let mut spawned_tids = [0usize; 32];
    let mut spawned_count = 0usize;
    for i in 0..count {
        let e = &entries[i];

        // Skip already-spawned
        if &e.name[0..8] == b"NAMESRVR" || (&e.name[0..8] == b"CONSOLE " && &e.name[8..11] == b"ELF") {
            continue;
        }
        // Skip INPUT (deferred to after keyboard) and VFS (deferred to after phase 2)
        if (&e.name[0..8] == b"INPUT   " || &e.name[0..8] == b"VFS     ") && &e.name[8..11] == b"ELF" {
            continue;
        }
        // Only spawn .ELF files
        if &e.name[8..11] != b"ELF" {
            continue;
        }
        // Skip non-essential ELFs — they will be loaded from disk later
        if !is_essential_elf(&e.name) {
            continue;
        }

        let mut namebuf = [0u8; 16];
        let namelen = fat_name_to_buf(&e.name, &mut namebuf);
        if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
            match load_elf(data) {
                Ok(info) => {
                    let tid = info.tid;
                    grant_caps_by_name(&e.name, tid);
                    if console_pipe != 0 {
                        let _ = syscall::sys_pipe_fd_set(tid, 1, console_pipe, true);
                        let _ = syscall::sys_pipe_fd_set(tid, 2, console_pipe, true);
                    }
                    let _ = set_args(&info, &[&namebuf[..namelen]]);
                    let _ = info.start();
                    if spawned_count < 32 {
                        spawned_tids[spawned_count] = tid;
                        spawned_count += 1;
                    }
                    let base_len = e.name[0..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
                    let mut lbuf = [0u8; 8];
                    for j in 0..base_len { lbuf[j] = e.name[j].to_ascii_lowercase(); }
                    if let Ok(name) = core::str::from_utf8(&lbuf[..base_len]) {
                        println!("[init] Spawned {} (TID {})", name, tid);
                    }
                }
                Err(()) => println!("[init] FAILED to spawn"),
            }
        }
    }

    // Pass 4: spawn INPUT.ELF (needs keyboard to be running)
    let mut input_tid: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"INPUT   " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        input_tid = info.tid;
                        if console_pipe != 0 {
                            let _ = syscall::sys_pipe_fd_set(info.tid, 1, console_pipe, true);
                            let _ = syscall::sys_pipe_fd_set(info.tid, 2, console_pipe, true);
                        }
                        let _ = set_args(&info, &[b"input"]);
                        let _ = info.start();
                        println!("[init] Spawned input (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn input"),
                }
            }
            break;
        }
    }

    // Wire fd 0 (stdin) to input server for all previously spawned tasks
    if input_tid != 0 {
        for i in 0..spawned_count {
            let _ = syscall::sys_fd_set(spawned_tids[i], 0, input_tid, 1); // TAG_READ=1
        }
    }

    // Pass 5: load VFS.ELF but do NOT start it yet (deferred to after phase 2)
    let mut vfs_spawn: Option<SpawnInfo> = None;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"VFS     " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        grant_caps_by_name(&e.name, info.tid);
                        if console_pipe != 0 {
                            let _ = syscall::sys_pipe_fd_set(info.tid, 1, console_pipe, true);
                            let _ = syscall::sys_pipe_fd_set(info.tid, 2, console_pipe, true);
                        }
                        if input_tid != 0 {
                            let _ = syscall::sys_fd_set(info.tid, 0, input_tid, 1);
                        }
                        let _ = set_args(&info, &[b"vfs"]);
                        println!("[init] Spawned vfs (TID {}, deferred start)", info.tid);
                        vfs_spawn = Some(info);
                    }
                    Err(()) => println!("[init] FAILED to spawn vfs"),
                }
            }
            break;
        }
    }

    BootContext { console_pipe, input_tid, vfs_spawn }
}

// ---------------------------------------------------------------------------
// Phase 2: Load remaining programs from disk
// ---------------------------------------------------------------------------

const MAX_DEFERRED: usize = 16;

struct DeferredTasks {
    spawns: [Option<SpawnInfo>; MAX_DEFERRED],
    count: usize,
}

impl DeferredTasks {
    fn new() -> Self {
        const NONE: Option<SpawnInfo> = None;
        DeferredTasks { spawns: [NONE; MAX_DEFERRED], count: 0 }
    }

    /// Start each deferred task one at a time, waiting for each to exit
    /// before starting the next (prevents interleaved output).
    fn start_sequentially(&mut self) {
        for i in 0..self.count {
            if let Some(info) = self.spawns[i].take() {
                let _ = info.start();
                let _ = syscall::sys_wait();
            }
        }
    }
}

fn load_from_vfs(vfs_tid: usize, console_pipe: usize, input_tid: usize) -> DeferredTasks {
    let mut deferred = DeferredTasks::new();

    // Open /usr/bin directory via VFS
    let (dir_handle, _, _) = match vfs::open(vfs_tid, b"/usr/bin") {
        Ok(h) => h,
        Err(_) => {
            println!("[init] /usr/bin not found on VFS.");
            return deferred;
        }
    };

    // Find LOGIN.ELF (or SHELL.ELF as fallback) in /usr/bin
    let mut login_name: Option<[u8; 11]> = None;
    let mut shell_name: Option<[u8; 11]> = None;
    let mut index = 0u32;

    loop {
        match vfs::readdir(vfs_tid, dir_handle, index) {
            Ok(Some(entry)) => {
                if &entry.name[0..8] == b"LOGIN   " && &entry.name[8..11] == b"ELF" && !entry.is_dir {
                    login_name = Some(entry.name);
                    break;
                }
                if &entry.name[0..8] == b"SHELL   " && &entry.name[8..11] == b"ELF" && !entry.is_dir {
                    shell_name = Some(entry.name);
                }
                index += 1;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = vfs::close(vfs_tid, dir_handle);

    let name = match login_name.or(shell_name) {
        Some(n) => n,
        None => {
            println!("[init] login/shell not found in /usr/bin");
            return deferred;
        }
    };

    let mut namebuf = [0u8; 16];
    let namelen = fat_name_to_buf(&name, &mut namebuf);
    let loading_name = if login_name.is_some() { "login" } else { "shell" };

    // Build path: "/usr/bin/SHELL.ELF"
    let mut path = [0u8; 48];
    let prefix = b"/usr/bin/";
    path[..prefix.len()].copy_from_slice(prefix);
    path[prefix.len()..prefix.len() + namelen].copy_from_slice(&namebuf[..namelen]);
    let path_len = prefix.len() + namelen;

    // Open file via VFS
    let (file_handle, file_size, _) = match vfs::open(vfs_tid, &path[..path_len]) {
        Ok(h) => h,
        Err(_) => {
            println!("[init]   FAILED to open via VFS");
            return deferred;
        }
    };

    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Allocate pages and read file content via VFS directly into them
    let mut success = true;
    for p in 0..pages_needed {
        let frame = match syscall::sys_phys_alloc(1) {
            Ok(f) => f,
            Err(()) => { success = false; break; }
        };
        if syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1).is_err() {
            success = false; break;
        }
        let offset = (p * PAGE_SIZE) as u32;
        let to_read = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        if vfs::read(vfs_tid, file_handle, frame, offset, to_read).is_err() {
            success = false; break;
        }
    }

    let _ = vfs::close(vfs_tid, file_handle);

    if !success {
        println!("[init]   FAILED to read from VFS");
        return deferred;
    }

    let data = unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) };
    match load_elf(data) {
        Ok(info) => {
            let tid = info.tid;
            grant_caps_by_name(&name, tid);
            if console_pipe != 0 {
                let _ = syscall::sys_pipe_fd_set(tid, 1, console_pipe, true);
                let _ = syscall::sys_pipe_fd_set(tid, 2, console_pipe, true);
            }
            if input_tid != 0 {
                let _ = syscall::sys_fd_set(tid, 0, input_tid, 1);
            }
            let _ = set_args(&info, &[&namebuf[..namelen]]);
            println!("[init] Spawned {} (TID {}, deferred start)", loading_name, tid);
            if deferred.count < MAX_DEFERRED {
                deferred.spawns[deferred.count] = Some(info);
                deferred.count += 1;
            }
        }
        Err(()) => println!("[init]   FAILED to spawn"),
    }

    deferred
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[init] Starting init process.");

    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    let mod_count = info.module_count as usize;

    // Find boot image module
    let mut found = false;
    for i in 0..mod_count {
        let m = &info.modules[i];
        let name = module_name(&m.name);
        if starts_with(name, b"boot") {
            let phys = m.phys_start as usize;
            let size = (m.phys_end - m.phys_start) as usize;

            // Phase 1: Load essential services from boot image
            let ctx = load_essentials_from_boot_image(phys, size);

            // Unload boot image — return pages to the physical memory allocator
            let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
            let _ = syscall::sys_phys_free(phys, pages);
            println!("[init] Boot image freed ({} pages).", pages);

            // Phase 2: Start VFS, wait for it to register
            let vfs_tid = if let Some(vfs) = ctx.vfs_spawn {
                println!("[init] Starting VFS (TID {})", vfs.tid);
                let _ = vfs.start();
                match lookup_service_with_retry(b"vfs", 50) {
                    Some(tid) => {
                        println!("[init] VFS ready.");
                        Some(tid)
                    }
                    None => {
                        println!("[init] VFS failed to register.");
                        None
                    }
                }
            } else {
                None
            };

            // Phase 3: Load remaining programs from VFS (loaded but not started)
            let mut deferred = if let Some(vfs) = vfs_tid {
                load_from_vfs(vfs, ctx.console_pipe, ctx.input_tid)
            } else {
                println!("[init] No VFS, skipping disk program loading.");
                DeferredTasks::new()
            };

            // Phase 4: Start non-essential programs
            println!("[init] All programs loaded. Starting deferred tasks.");
            deferred.start_sequentially();

            found = true;
            break;
        }
    }

    if !found {
        println!("[init] ERROR: boot image module not found!");
    }

    loop {
        syscall::sys_yield();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[init] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
