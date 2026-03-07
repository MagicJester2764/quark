#![no_std]
#![no_main]
#![allow(dead_code)]

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const PAGE_SIZE: usize = 4096;
const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;
const TAG_NS_LOOKUP: u64 = 2;

// Disk driver protocol
const TAG_READ_SECTOR: u64 = 1;
const TAG_DISK_OK: u64 = 0;

// VFS IPC tags
const TAG_OPEN: u64 = 1;
const TAG_READ: u64 = 2;
const TAG_CLOSE: u64 = 3;
const TAG_READDIR: u64 = 4;
const TAG_STAT: u64 = 5;
const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

// Error codes in reply data[0]
const ERR_NOT_FOUND: u64 = 1;
const ERR_INVALID_HANDLE: u64 = 2;
const ERR_IO: u64 = 3;
const ERR_TOO_MANY_OPEN: u64 = 4;
const ERR_INVALID_PATH: u64 = 5;
const ERR_NOT_DIR: u64 = 6;
const ERR_IS_DIR: u64 = 7;

// Virtual addresses for temp mappings
const DISK_IO_BUF: usize = 0x86_0000_0000;
const CLIENT_BUF: usize = 0x87_0000_0000;

// ---------------------------------------------------------------------------
// FAT32 structures
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

// ---------------------------------------------------------------------------
// Disk reader (communicates with disk driver via IPC)
// ---------------------------------------------------------------------------

struct DiskState {
    disk_tid: usize,
    buf_phys: usize,
    part_lba: u32,
    bpb: Bpb,
}

impl DiskState {
    fn raw_read_sector(disk_tid: usize, buf_phys: usize, lba: u32) -> Result<(), ()> {
        let msg = Message {
            sender: 0,
            tag: TAG_READ_SECTOR,
            data: [lba as u64, buf_phys as u64, 0, 0, 0, 0],
        };
        let mut reply = Message::empty();
        if syscall::sys_call(disk_tid, &msg, &mut reply).is_err() {
            return Err(());
        }
        if reply.tag != TAG_DISK_OK {
            return Err(());
        }
        Ok(())
    }

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
        if self.read_sector(lba).is_err() {
            return None;
        }
        let data = self.sector_data();
        let next = read_u32(data, offset_in_sector) & 0x0FFF_FFFF;
        if next >= 0x0FFF_FFF8 { None } else { Some(next) }
    }

    fn cluster_start_lba(&self, cluster: u32) -> u32 {
        let data_start = self.bpb.reserved_sectors + self.bpb.num_fats * self.bpb.fat_size_32;
        data_start + (cluster - 2) * self.bpb.sectors_per_cluster
    }

    fn find_rootfs_partition(disk_tid: usize, buf_phys: usize) -> Result<u32, ()> {
        Self::raw_read_sector(disk_tid, buf_phys, 0)?;
        let sec0 = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        let has_mbr = sec0[510] == 0x55 && sec0[511] == 0xAA;
        let bps = read_u16(sec0, 11);
        let is_fat = bps == 512 || bps == 1024 || bps == 2048 || bps == 4096;

        if !has_mbr || is_fat {
            return Ok(0);
        }

        // Read GPT header (LBA 1)
        Self::raw_read_sector(disk_tid, buf_phys, 1)?;
        let hdr = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        if &hdr[0..8] != b"EFI PART" {
            // Try MBR partition 1
            Self::raw_read_sector(disk_tid, buf_phys, 0)?;
            let mbr = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
            let p1_lba = read_u32(mbr, 446 + 8);
            if p1_lba != 0 {
                return Ok(p1_lba);
            }
            return Err(());
        }

        let entry_start_lba = read_u32(hdr, 72);
        let entry_size = read_u32(hdr, 84);
        if entry_size == 0 {
            return Err(());
        }

        // Read partition entries, find partition 2 (index 1)
        Self::raw_read_sector(disk_tid, buf_phys, entry_start_lba)?;
        let entries = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let entries_per_sector = 512 / entry_size as usize;
        let part_idx = 1;
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

        let start_lba = read_u32(data, offset_in_sector + 32);
        if start_lba == 0 {
            return Err(());
        }

        Ok(start_lba)
    }
}

// ---------------------------------------------------------------------------
// Open file table
// ---------------------------------------------------------------------------

const MAX_OPEN_FILES: usize = 32;

struct OpenFile {
    in_use: bool,
    owner_tid: usize,
    first_cluster: u32,
    file_size: u32,
    is_dir: bool,
    // Current read position for sequential reads
    read_offset: u32,
    // Cache current cluster position to avoid re-traversing chain
    cur_cluster: u32,
    cur_cluster_offset: u32, // byte offset corresponding to cur_cluster start
}

static mut FILE_TABLE: [OpenFile; MAX_OPEN_FILES] = {
    const EMPTY: OpenFile = OpenFile {
        in_use: false,
        owner_tid: 0,
        first_cluster: 0,
        file_size: 0,
        is_dir: false,
        read_offset: 0,
        cur_cluster: 0,
        cur_cluster_offset: 0,
    };
    [EMPTY; MAX_OPEN_FILES]
};

fn alloc_handle(tid: usize, cluster: u32, size: u32, is_dir: bool) -> Option<usize> {
    unsafe {
        for i in 0..MAX_OPEN_FILES {
            if !FILE_TABLE[i].in_use {
                FILE_TABLE[i] = OpenFile {
                    in_use: true,
                    owner_tid: tid,
                    first_cluster: cluster,
                    file_size: size,
                    is_dir,
                    read_offset: 0,
                    cur_cluster: cluster,
                    cur_cluster_offset: 0,
                };
                return Some(i);
            }
        }
    }
    None
}

fn get_handle(handle: usize, tid: usize) -> Option<&'static mut OpenFile> {
    if handle >= MAX_OPEN_FILES {
        return None;
    }
    unsafe {
        let f = &mut FILE_TABLE[handle];
        if f.in_use && f.owner_tid == tid {
            Some(f)
        } else {
            None
        }
    }
}

fn close_handle(handle: usize, tid: usize) -> bool {
    if handle >= MAX_OPEN_FILES {
        return false;
    }
    unsafe {
        let f = &mut FILE_TABLE[handle];
        if f.in_use && f.owner_tid == tid {
            f.in_use = false;
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Convert a path component to FAT 8.3 name.
/// Input: "HELLO.ELF" or "USR" (uppercase, no long names)
/// Output: "HELLO   ELF" or "USR        "
fn to_fat83(component: &[u8], out: &mut [u8; 11]) {
    *out = [b' '; 11];

    // Find dot separator
    let dot_pos = component.iter().position(|&b| b == b'.');

    let (base, ext) = match dot_pos {
        Some(pos) => (&component[..pos], &component[pos + 1..]),
        None => (component, &[] as &[u8]),
    };

    // Copy base name (up to 8 chars), uppercase
    let base_len = base.len().min(8);
    for i in 0..base_len {
        out[i] = base[i].to_ascii_uppercase();
    }

    // Copy extension (up to 3 chars), uppercase
    let ext_len = ext.len().min(3);
    for i in 0..ext_len {
        out[8 + i] = ext[i].to_ascii_uppercase();
    }
}

/// Resolve a path like "/USR/BIN/HELLO.ELF" to (cluster, size, is_dir).
/// Paths use "/" separators. Leading "/" is optional.
fn resolve_path(
    disk: &DiskState,
    path: &[u8],
) -> Result<(u32, u32, bool), u64> {
    let path = if !path.is_empty() && path[0] == b'/' {
        &path[1..]
    } else {
        path
    };

    if path.is_empty() {
        // Root directory
        return Ok((disk.bpb.root_cluster, 0, true));
    }

    let mut current_cluster = disk.bpb.root_cluster;

    // Split path into components
    let mut remaining = path;
    loop {
        // Find next "/" or end
        let (component, rest) = match remaining.iter().position(|&b| b == b'/') {
            Some(pos) => (&remaining[..pos], &remaining[pos + 1..]),
            None => (remaining, &[] as &[u8]),
        };

        if component.is_empty() {
            remaining = rest;
            if remaining.is_empty() {
                return Ok((current_cluster, 0, true));
            }
            continue;
        }

        let mut target = [0u8; 11];
        to_fat83(component, &mut target);

        let is_last = rest.is_empty();

        // Search directory for this component
        match find_entry(disk, current_cluster, &target)? {
            Some((cluster, size, is_dir)) => {
                if is_last {
                    return Ok((cluster, size, is_dir));
                }
                // Intermediate component must be a directory
                if !is_dir {
                    return Err(ERR_NOT_DIR);
                }
                current_cluster = cluster;
                remaining = rest;
            }
            None => return Err(ERR_NOT_FOUND),
        }
    }
}

/// Search a directory for an entry matching the given FAT 8.3 name.
/// Returns (cluster, size, is_dir) or None.
fn find_entry(
    disk: &DiskState,
    dir_cluster: u32,
    name: &[u8; 11],
) -> Result<Option<(u32, u32, bool)>, u64> {
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    loop {
        let start_lba = disk.cluster_start_lba(cluster);
        for s in 0..spc {
            if disk.read_sector(start_lba + s).is_err() {
                return Err(ERR_IO);
            }
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(disk.sector_data());

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 {
                    return Ok(None); // end of directory
                }
                if first_byte == 0xE5 {
                    continue;
                }
                let attr = sec_buf[off + 11];
                if attr & 0x0F == 0x0F {
                    continue; // LFN
                }
                if attr & 0x08 != 0 {
                    continue; // volume label
                }

                if &sec_buf[off..off + 11] == name {
                    let hi = read_u16(&sec_buf, off + 20) as u32;
                    let lo = read_u16(&sec_buf, off + 26) as u32;
                    let cluster = (hi << 16) | lo;
                    let size = read_u32(&sec_buf, off + 28);
                    let is_dir = attr & 0x10 != 0;
                    return Ok(Some((cluster, size, is_dir)));
                }
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Read file data into client's physical page
// ---------------------------------------------------------------------------

/// Read up to `max_bytes` from a file at `offset` into the client's physical page.
/// Returns bytes actually read.
fn read_file_data(
    disk: &DiskState,
    file: &mut OpenFile,
    client_phys: usize,
    offset: u32,
    max_bytes: u32,
) -> Result<u32, u64> {
    if file.is_dir {
        return Err(ERR_IS_DIR);
    }
    if offset >= file.file_size {
        return Ok(0);
    }

    let available = file.file_size - offset;
    let to_read = max_bytes.min(available).min(PAGE_SIZE as u32);
    if to_read == 0 {
        return Ok(0);
    }

    // Map client's physical page
    if syscall::sys_map_phys(client_phys, CLIENT_BUF, 1).is_err() {
        return Err(ERR_IO);
    }

    let cluster_bytes = disk.bpb.sectors_per_cluster * disk.bpb.bytes_per_sector;

    // Navigate to the cluster containing `offset`
    let mut cluster;
    let mut byte_pos;

    // Use cached position if we can advance from it
    if offset >= file.cur_cluster_offset && file.cur_cluster != 0 {
        cluster = file.cur_cluster;
        byte_pos = file.cur_cluster_offset;
    } else {
        cluster = file.first_cluster;
        byte_pos = 0;
    }

    // Skip clusters until we reach the one containing `offset`
    while byte_pos + cluster_bytes <= offset {
        match disk.fat_next(cluster) {
            Some(next) => {
                cluster = next;
                byte_pos += cluster_bytes;
            }
            None => return Ok(0),
        }
    }

    // Cache the position
    file.cur_cluster = cluster;
    file.cur_cluster_offset = byte_pos;

    let mut written = 0u32;

    while written < to_read {
        let offset_in_cluster = (offset + written) - byte_pos;
        let sector_in_cluster = offset_in_cluster / disk.bpb.bytes_per_sector;
        let offset_in_sector = offset_in_cluster % disk.bpb.bytes_per_sector;

        let lba = disk.cluster_start_lba(cluster) + sector_in_cluster;
        if disk.read_sector(lba).is_err() {
            return Err(ERR_IO);
        }

        let copy_start = offset_in_sector as usize;
        let copy_len = (512 - copy_start).min((to_read - written) as usize);

        unsafe {
            core::ptr::copy_nonoverlapping(
                (DISK_IO_BUF + copy_start) as *const u8,
                (CLIENT_BUF + written as usize) as *mut u8,
                copy_len,
            );
        }

        written += copy_len as u32;

        // Check if we need to move to next cluster
        let new_offset_in_cluster = offset_in_cluster + copy_len as u32;
        if new_offset_in_cluster >= cluster_bytes && written < to_read {
            match disk.fat_next(cluster) {
                Some(next) => {
                    cluster = next;
                    byte_pos += cluster_bytes;
                    file.cur_cluster = cluster;
                    file.cur_cluster_offset = byte_pos;
                }
                None => break,
            }
        }
    }

    file.read_offset = offset + written;

    Ok(written)
}

// ---------------------------------------------------------------------------
// Read directory entries
// ---------------------------------------------------------------------------

/// Read directory entry at `index` from a directory.
/// Returns entry info packed into IPC message data words:
///   data[0] = handle (echo back)
///   data[1..2] = 8.3 name (11 bytes in 2 words)
///   data[3] = file_size
///   data[4] = (is_dir << 32) | first_cluster
///   data[5] = attr
fn read_dir_entry(
    disk: &DiskState,
    dir_cluster: u32,
    index: u32,
) -> Result<Option<(u32, [u8; 11], u32, bool, u8)>, u64> {
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;
    let mut current_idx: u32 = 0;

    loop {
        let start_lba = disk.cluster_start_lba(cluster);
        for s in 0..spc {
            if disk.read_sector(start_lba + s).is_err() {
                return Err(ERR_IO);
            }
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(disk.sector_data());

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 {
                    return Ok(None);
                }
                if first_byte == 0xE5 {
                    continue;
                }
                let attr = sec_buf[off + 11];
                if attr & 0x0F == 0x0F {
                    continue; // LFN
                }
                if attr & 0x08 != 0 {
                    continue; // volume label
                }

                if current_idx == index {
                    let mut name = [0u8; 11];
                    name.copy_from_slice(&sec_buf[off..off + 11]);
                    let hi = read_u16(&sec_buf, off + 20) as u32;
                    let lo = read_u16(&sec_buf, off + 26) as u32;
                    let size = read_u32(&sec_buf, off + 28);
                    let is_dir = attr & 0x10 != 0;
                    let _cluster = (hi << 16) | lo;
                    return Ok(Some((_cluster, name, size, is_dir, attr)));
                }
                current_idx += 1;
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// IPC helpers
// ---------------------------------------------------------------------------

fn register_with_nameserver() {
    let name = b"vfs";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let w1 = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
    let w2 = u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]);

    let msg = Message {
        sender: 0,
        tag: TAG_NS_REGISTER,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
        println!("[vfs] Registered with nameserver.");
    } else {
        println!("[vfs] Failed to register with nameserver.");
    }
}

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

fn error_reply(sender: usize, err_code: u64) {
    let reply = Message {
        sender: 0,
        tag: TAG_ERROR,
        data: [err_code, 0, 0, 0, 0, 0],
    };
    let _ = syscall::sys_reply(sender, &reply);
}

/// Extract a null-terminated path string from IPC message data words.
fn extract_path(data: &[u64; 6]) -> &[u8] {
    let bytes = unsafe {
        core::slice::from_raw_parts(data.as_ptr() as *const u8, 48)
    };
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(48);
    &bytes[..len]
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[vfs] Started.");

    // Discover disk service
    let disk_tid = match lookup_service_with_retry(b"disk", 20) {
        Some(tid) => tid,
        None => {
            println!("[vfs] Disk service not found. Exiting.");
            syscall::sys_exit();
        }
    };
    println!("[vfs] Found disk at TID {}", disk_tid);

    // Allocate I/O buffer
    let buf_phys = match syscall::sys_phys_alloc(1) {
        Ok(p) => p,
        Err(()) => {
            println!("[vfs] Failed to alloc phys page.");
            syscall::sys_exit();
        }
    };
    if syscall::sys_map_phys(buf_phys, DISK_IO_BUF, 1).is_err() {
        println!("[vfs] Failed to map I/O buffer.");
        syscall::sys_exit();
    }

    // Find rootfs partition
    let part_lba = match DiskState::find_rootfs_partition(disk_tid, buf_phys) {
        Ok(lba) => lba,
        Err(()) => {
            println!("[vfs] Failed to find rootfs partition.");
            syscall::sys_exit();
        }
    };
    println!("[vfs] Rootfs partition at LBA {}", part_lba);

    // Read BPB
    if part_lba > 0 {
        if DiskState::raw_read_sector(disk_tid, buf_phys, part_lba).is_err() {
            println!("[vfs] Failed to read BPB.");
            syscall::sys_exit();
        }
    }
    let data = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
    let bpb = parse_bpb(data);
    println!(
        "[vfs] FAT32: bps={} spc={} reserved={} root={}",
        bpb.bytes_per_sector, bpb.sectors_per_cluster,
        bpb.reserved_sectors, bpb.root_cluster
    );

    let disk = DiskState {
        disk_tid,
        buf_phys,
        part_lba,
        bpb,
    };

    // Register with nameserver
    register_with_nameserver();

    // Service loop
    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        let sender = msg.sender;

        match msg.tag {
            TAG_OPEN => handle_open(&disk, sender, &msg),
            TAG_READ => handle_read(&disk, sender, &msg),
            TAG_CLOSE => handle_close(sender, &msg),
            TAG_READDIR => handle_readdir(&disk, sender, &msg),
            TAG_STAT => handle_stat(sender, &msg),
            _ => error_reply(sender, 0xFF),
        }
    }
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

/// TAG_OPEN: data[0..6] = path (up to 48 bytes, null-terminated)
/// Reply: tag=TAG_OK, data[0]=handle  OR  tag=TAG_ERROR, data[0]=error_code
fn handle_open(disk: &DiskState, sender: usize, msg: &Message) {
    let path = extract_path(&msg.data);
    if path.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    match resolve_path(disk, path) {
        Ok((cluster, size, is_dir)) => {
            match alloc_handle(sender, cluster, size, is_dir) {
                Some(handle) => {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [handle as u64, size as u64, is_dir as u64, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                None => error_reply(sender, ERR_TOO_MANY_OPEN),
            }
        }
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_READ: data[0]=handle, data[1]=phys_addr, data[2]=offset, data[3]=max_bytes
/// Reply: tag=TAG_OK, data[0]=bytes_read  OR  tag=TAG_ERROR, data[0]=error_code
fn handle_read(disk: &DiskState, sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let phys_addr = msg.data[1] as usize;
    let offset = msg.data[2] as u32;
    let max_bytes = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => {
            match read_file_data(disk, file, phys_addr, offset, max_bytes) {
                Ok(bytes_read) => {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [bytes_read as u64, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                Err(code) => error_reply(sender, code),
            }
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// TAG_CLOSE: data[0]=handle
/// Reply: tag=TAG_OK  OR  tag=TAG_ERROR
fn handle_close(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    if close_handle(handle, sender) {
        let reply = Message {
            sender: 0,
            tag: TAG_OK,
            data: [0; 6],
        };
        let _ = syscall::sys_reply(sender, &reply);
    } else {
        error_reply(sender, ERR_INVALID_HANDLE);
    }
}

/// TAG_READDIR: data[0]=handle (must be a directory), data[1]=entry_index
/// Reply: tag=TAG_OK, data[0..1]=name (11 bytes), data[2]=size, data[3]=flags, data[4]=cluster
///    OR: tag=TAG_ERROR with ERR_NOT_FOUND when no more entries
fn handle_readdir(disk: &DiskState, sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let index = msg.data[1] as u32;

    let dir_cluster = match get_handle(handle, sender) {
        Some(file) => {
            if !file.is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            file.first_cluster
        }
        None => {
            error_reply(sender, ERR_INVALID_HANDLE);
            return;
        }
    };

    match read_dir_entry(disk, dir_cluster, index) {
        Ok(Some((_cluster, name, size, is_dir, attr))) => {
            // Pack 11-byte name into 2 u64 words
            let mut name_bytes = [0u8; 16];
            name_bytes[..11].copy_from_slice(&name);
            let w0 = u64::from_le_bytes(name_bytes[0..8].try_into().unwrap());
            let w1 = u64::from_le_bytes(name_bytes[8..16].try_into().unwrap());

            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [
                    w0,
                    w1,
                    size as u64,
                    ((is_dir as u64) << 32) | (_cluster as u64),
                    attr as u64,
                    0,
                ],
            };
            let _ = syscall::sys_reply(sender, &reply);
        }
        Ok(None) => error_reply(sender, ERR_NOT_FOUND),
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_STAT: data[0]=handle
/// Reply: tag=TAG_OK, data[0]=size, data[1]=is_dir
fn handle_stat(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    match get_handle(handle, sender) {
        Some(file) => {
            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [
                    file.file_size as u64,
                    file.is_dir as u64,
                    file.first_cluster as u64,
                    0, 0, 0,
                ],
            };
            let _ = syscall::sys_reply(sender, &reply);
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[vfs] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
