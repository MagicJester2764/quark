#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{println, syscall};

const PAGE_SIZE: usize = 4096;
const BOOT_INFO_ADDR: usize = 0x80_4000_0000;
const FILE_BUF_BASE: usize = 0x82_0000_0000;
const BOOT_IMG_BASE: usize = 0x85_0000_0000;
const DISK_IO_BUF: usize = 0x86_0000_0000;

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const TAG_READ_SECTOR: u64 = 1;
const TAG_DISK_OK: u64 = 0;

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
        || base == b"DISK    " || base == b"INPUT   "
}

/// Grant capabilities based on FAT 8.3 name.
fn grant_caps_by_name(name: &[u8; 11], tid: usize) {
    let base = &name[0..8];
    if base == b"KEYBOARD" {
        let _ = syscall::sys_grant_ioport(tid);
        let _ = syscall::sys_grant_irq(tid, 1);
    } else if base == b"DISK    " {
        let _ = syscall::sys_grant_ioport(tid);
        let _ = syscall::sys_grant_irq(tid, 14);
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_MAP_PHYS);
    } else if base == b"DISKTEST" {
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS);
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

// ---------------------------------------------------------------------------
// Disk-based FAT32 reader
// ---------------------------------------------------------------------------

struct DiskReader {
    disk_tid: usize,
    buf_phys: usize,
    part_lba: u32, // starting LBA of the FAT32 partition
    bpb: Bpb,
}

impl DiskReader {
    fn new(disk_tid: usize) -> Result<Self, ()> {
        let buf_phys = syscall::sys_phys_alloc(1).map_err(|_| {
            println!("[init] Failed to alloc phys page for disk I/O");
        })?;
        syscall::sys_map_phys(buf_phys, DISK_IO_BUF, 1).map_err(|_| {
            println!("[init] Failed to map disk I/O buffer");
        })?;

        // Find the rootfs partition by reading GPT
        let part_lba = Self::find_rootfs_partition(disk_tid, buf_phys)?;
        println!("[init] Rootfs partition at LBA {}", part_lba);

        // Read BPB from partition start
        if part_lba > 0 {
            Self::raw_read_sector(disk_tid, buf_phys, part_lba).map_err(|_| {
                println!("[init] Failed to read BPB at LBA {}", part_lba);
            })?;
        }
        let data = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let bpb = parse_bpb(data);
        println!("[init] BPB: bps={} spc={} reserved={} root_cluster={}",
            bpb.bytes_per_sector, bpb.sectors_per_cluster,
            bpb.reserved_sectors, bpb.root_cluster);

        Ok(DiskReader { disk_tid, buf_phys, part_lba, bpb })
    }

    /// Parse GPT to find the second partition (rootfs).
    fn find_rootfs_partition(disk_tid: usize, buf_phys: usize) -> Result<u32, ()> {
        // Read LBA 0 to check for protective MBR vs raw FAT
        Self::raw_read_sector(disk_tid, buf_phys, 0)?;
        let sec0 = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        // Check MBR signature (0x55AA at offset 510)
        let has_mbr = sec0[510] == 0x55 && sec0[511] == 0xAA;
        // Check if sector 0 looks like a FAT BPB (bytes_per_sector at offset 11)
        let bps = read_u16(sec0, 11);
        let is_fat = bps == 512 || bps == 1024 || bps == 2048 || bps == 4096;

        if !has_mbr || is_fat {
            // No partition table, or this is a raw FAT filesystem
            println!("[init] No partition table, using LBA 0 as filesystem start");
            return Ok(0);
        }

        // Read GPT header (LBA 1)
        Self::raw_read_sector(disk_tid, buf_phys, 1)?;
        let hdr = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        // Verify GPT signature "EFI PART"
        if &hdr[0..8] != b"EFI PART" {
            // Try MBR partition table instead
            println!("[init] No GPT, checking MBR partitions");
            Self::raw_read_sector(disk_tid, buf_phys, 0)?;
            let mbr = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
            // MBR partition entry 1 at offset 446, start LBA at offset 8
            let p1_lba = read_u32(mbr, 446 + 8);
            if p1_lba != 0 {
                return Ok(p1_lba);
            }
            println!("[init] No usable partition found");
            return Err(());
        }

        let entry_start_lba = read_u32(hdr, 72); // partition entry start LBA (usually 2)
        let entry_size = read_u32(hdr, 84);       // size of each entry (usually 128)
        let num_entries = read_u32(hdr, 80);

        println!("[init] GPT: {} entries at LBA {}, size {}", num_entries, entry_start_lba, entry_size);

        if entry_size == 0 {
            return Err(());
        }

        // Read the sector containing partition entries
        Self::raw_read_sector(disk_tid, buf_phys, entry_start_lba)?;
        let entries = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        // We want the second partition (index 1) — the rootfs
        let entries_per_sector = 512 / entry_size as usize;
        let part_idx = 1; // second partition
        let sector_of_entry = part_idx / entries_per_sector;
        let offset_in_sector = (part_idx % entries_per_sector) * entry_size as usize;

        if sector_of_entry > 0 {
            Self::raw_read_sector(disk_tid, buf_phys, entry_start_lba + sector_of_entry as u32)?;
        }

        let data = if sector_of_entry > 0 {
            unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) }
        } else {
            entries
        };

        // GPT entry: bytes 32-39 = starting LBA (u64 LE)
        let start_lba_off = offset_in_sector + 32;
        let start_lba = read_u32(data, start_lba_off); // lower 32 bits suffice

        println!("[init] GPT partition 2 starts at LBA {}", start_lba);

        if start_lba == 0 {
            println!("[init] Rootfs partition not found in GPT");
            return Err(());
        }

        Ok(start_lba)
    }

    fn raw_read_sector(disk_tid: usize, buf_phys: usize, lba: u32) -> Result<(), ()> {
        let msg = Message {
            sender: 0,
            tag: TAG_READ_SECTOR,
            data: [lba as u64, buf_phys as u64, 0, 0, 0, 0],
        };
        let mut reply = Message::empty();
        if syscall::sys_call(disk_tid, &msg, &mut reply).is_err() {
            println!("[init] disk read IPC failed for LBA {}", lba);
            return Err(());
        }
        if reply.tag != TAG_DISK_OK {
            println!("[init] disk read LBA {} failed: tag={}", lba, reply.tag);
            return Err(());
        }
        Ok(())
    }

    /// Read a sector relative to the partition start.
    fn read_sector(&self, lba: u32) -> Result<(), ()> {
        Self::raw_read_sector(self.disk_tid, self.buf_phys, self.part_lba + lba)
    }

    fn sector_data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) }
    }

    fn fat_next(&self, cluster: u32) -> Option<u32> {
        let fat_byte_off = (cluster as usize) * 4;
        let sector_in_fat = fat_byte_off / 512;
        let offset_in_sector = fat_byte_off % 512;
        let lba = self.bpb.reserved_sectors + sector_in_fat as u32;
        if self.read_sector(lba).is_err() { return None; }
        let data = self.sector_data();
        let next = read_u32(data, offset_in_sector) & 0x0FFF_FFFF;
        if next >= 0x0FFF_FFF8 { None } else { Some(next) }
    }

    fn cluster_start_lba(&self, cluster: u32) -> u32 {
        let data_start = self.bpb.reserved_sectors + self.bpb.num_fats * self.bpb.fat_size_32;
        data_start + (cluster - 2) * self.bpb.sectors_per_cluster
    }
}

/// Scan a directory at the given starting cluster for file entries.
fn disk_scan_dir(disk: &DiskReader, dir_cluster: u32) -> ([RootDirEntry; MAX_DIR_ENTRIES], usize) {
    let mut entries: [RootDirEntry; MAX_DIR_ENTRIES] = unsafe { core::mem::zeroed() };
    let mut count = 0;
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    'outer: loop {
        let start_lba = disk.cluster_start_lba(cluster);
        for s in 0..spc {
            if disk.read_sector(start_lba + s).is_err() {
                break 'outer;
            }
            // Copy sector data out before it's overwritten by fat_next
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(disk.sector_data());

            for e in 0..16 {
                if count >= MAX_DIR_ENTRIES { break 'outer; }
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 { break 'outer; }
                if first_byte == 0xE5 { continue; }
                let attr = sec_buf[off + 11];
                if attr & 0x0F == 0x0F { continue; }
                if attr & 0x08 != 0 { continue; }
                if attr & 0x10 != 0 { continue; }

                let mut name = [0u8; 11];
                name.copy_from_slice(&sec_buf[off..off + 11]);
                let cluster_hi = read_u16(&sec_buf, off + 20) as u32;
                let cluster_lo = read_u16(&sec_buf, off + 26) as u32;
                entries[count] = RootDirEntry {
                    name,
                    first_cluster: (cluster_hi << 16) | cluster_lo,
                    file_size: read_u32(&sec_buf, off + 28),
                };
                count += 1;
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }

    (entries, count)
}

/// Find a subdirectory by its FAT 8.3 name within a directory cluster.
/// Returns the subdirectory's starting cluster, or None if not found.
fn disk_find_subdir(disk: &DiskReader, dir_cluster: u32, target: &[u8; 11]) -> Option<u32> {
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    loop {
        let start_lba = disk.cluster_start_lba(cluster);
        for s in 0..spc {
            if disk.read_sector(start_lba + s).is_err() { return None; }
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(disk.sector_data());

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 { return None; }
                if first_byte == 0xE5 { continue; }
                let attr = sec_buf[off + 11];
                if attr & 0x0F == 0x0F { continue; }
                if attr & 0x10 == 0 { continue; } // skip non-directories

                if &sec_buf[off..off + 11] == target {
                    let hi = read_u16(&sec_buf, off + 20) as u32;
                    let lo = read_u16(&sec_buf, off + 26) as u32;
                    return Some((hi << 16) | lo);
                }
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }
    None
}

fn read_file_from_disk(
    disk: &DiskReader,
    first_cluster: u32,
    file_size: u32,
) -> Result<&'static [u8], ()> {
    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    for p in 0..pages_needed {
        let frame = syscall::sys_phys_alloc(1)?;
        syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1)?;
    }

    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = first_cluster;
    let mut copied = 0usize;

    while copied < size {
        let start_lba = disk.cluster_start_lba(cluster);
        for s in 0..spc {
            if copied >= size { break; }
            disk.read_sector(start_lba + s)?;
            let chunk = 512.min(size - copied);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    DISK_IO_BUF as *const u8,
                    (FILE_BUF_BASE + copied) as *mut u8,
                    chunk,
                );
            }
            copied += chunk;
        }
        if copied < size {
            match disk.fat_next(cluster) {
                Some(next) => cluster = next,
                None => break,
            }
        }
    }

    Ok(unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) })
}

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
    console_tid: usize,
    input_tid: usize,
}

fn load_essentials_from_boot_image(rootfs_phys: usize, rootfs_size: usize) -> BootContext {
    println!("[init] Mounting boot image");

    // Map the entire rootfs image
    let rootfs_pages = (rootfs_size + PAGE_SIZE - 1) / PAGE_SIZE;
    if syscall::sys_map_phys(rootfs_phys, BOOT_IMG_BASE, rootfs_pages).is_err() {
        println!("[init] Failed to map boot image");
        return BootContext { console_tid: 0, input_tid: 0 };
    }

    let rootfs = unsafe { core::slice::from_raw_parts(BOOT_IMG_BASE as *const u8, rootfs_size) };
    let bpb = parse_bpb(rootfs);
    let (entries, count) = scan_root_dir(rootfs, &bpb);

    println!("[init] Files on boot image: {}", count);

    // Pass 1: find and spawn NAMESRVR.ELF first (guarantees TID 2)
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"NAMESRVR" && &e.name[8..11] == b"ELF" {
            println!("[init] Loading NAMESRVR.ELF");
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        let _ = info.start();
                        println!("[init]   Spawned TID {}", info.tid);
                    }
                    Err(()) => println!("[init]   FAILED to spawn"),
                }
            }
            break;
        }
    }

    // Pass 2: spawn CONSOLE.ELF (needs framebuffer info from boot info)
    let mut console_tid: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"CONSOLE " && &e.name[8..11] == b"ELF" {
            println!("[init] Loading CONSOLE.ELF");
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        console_tid = info.tid;
                        let _ = syscall::sys_grant_cap(info.tid, syscall::CAP_MAP_PHYS);
                        let _ = info.start();
                        send_fb_info(info.tid);
                        // Wire init's own stdout/stderr to console server
                        let my_tid = syscall::sys_getpid() as usize;
                        let _ = syscall::sys_fd_set(my_tid, 1, info.tid, 1); // TAG_WRITE=1
                        let _ = syscall::sys_fd_set(my_tid, 2, info.tid, 1);
                        println!("[init]   Console spawned TID {}", info.tid);
                    }
                    Err(()) => println!("[init]   FAILED to spawn console"),
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
        // Skip INPUT (deferred to after keyboard)
        if &e.name[0..8] == b"INPUT   " && &e.name[8..11] == b"ELF" {
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
        if let Ok(fname) = core::str::from_utf8(&namebuf[..namelen]) {
            println!("[init] Loading {}", fname);
        }

        if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
            match load_elf(data) {
                Ok(info) => {
                    let tid = info.tid;
                    grant_caps_by_name(&e.name, tid);
                    if console_tid != 0 {
                        let _ = syscall::sys_fd_set(tid, 1, console_tid, 1);
                        let _ = syscall::sys_fd_set(tid, 2, console_tid, 1);
                    }
                    let _ = info.start();
                    if spawned_count < 32 {
                        spawned_tids[spawned_count] = tid;
                        spawned_count += 1;
                    }
                    println!("[init]   Spawned TID {}", tid);
                }
                Err(()) => println!("[init]   FAILED to spawn"),
            }
        }
    }

    // Pass 4: spawn INPUT.ELF (needs keyboard to be running)
    let mut input_tid: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"INPUT   " && &e.name[8..11] == b"ELF" {
            println!("[init] Loading INPUT.ELF");
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match load_elf(data) {
                    Ok(info) => {
                        input_tid = info.tid;
                        if console_tid != 0 {
                            let _ = syscall::sys_fd_set(info.tid, 1, console_tid, 1);
                            let _ = syscall::sys_fd_set(info.tid, 2, console_tid, 1);
                        }
                        let _ = info.start();
                        println!("[init]   Input spawned TID {}", info.tid);
                    }
                    Err(()) => println!("[init]   FAILED to spawn input"),
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

    BootContext { console_tid, input_tid }
}

// ---------------------------------------------------------------------------
// Phase 2: Load remaining programs from disk
// ---------------------------------------------------------------------------

fn load_from_disk(console_tid: usize, input_tid: usize) {
    // Discover disk service via nameserver
    let disk_tid = match lookup_service_with_retry(b"disk", 20) {
        Some(tid) => tid,
        None => {
            println!("[init] Disk service not found, skipping disk loading.");
            return;
        }
    };
    println!("[init] Found disk at TID {}", disk_tid);

    // Initialize disk reader (reads BPB from sector 0)
    let disk = match DiskReader::new(disk_tid) {
        Ok(d) => d,
        Err(()) => {
            println!("[init] Failed to read disk filesystem.");
            return;
        }
    };

    // Navigate to /usr/bin/
    //   FAT 8.3: "USR     " (dir), "BIN     " (dir)
    let usr_cluster = match disk_find_subdir(&disk, disk.bpb.root_cluster, b"USR        ") {
        Some(c) => c,
        None => {
            println!("[init] /usr not found on disk.");
            return;
        }
    };
    let bin_cluster = match disk_find_subdir(&disk, usr_cluster, b"BIN        ") {
        Some(c) => c,
        None => {
            println!("[init] /usr/bin not found on disk.");
            return;
        }
    };

    let (entries, count) = disk_scan_dir(&disk, bin_cluster);
    println!("[init] Files in /usr/bin: {}", count);

    // Load non-essential ELFs from disk
    for i in 0..count {
        let e = &entries[i];
        if &e.name[8..11] != b"ELF" { continue; }
        if is_essential_elf(&e.name) { continue; }

        let mut namebuf = [0u8; 16];
        let namelen = fat_name_to_buf(&e.name, &mut namebuf);
        if let Ok(fname) = core::str::from_utf8(&namebuf[..namelen]) {
            println!("[init] Loading {} from disk", fname);
        }

        match read_file_from_disk(&disk, e.first_cluster, e.file_size) {
            Ok(data) => match load_elf(data) {
                Ok(info) => {
                    let tid = info.tid;
                    grant_caps_by_name(&e.name, tid);
                    if console_tid != 0 {
                        let _ = syscall::sys_fd_set(tid, 1, console_tid, 1);
                        let _ = syscall::sys_fd_set(tid, 2, console_tid, 1);
                    }
                    let _ = info.start();
                    if input_tid != 0 {
                        let _ = syscall::sys_fd_set(tid, 0, input_tid, 1);
                    }
                    println!("[init]   Spawned TID {}", tid);
                }
                Err(()) => println!("[init]   FAILED to spawn"),
            },
            Err(()) => println!("[init]   FAILED to read from disk"),
        }
    }
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

            // Phase 2: Load remaining programs from disk
            load_from_disk(ctx.console_tid, ctx.input_tid);

            found = true;
            break;
        }
    }

    if !found {
        println!("[init] ERROR: boot image module not found!");
    }

    println!("[init] All programs loaded. Entering idle loop.");

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
