#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod ext2;
pub mod ext2_alloc;
pub mod ext2_dir;

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

pub const PAGE_SIZE: usize = 4096;
const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;
const TAG_NS_LOOKUP: u64 = 2;

// Disk driver protocol
pub const TAG_READ_SECTOR: u64 = 1;
pub const TAG_WRITE_SECTOR: u64 = 2;
pub const TAG_DISK_OK: u64 = 0;
pub const TAG_READ_SECTORS: u64 = 4;

// VFS IPC tags
const TAG_OPEN: u64 = 1;
const TAG_READ: u64 = 2;
const TAG_CLOSE: u64 = 3;
const TAG_READDIR: u64 = 4;
const TAG_STAT: u64 = 5;
const TAG_WRITE: u64 = 6;
const TAG_CREATE: u64 = 7;
const TAG_READDIR_BULK: u64 = 8;
const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

// Error codes in reply data[0]
pub const ERR_NOT_FOUND: u64 = 1;
pub const ERR_INVALID_HANDLE: u64 = 2;
pub const ERR_IO: u64 = 3;
pub const ERR_TOO_MANY_OPEN: u64 = 4;
pub const ERR_INVALID_PATH: u64 = 5;
pub const ERR_NOT_DIR: u64 = 6;
pub const ERR_IS_DIR: u64 = 7;
pub const ERR_PERMISSION: u64 = 8;

// ---------------------------------------------------------------------------
// Filesystem type detection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum FsType {
    Fat32,
    Ext2,
}

static mut FS_TYPE: FsType = FsType::Fat32;
static mut EXT2_STATE: Option<ext2::Ext2State> = None;

fn ext2_state() -> &'static ext2::Ext2State {
    unsafe { EXT2_STATE.as_ref().unwrap() }
}

fn ext2_state_mut() -> &'static mut ext2::Ext2State {
    unsafe { EXT2_STATE.as_mut().unwrap() }
}

// Virtual addresses for temp mappings
pub const DISK_IO_BUF: usize = 0x86_0000_0000;
pub const CLIENT_BUF: usize = 0x87_0000_0000;
pub const CACHE_BUF_BASE: usize = 0x8A_0000_0000;
pub const SHMEM_BUF: usize = 0x8B_0000_0000;

// ---------------------------------------------------------------------------
// Sector cache
// ---------------------------------------------------------------------------

const CACHE_ENTRIES: usize = 256;
const HASH_BUCKETS: usize = 128;
const NONE: u16 = 0xFFFF; // sentinel for "no entry"

pub struct CacheEntry {
    valid: bool,
    used: bool,
    lba: u32,
    hash_next: u16, // next slot in hash chain, NONE = end
}

pub struct SectorCache {
    entries: [CacheEntry; CACHE_ENTRIES],
    buckets: [u16; HASH_BUCKETS],
    clock_hand: usize,
}

impl SectorCache {
    pub const fn new() -> Self {
        const EMPTY: CacheEntry = CacheEntry {
            valid: false,
            used: false,
            lba: 0,
            hash_next: NONE,
        };
        SectorCache {
            entries: [EMPTY; CACHE_ENTRIES],
            buckets: [NONE; HASH_BUCKETS],
            clock_hand: 0,
        }
    }

    /// Look up a sector in the cache via hash chain. O(1) average.
    pub fn lookup(&mut self, lba: u32) -> Option<usize> {
        let bucket = (lba as usize) % HASH_BUCKETS;
        let mut idx = self.buckets[bucket];
        while idx != NONE {
            let i = idx as usize;
            if self.entries[i].valid && self.entries[i].lba == lba {
                self.entries[i].used = true;
                return Some(i);
            }
            idx = self.entries[i].hash_next;
        }
        None
    }

    /// Find a victim slot using clock eviction and insert a new sector.
    /// `src` is the memory address containing the 512-byte sector data.
    /// Returns the slot index.
    pub fn insert(&mut self, lba: u32, src: usize) -> usize {
        // First check for an invalid (empty) slot
        for i in 0..CACHE_ENTRIES {
            if !self.entries[i].valid {
                self.fill_slot(i, lba, src);
                self.link(i);
                return i;
            }
        }
        // Clock eviction
        loop {
            let i = self.clock_hand;
            self.clock_hand = (self.clock_hand + 1) % CACHE_ENTRIES;
            if self.entries[i].used {
                self.entries[i].used = false;
            } else {
                self.unlink(i);
                self.fill_slot(i, lba, src);
                self.link(i);
                return i;
            }
        }
    }

    fn fill_slot(&mut self, idx: usize, lba: u32, src: usize) {
        // Copy from source address into cache slot
        unsafe {
            core::ptr::copy_nonoverlapping(
                src as *const u8,
                (CACHE_BUF_BASE + idx * 512) as *mut u8,
                512,
            );
        }
        self.entries[idx] = CacheEntry {
            valid: true,
            used: true,
            lba,
            hash_next: NONE,
        };
    }

    /// Prepend slot `idx` to its LBA's hash bucket chain.
    fn link(&mut self, idx: usize) {
        let bucket = (self.entries[idx].lba as usize) % HASH_BUCKETS;
        self.entries[idx].hash_next = self.buckets[bucket];
        self.buckets[bucket] = idx as u16;
    }

    /// Remove slot `idx` from its hash bucket chain.
    fn unlink(&mut self, idx: usize) {
        let bucket = (self.entries[idx].lba as usize) % HASH_BUCKETS;
        let target = idx as u16;
        if self.buckets[bucket] == target {
            self.buckets[bucket] = self.entries[idx].hash_next;
        } else {
            let mut prev = self.buckets[bucket];
            while prev != NONE {
                let p = prev as usize;
                if self.entries[p].hash_next == target {
                    self.entries[p].hash_next = self.entries[idx].hash_next;
                    break;
                }
                prev = self.entries[p].hash_next;
            }
        }
        self.entries[idx].hash_next = NONE;
    }

    /// Invalidate any cached copy of a given LBA.
    pub fn invalidate(&mut self, lba: u32) {
        if let Some(idx) = self.lookup(lba) {
            self.unlink(idx);
            self.entries[idx].valid = false;
        }
    }
}

pub static mut SECTOR_CACHE: SectorCache = SectorCache::new();

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

pub fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

pub fn read_u32(data: &[u8], off: usize) -> u32 {
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

    /// Read a sector through the cache. Returns a slice to cached data.
    fn cached_read_sector(&self, lba: u32) -> Result<&[u8], ()> {
        let abs_lba = self.part_lba + lba;
        let cache = unsafe { &mut SECTOR_CACHE };
        let idx = if let Some(i) = cache.lookup(abs_lba) {
            i
        } else {
            // Cache miss — read from disk into DISK_IO_BUF, then insert into cache
            Self::raw_read_sector(self.disk_tid, self.buf_phys, abs_lba)?;
            cache.insert(abs_lba, DISK_IO_BUF)
        };
        Ok(unsafe { core::slice::from_raw_parts((CACHE_BUF_BASE + idx * 512) as *const u8, 512) })
    }

    /// Read multiple consecutive sectors into DISK_IO_BUF (up to 8, fitting one 4K page).
    fn raw_read_sectors(disk_tid: usize, buf_phys: usize, start_lba: u32, count: u32) -> Result<(), ()> {
        let msg = Message {
            sender: 0,
            tag: TAG_READ_SECTORS,
            data: [start_lba as u64, buf_phys as u64, count as u64, 0, 0, 0],
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

    /// Prefetch consecutive sectors into the cache using a single multi-sector IPC call.
    fn prefetch_sectors(&self, start_lba: u32, count: u32) {
        let count = count.min(8) as usize;
        let cache = unsafe { &mut SECTOR_CACHE };

        // Check how many sectors are already cached
        let mut all_cached = true;
        for i in 0..count {
            let abs_lba = self.part_lba + start_lba + i as u32;
            if cache.lookup(abs_lba).is_none() {
                all_cached = false;
                break;
            }
        }
        if all_cached {
            return;
        }

        // Read all sectors in one IPC call
        let abs_start = self.part_lba + start_lba;
        if Self::raw_read_sectors(self.disk_tid, self.buf_phys, abs_start, count as u32).is_err() {
            return;
        }

        // Insert each sector into the cache
        for i in 0..count {
            let abs_lba = abs_start + i as u32;
            if cache.lookup(abs_lba).is_none() {
                cache.insert(abs_lba, DISK_IO_BUF + i * 512);
            }
        }
    }

    fn sector_data(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) }
    }

    fn fat_next(&self, cluster: u32) -> Option<u32> {
        let fat_byte_off = (cluster as usize) * 4;
        let sector_in_fat = fat_byte_off / 512;
        let offset_in_sector = fat_byte_off % 512;
        let lba = self.bpb.reserved_sectors + sector_in_fat as u32;
        let data = self.cached_read_sector(lba).ok()?;
        let next = read_u32(data, offset_in_sector) & 0x0FFF_FFFF;
        if next >= 0x0FFF_FFF8 { None } else { Some(next) }
    }

    fn cluster_start_lba(&self, cluster: u32) -> u32 {
        let data_start = self.bpb.reserved_sectors + self.bpb.num_fats * self.bpb.fat_size_32;
        data_start + (cluster - 2) * self.bpb.sectors_per_cluster
    }

    fn write_sector(&self, lba: u32) -> Result<(), ()> {
        let msg = Message {
            sender: 0,
            tag: TAG_WRITE_SECTOR,
            data: [
                (self.part_lba + lba) as u64,
                self.buf_phys as u64,
                0, 0, 0, 0,
            ],
        };
        let mut reply = Message::empty();
        if syscall::sys_call(self.disk_tid, &msg, &mut reply).is_err() {
            return Err(());
        }
        if reply.tag != TAG_DISK_OK {
            return Err(());
        }
        Ok(())
    }

    fn sector_data_mut(&self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) }
    }

    /// Write a FAT entry: set fat[cluster] = value.
    fn fat_set(&self, cluster: u32, value: u32) -> Result<(), ()> {
        let fat_byte_off = (cluster as usize) * 4;
        let sector_in_fat = fat_byte_off / 512;
        let offset_in_sector = fat_byte_off % 512;
        let lba = self.bpb.reserved_sectors + sector_in_fat as u32;

        // Read the FAT sector
        self.read_sector(lba).map_err(|_| ())?;

        // Modify the entry (preserve top 4 bits)
        let data = self.sector_data_mut();
        let old = read_u32(data, offset_in_sector);
        let new_val = (old & 0xF000_0000) | (value & 0x0FFF_FFFF);
        let bytes = new_val.to_le_bytes();
        data[offset_in_sector..offset_in_sector + 4].copy_from_slice(&bytes);

        // Write back
        self.write_sector(lba).map_err(|_| ())?;
        unsafe { SECTOR_CACHE.invalidate(self.part_lba + lba); }

        // Update second FAT copy if present (buffer still has modified sector)
        if self.bpb.num_fats > 1 {
            let lba2 = lba + self.bpb.fat_size_32;
            self.write_sector(lba2).map_err(|_| ())?;
            unsafe { SECTOR_CACHE.invalidate(self.part_lba + lba2); }
        }

        Ok(())
    }

    /// Allocate a free cluster. Marks it as EOF in the FAT.
    fn fat_alloc(&self) -> Result<u32, ()> {
        let total_data_clusters =
            (self.bpb.fat_size_32 * 512 / 4) as u32;
        // Scan FAT for a free entry (value == 0)
        for cluster in 2..total_data_clusters {
            let fat_byte_off = (cluster as usize) * 4;
            let sector_in_fat = fat_byte_off / 512;
            let offset_in_sector = fat_byte_off % 512;
            let lba = self.bpb.reserved_sectors + sector_in_fat as u32;

            let data = match self.cached_read_sector(lba) {
                Ok(d) => d,
                Err(()) => continue,
            };
            let val = read_u32(data, offset_in_sector) & 0x0FFF_FFFF;
            if val == 0 {
                // Mark as EOF
                self.fat_set(cluster, 0x0FFF_FFFF)?;
                return Ok(cluster);
            }
        }
        Err(()) // disk full
    }

    /// Extend a cluster chain by allocating a new cluster and linking it.
    fn fat_extend(&self, last_cluster: u32) -> Result<u32, ()> {
        let new_cluster = self.fat_alloc()?;
        self.fat_set(last_cluster, new_cluster)?;
        Ok(new_cluster)
    }

    /// Zero out a cluster's data sectors.
    fn zero_cluster(&self, cluster: u32) -> Result<(), ()> {
        let start_lba = self.cluster_start_lba(cluster);
        let data = self.sector_data_mut();
        for i in 0..512 {
            data[i] = 0;
        }
        for s in 0..self.bpb.sectors_per_cluster {
            self.write_sector(start_lba + s).map_err(|_| ())?;
            unsafe { SECTOR_CACHE.invalidate(self.part_lba + start_lba + s); }
        }
        Ok(())
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

enum FsFileData {
    Fat32 {
        first_cluster: u32,
        cur_cluster: u32,
        cur_cluster_offset: u32,
        dir_cluster: u32,
        fat_name: [u8; 11],
    },
    Ext2 {
        inode_num: u32,
        inode: ext2::Ext2Inode,
        parent_inode: u32,
    },
    None,
}

struct OpenFile {
    in_use: bool,
    owner_tid: usize,
    file_size: u32,
    is_dir: bool,
    writable: bool,
    read_offset: u32,
    fs: FsFileData,
}

static mut FILE_TABLE: [OpenFile; MAX_OPEN_FILES] = {
    const EMPTY: OpenFile = OpenFile {
        in_use: false,
        owner_tid: 0,
        file_size: 0,
        is_dir: false,
        writable: true,
        read_offset: 0,
        fs: FsFileData::None,
    };
    [EMPTY; MAX_OPEN_FILES]
};

fn alloc_handle_fat32(
    tid: usize,
    cluster: u32,
    size: u32,
    is_dir: bool,
    dir_cluster: u32,
    fat_name: &[u8; 11],
) -> Option<usize> {
    unsafe {
        for i in 0..MAX_OPEN_FILES {
            if !FILE_TABLE[i].in_use {
                FILE_TABLE[i] = OpenFile {
                    in_use: true,
                    owner_tid: tid,
                    file_size: size,
                    is_dir,
                    writable: true, // FAT32: no permission checks
                    read_offset: 0,
                    fs: FsFileData::Fat32 {
                        first_cluster: cluster,
                        cur_cluster: cluster,
                        cur_cluster_offset: 0,
                        dir_cluster,
                        fat_name: *fat_name,
                    },
                };
                return Some(i);
            }
        }
    }
    None
}

fn alloc_handle_ext2(
    tid: usize,
    inode_num: u32,
    inode: &ext2::Ext2Inode,
    parent_inode: u32,
    writable: bool,
) -> Option<usize> {
    unsafe {
        for i in 0..MAX_OPEN_FILES {
            if !FILE_TABLE[i].in_use {
                FILE_TABLE[i] = OpenFile {
                    in_use: true,
                    owner_tid: tid,
                    file_size: inode.i_size,
                    is_dir: inode.is_dir(),
                    writable,
                    read_offset: 0,
                    fs: FsFileData::Ext2 {
                        inode_num,
                        inode: *inode,
                        parent_inode,
                    },
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

/// Resolve a path like "/USR/BIN/HELLO.ELF" to (cluster, size, is_dir, parent_cluster, fat_name).
/// Paths use "/" separators. Leading "/" is optional.
fn resolve_path(
    disk: &DiskState,
    path: &[u8],
) -> Result<(u32, u32, bool, u32, [u8; 11]), u64> {
    let path = if !path.is_empty() && path[0] == b'/' {
        &path[1..]
    } else {
        path
    };

    if path.is_empty() {
        // Root directory
        let root_name = [b' '; 11];
        return Ok((disk.bpb.root_cluster, 0, true, 0, root_name));
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
                let root_name = [b' '; 11];
                return Ok((current_cluster, 0, true, 0, root_name));
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
                    return Ok((cluster, size, is_dir, current_cluster, target));
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
        disk.prefetch_sectors(start_lba, spc);
        for s in 0..spc {
            let sec_data = disk.cached_read_sector(start_lba + s).map_err(|_| ERR_IO)?;
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

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

    let (first_cluster, cur_cluster, cur_cluster_offset) = match &file.fs {
        FsFileData::Fat32 { first_cluster, cur_cluster, cur_cluster_offset, .. } =>
            (*first_cluster, *cur_cluster, *cur_cluster_offset),
        _ => return Err(ERR_IO),
    };

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
    if offset >= cur_cluster_offset && cur_cluster != 0 {
        cluster = cur_cluster;
        byte_pos = cur_cluster_offset;
    } else {
        cluster = first_cluster;
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
    if let FsFileData::Fat32 { cur_cluster: cc, cur_cluster_offset: co, .. } = &mut file.fs {
        *cc = cluster;
        *co = byte_pos;
    }

    // Prefetch all sectors in the current cluster
    let cluster_lba = disk.cluster_start_lba(cluster);
    disk.prefetch_sectors(cluster_lba, disk.bpb.sectors_per_cluster);

    let mut written = 0u32;

    while written < to_read {
        let offset_in_cluster = (offset + written) - byte_pos;
        let sector_in_cluster = offset_in_cluster / disk.bpb.bytes_per_sector;
        let offset_in_sector = offset_in_cluster % disk.bpb.bytes_per_sector;

        let lba = disk.cluster_start_lba(cluster) + sector_in_cluster;
        let sec_data = disk.cached_read_sector(lba).map_err(|_| ERR_IO)?;

        let copy_start = offset_in_sector as usize;
        let copy_len = (512 - copy_start).min((to_read - written) as usize);

        unsafe {
            core::ptr::copy_nonoverlapping(
                sec_data.as_ptr().add(copy_start),
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
                    if let FsFileData::Fat32 { cur_cluster: cc, cur_cluster_offset: co, .. } = &mut file.fs {
                        *cc = cluster;
                        *co = byte_pos;
                    }
                    // Prefetch next cluster's sectors
                    let next_lba = disk.cluster_start_lba(cluster);
                    disk.prefetch_sectors(next_lba, disk.bpb.sectors_per_cluster);
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
        disk.prefetch_sectors(start_lba, spc);
        for s in 0..spc {
            let sec_data = disk.cached_read_sector(start_lba + s).map_err(|_| ERR_IO)?;
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

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
// Create a new directory entry
// ---------------------------------------------------------------------------

/// Create a new file entry in a directory. Returns the first cluster of the new file.
fn create_dir_entry(
    disk: &DiskState,
    dir_cluster: u32,
    name: &[u8; 11],
    is_dir: bool,
) -> Result<u32, u64> {
    // Check if name already exists
    if let Ok(Some(_)) = find_entry(disk, dir_cluster, name) {
        return Err(ERR_INVALID_PATH); // already exists
    }

    // Allocate a cluster for the new file/dir
    let new_cluster = disk.fat_alloc().map_err(|_| ERR_IO)?;

    // Zero the new cluster
    disk.zero_cluster(new_cluster).map_err(|_| ERR_IO)?;

    // If creating a directory, write "." and ".." entries
    if is_dir {
        let start_lba = disk.cluster_start_lba(new_cluster);
        if disk.read_sector(start_lba).is_err() {
            return Err(ERR_IO);
        }
        let sec = disk.sector_data_mut();

        // "." entry — points to self
        sec[0..11].copy_from_slice(b".          ");
        sec[11] = 0x10; // directory attribute
        let cl_hi = ((new_cluster >> 16) & 0xFFFF) as u16;
        let cl_lo = (new_cluster & 0xFFFF) as u16;
        sec[20..22].copy_from_slice(&cl_hi.to_le_bytes());
        sec[26..28].copy_from_slice(&cl_lo.to_le_bytes());

        // ".." entry — points to parent
        sec[32..43].copy_from_slice(b"..         ");
        sec[43] = 0x10;
        let parent_cl = if dir_cluster == disk.bpb.root_cluster { 0 } else { dir_cluster };
        let p_hi = ((parent_cl >> 16) & 0xFFFF) as u16;
        let p_lo = (parent_cl & 0xFFFF) as u16;
        sec[52..54].copy_from_slice(&p_hi.to_le_bytes());
        sec[58..60].copy_from_slice(&p_lo.to_le_bytes());

        disk.write_sector(start_lba).map_err(|_| ERR_IO)?;
        unsafe { SECTOR_CACHE.invalidate(disk.part_lba + start_lba); }
    }

    // Find a free slot in the parent directory
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    loop {
        let start_lba = disk.cluster_start_lba(cluster);
        disk.prefetch_sectors(start_lba, spc);
        for s in 0..spc {
            let sec_data = disk.cached_read_sector(start_lba + s).map_err(|_| ERR_IO)?;
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                // Free slot: 0x00 (end of dir) or 0xE5 (deleted)
                if first_byte == 0x00 || first_byte == 0xE5 {
                    // Write the new entry
                    sec_buf[off..off + 11].copy_from_slice(name);
                    sec_buf[off + 11] = if is_dir { 0x10 } else { 0x20 }; // dir or archive
                    // Zero out remaining fields (timestamps etc.)
                    for i in 12..32 {
                        if i != 11 {
                            sec_buf[off + i] = 0;
                        }
                    }
                    // Set first cluster
                    let cl_hi = ((new_cluster >> 16) & 0xFFFF) as u16;
                    let cl_lo = (new_cluster & 0xFFFF) as u16;
                    sec_buf[off + 20..off + 22].copy_from_slice(&cl_hi.to_le_bytes());
                    sec_buf[off + 26..off + 28].copy_from_slice(&cl_lo.to_le_bytes());
                    // Size = 0 initially
                    sec_buf[off + 28..off + 32].copy_from_slice(&0u32.to_le_bytes());

                    // If this was end-of-dir (0x00), mark next slot as end if room
                    if first_byte == 0x00 && e + 1 < 16 {
                        sec_buf[(e + 1) * 32] = 0x00;
                    }

                    // Write sector back
                    let data = disk.sector_data_mut();
                    data.copy_from_slice(&sec_buf);
                    disk.write_sector(start_lba + s).map_err(|_| ERR_IO)?;
                    unsafe { SECTOR_CACHE.invalidate(disk.part_lba + start_lba + s); }

                    return Ok(new_cluster);
                }
            }
        }
        // Extend the directory with a new cluster
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => {
                let new_dir_cluster = disk.fat_extend(cluster).map_err(|_| ERR_IO)?;
                disk.zero_cluster(new_dir_cluster).map_err(|_| ERR_IO)?;
                cluster = new_dir_cluster;
                // Loop again — the zeroed cluster will have 0x00 entries
            }
        }
    }
}

/// Update the file size in its directory entry.
fn update_dir_entry_size(
    disk: &DiskState,
    dir_cluster: u32,
    name: &[u8; 11],
    new_size: u32,
) -> Result<(), u64> {
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    loop {
        let start_lba = disk.cluster_start_lba(cluster);
        disk.prefetch_sectors(start_lba, spc);
        for s in 0..spc {
            let sec_data = disk.cached_read_sector(start_lba + s).map_err(|_| ERR_IO)?;
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 {
                    return Err(ERR_NOT_FOUND);
                }
                if first_byte == 0xE5 {
                    continue;
                }
                let attr = sec_buf[off + 11];
                if attr & 0x0F == 0x0F || attr & 0x08 != 0 {
                    continue;
                }
                if &sec_buf[off..off + 11] == name {
                    sec_buf[off + 28..off + 32].copy_from_slice(&new_size.to_le_bytes());
                    let data = disk.sector_data_mut();
                    data.copy_from_slice(&sec_buf);
                    disk.write_sector(start_lba + s).map_err(|_| ERR_IO)?;
                    unsafe { SECTOR_CACHE.invalidate(disk.part_lba + start_lba + s); }
                    return Ok(());
                }
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }
    Err(ERR_NOT_FOUND)
}

// ---------------------------------------------------------------------------
// Write file data from client's physical page
// ---------------------------------------------------------------------------

/// Write up to `len` bytes to a file at `offset` from the client's physical page.
/// Returns bytes actually written.
fn write_file_data(
    disk: &DiskState,
    file: &mut OpenFile,
    client_phys: usize,
    offset: u32,
    len: u32,
) -> Result<u32, u64> {
    if file.is_dir {
        return Err(ERR_IS_DIR);
    }

    let first_cluster = match &file.fs {
        FsFileData::Fat32 { first_cluster, .. } => *first_cluster,
        _ => return Err(ERR_IO),
    };

    let to_write = len.min(PAGE_SIZE as u32);
    if to_write == 0 {
        return Ok(0);
    }

    // Map client's physical page
    if syscall::sys_map_phys(client_phys, CLIENT_BUF, 1).is_err() {
        return Err(ERR_IO);
    }

    let cluster_bytes = disk.bpb.sectors_per_cluster * disk.bpb.bytes_per_sector;

    // Navigate to the cluster containing `offset`, allocating as needed
    let mut cluster = first_cluster;
    let mut byte_pos: u32 = 0;

    // Skip clusters until we reach the one containing `offset`
    while byte_pos + cluster_bytes <= offset {
        match disk.fat_next(cluster) {
            Some(next) => {
                cluster = next;
                byte_pos += cluster_bytes;
            }
            None => {
                // Need to allocate more clusters to reach the offset
                let new = disk.fat_extend(cluster).map_err(|_| ERR_IO)?;
                disk.zero_cluster(new).map_err(|_| ERR_IO)?;
                cluster = new;
                byte_pos += cluster_bytes;
            }
        }
    }

    let mut written = 0u32;

    while written < to_write {
        let offset_in_cluster = (offset + written) - byte_pos;
        let sector_in_cluster = offset_in_cluster / disk.bpb.bytes_per_sector;
        let offset_in_sector = offset_in_cluster % disk.bpb.bytes_per_sector;

        let lba = disk.cluster_start_lba(cluster) + sector_in_cluster;

        // Read existing sector data (for partial-sector writes)
        if disk.read_sector(lba).is_err() {
            return Err(ERR_IO);
        }

        let copy_start = offset_in_sector as usize;
        let copy_len = (512 - copy_start).min((to_write - written) as usize);

        // Copy from client buffer into disk I/O buffer
        unsafe {
            core::ptr::copy_nonoverlapping(
                (CLIENT_BUF + written as usize) as *const u8,
                (DISK_IO_BUF + copy_start) as *mut u8,
                copy_len,
            );
        }

        // Write sector back to disk
        disk.write_sector(lba).map_err(|_| ERR_IO)?;
        unsafe { SECTOR_CACHE.invalidate(disk.part_lba + lba); }

        written += copy_len as u32;

        // Check if we need to move to next cluster
        let new_offset_in_cluster = offset_in_cluster + copy_len as u32;
        if new_offset_in_cluster >= cluster_bytes && written < to_write {
            match disk.fat_next(cluster) {
                Some(next) => {
                    cluster = next;
                    byte_pos += cluster_bytes;
                }
                None => {
                    let new = disk.fat_extend(cluster).map_err(|_| ERR_IO)?;
                    disk.zero_cluster(new).map_err(|_| ERR_IO)?;
                    cluster = new;
                    byte_pos += cluster_bytes;
                }
            }
        }
    }

    // Update cached position
    if let FsFileData::Fat32 { cur_cluster: cc, cur_cluster_offset: co, .. } = &mut file.fs {
        *cc = cluster;
        *co = byte_pos;
    }

    // Update file size if we wrote past the end
    let new_end = offset + written;
    if new_end > file.file_size {
        file.file_size = new_end;
    }

    Ok(written)
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
// Cache warmup
// ---------------------------------------------------------------------------

/// Prefetch FAT table sectors and root directory cluster into the sector cache.
/// This eliminates cold-miss IPC round-trips on the first readdir/ls.
fn warm_cache(disk: &DiskState) {
    let fat_start = disk.bpb.reserved_sectors;
    let fat_sectors = disk.bpb.fat_size_32.min(64); // cap at 64 sectors (32 KiB of FAT)

    // Prefetch FAT in 8-sector batches
    let mut lba = 0u32;
    while lba < fat_sectors {
        let batch = (fat_sectors - lba).min(8);
        disk.prefetch_sectors(fat_start + lba, batch);
        lba += batch;
    }

    // Prefetch root directory's first cluster
    let root_lba = disk.cluster_start_lba(disk.bpb.root_cluster);
    disk.prefetch_sectors(root_lba, disk.bpb.sectors_per_cluster.min(8));
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

    // Allocate sector cache buffer (32 pages = 128 KiB for 256 x 512-byte entries)
    // Allocate one page at a time since phys_alloc doesn't guarantee contiguity.
    let cache_pages = 32;
    for i in 0..cache_pages {
        let phys = match syscall::sys_phys_alloc(1) {
            Ok(p) => p,
            Err(()) => {
                println!("[vfs] Failed to alloc cache page.");
                syscall::sys_exit();
            }
        };
        if syscall::sys_map_phys(phys, CACHE_BUF_BASE + i * PAGE_SIZE, 1).is_err() {
            println!("[vfs] Failed to map cache buffer.");
            syscall::sys_exit();
        }
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

    // Detect filesystem type: check for ext2 magic at partition offset 1024 (sector 2)
    if DiskState::raw_read_sector(disk_tid, buf_phys, part_lba + 2).is_ok() {
        let sb_data = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let magic = read_u16(sb_data, 56);
        if magic == ext2::EXT2_MAGIC {
            println!("[vfs] ext2 detected.");
            match ext2::init_ext2(disk_tid, buf_phys, part_lba) {
                Ok(state) => {
                    println!(
                        "[vfs] ext2: blocks={} inodes={} block_size={} groups={}",
                        state.total_blocks, state.total_inodes,
                        state.block_size, state.num_block_groups
                    );
                    unsafe {
                        FS_TYPE = FsType::Ext2;
                        EXT2_STATE = Some(state);
                    }
                }
                Err(()) => {
                    println!("[vfs] ext2 init failed, falling back to FAT32.");
                }
            }
        }
    }

    // Create a dummy DiskState for FAT32 (needed even in ext2 mode for the service loop signature)
    let disk = if unsafe { FS_TYPE } == FsType::Fat32 {
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

        let d = DiskState { disk_tid, buf_phys, part_lba, bpb };
        warm_cache(&d);
        d
    } else {
        // Dummy — won't be used for ext2 path
        DiskState {
            disk_tid,
            buf_phys,
            part_lba,
            bpb: Bpb {
                bytes_per_sector: 512,
                sectors_per_cluster: 1,
                reserved_sectors: 0,
                num_fats: 0,
                fat_size_32: 0,
                root_cluster: 0,
            },
        }
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
            TAG_WRITE => handle_write(&disk, sender, &msg),
            TAG_CREATE => handle_create(&disk, sender, &msg),
            TAG_READDIR_BULK => handle_readdir_bulk(&disk, sender, &msg),
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
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_open_ext2(sender, msg);
        return;
    }

    let path = extract_path(&msg.data);
    if path.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    // Trailing slash means the caller expects a directory
    let trailing_slash = path.len() > 1 && path[path.len() - 1] == b'/';

    match resolve_path(disk, path) {
        Ok((cluster, size, is_dir, dir_cluster, fat_name)) => {
            if trailing_slash && !is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            match alloc_handle_fat32(sender, cluster, size, is_dir, dir_cluster, &fat_name) {
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
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_read_ext2(sender, msg);
        return;
    }

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
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_readdir_ext2(sender, msg);
        return;
    }

    let handle = msg.data[0] as usize;
    let index = msg.data[1] as u32;

    let dir_cluster = match get_handle(handle, sender) {
        Some(file) => {
            if !file.is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            match &file.fs {
                FsFileData::Fat32 { first_cluster, .. } => *first_cluster,
                _ => { error_reply(sender, ERR_IO); return; }
            }
        }
        None => {
            error_reply(sender, ERR_INVALID_HANDLE);
            return;
        }
    };

    match read_dir_entry(disk, dir_cluster, index) {
        Ok(Some((_cluster, name, size, is_dir, attr))) => {
            // Pack 11-byte FAT name into 4 u64 words (32 bytes, padded with zeros)
            let mut name_bytes = [0u8; 32];
            name_bytes[..11].copy_from_slice(&name);
            let w0 = u64::from_le_bytes(name_bytes[0..8].try_into().unwrap());
            let w1 = u64::from_le_bytes(name_bytes[8..16].try_into().unwrap());
            let w2 = u64::from_le_bytes(name_bytes[16..24].try_into().unwrap());
            let w3 = u64::from_le_bytes(name_bytes[24..32].try_into().unwrap());

            // data[4]: name_len(8) | attr(8) | ...
            let packed = 11u64 | ((attr as u64) << 8);

            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [w0, w1, w2, w3, packed, size as u64],
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
            let id = match &file.fs {
                FsFileData::Fat32 { first_cluster, .. } => *first_cluster as u64,
                FsFileData::Ext2 { inode_num, .. } => *inode_num as u64,
                FsFileData::None => 0,
            };
            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [
                    file.file_size as u64,
                    file.is_dir as u64,
                    id,
                    0, 0, 0,
                ],
            };
            let _ = syscall::sys_reply(sender, &reply);
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// TAG_WRITE: data[0]=handle, data[1]=phys_addr, data[2]=offset, data[3]=len
/// Reply: tag=TAG_OK, data[0]=bytes_written  OR  tag=TAG_ERROR, data[0]=error_code
fn handle_write(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_write_ext2(sender, msg);
        return;
    }

    let handle = msg.data[0] as usize;
    let phys_addr = msg.data[1] as usize;
    let offset = msg.data[2] as u32;
    let len = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => {
            let (dir_cluster, fat_name) = match &file.fs {
                FsFileData::Fat32 { dir_cluster, fat_name, .. } => (*dir_cluster, *fat_name),
                _ => { error_reply(sender, ERR_IO); return; }
            };
            match write_file_data(disk, file, phys_addr, offset, len) {
                Ok(bytes_written) => {
                    // Update directory entry with new size
                    let new_size = file.file_size;
                    let _ = update_dir_entry_size(disk, dir_cluster, &fat_name, new_size);
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [bytes_written as u64, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                Err(code) => error_reply(sender, code),
            }
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// TAG_CREATE: data[0..6] = path (up to 48 bytes, null-terminated)
///   Last component is the new file/dir name. Intermediate dirs must exist.
///   If data[5] bit 0 is set, create a directory.
/// Reply: tag=TAG_OK, data[0]=handle, data[1]=0 (size)  OR  tag=TAG_ERROR
fn handle_create(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_create_ext2(sender, msg);
        return;
    }

    // data[5] is used for flags — extract before treating data as path
    let flags = msg.data[5];
    let is_dir = flags & 1 != 0;

    let path = extract_path(&msg.data);
    if path.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    // Split path into parent + final component
    let path_trimmed = if !path.is_empty() && path[0] == b'/' {
        &path[1..]
    } else {
        path
    };

    // Find last '/'
    let (parent_path, file_name) = match path_trimmed.iter().rposition(|&b| b == b'/') {
        Some(pos) => (&path[..pos + 1], &path_trimmed[pos + 1..]),
        None => (b"/" as &[u8], path_trimmed),
    };

    if file_name.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    // Resolve parent directory
    let parent_cluster = match resolve_path(disk, parent_path) {
        Ok((cluster, _, is_parent_dir, _, _)) => {
            if !is_parent_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            cluster
        }
        Err(code) => {
            error_reply(sender, code);
            return;
        }
    };

    // Convert filename to FAT 8.3
    let mut fat_name = [0u8; 11];
    to_fat83(file_name, &mut fat_name);

    // Create the directory entry
    match create_dir_entry(disk, parent_cluster, &fat_name, is_dir) {
        Ok(new_cluster) => {
            match alloc_handle_fat32(sender, new_cluster, 0, is_dir, parent_cluster, &fat_name) {
                Some(handle) => {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [handle as u64, 0, is_dir as u64, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                None => error_reply(sender, ERR_TOO_MANY_OPEN),
            }
        }
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_READDIR_BULK: data[0]=handle, data[1]=shmem_handle
/// VFS maps shmem, fills with packed 24-byte entries, replies with count.
/// Entry format: [0..11] name, [11] attr, [12..16] size LE, [16..20] cluster LE, [20..24] pad
fn handle_readdir_bulk(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_readdir_bulk_ext2(sender, msg);
        return;
    }

    let handle = msg.data[0] as usize;
    let shmem_handle = msg.data[1] as usize;

    let dir_cluster = match get_handle(handle, sender) {
        Some(file) => {
            if !file.is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            match &file.fs {
                FsFileData::Fat32 { first_cluster, .. } => *first_cluster,
                _ => { error_reply(sender, ERR_IO); return; }
            }
        }
        None => {
            error_reply(sender, ERR_INVALID_HANDLE);
            return;
        }
    };

    // Map the shared memory page
    if syscall::sys_shmem_map(shmem_handle, SHMEM_BUF).is_err() {
        error_reply(sender, ERR_IO);
        return;
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(SHMEM_BUF as *mut u8, 4096) };
    let max_entries = 4096 / 64; // 64
    let mut count: u32 = 0;

    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;

    'outer: loop {
        let start_lba = disk.cluster_start_lba(cluster);
        disk.prefetch_sectors(start_lba, spc);
        for s in 0..spc {
            let sec_data = match disk.cached_read_sector(start_lba + s) {
                Ok(d) => d,
                Err(_) => break 'outer,
            };
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 {
                    break 'outer;
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

                if (count as usize) >= max_entries {
                    break 'outer;
                }

                // 64-byte entry: 48 name + 1 name_len + 1 attr + 2 pad + 4 size + 4 cluster + 4 pad
                let base = (count as usize) * 64;
                buf[base..base + 48].fill(0);
                buf[base..base + 11].copy_from_slice(&sec_buf[off..off + 11]);
                buf[base + 48] = 11; // name_len
                buf[base + 49] = attr;
                let size = read_u32(&sec_buf, off + 28);
                buf[base + 52..base + 56].copy_from_slice(&size.to_le_bytes());
                let hi = read_u16(&sec_buf, off + 20) as u32;
                let lo = read_u16(&sec_buf, off + 26) as u32;
                let entry_cluster = (hi << 16) | lo;
                buf[base + 56..base + 60].copy_from_slice(&entry_cluster.to_le_bytes());
                buf[base + 60..base + 64].fill(0);
                count += 1;
            }
        }
        match disk.fat_next(cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }

    // Unmap the shared memory before replying — client will destroy the region
    let _ = syscall::sys_shmem_unmap(shmem_handle, SHMEM_BUF);

    let reply = Message {
        sender: 0,
        tag: TAG_OK,
        data: [count as u64, 0, 0, 0, 0, 0],
    };
    let _ = syscall::sys_reply(sender, &reply);
}

// ---------------------------------------------------------------------------
// ext2 IPC handlers
// ---------------------------------------------------------------------------

fn get_sender_uid_gid(sender: usize) -> (u32, u32) {
    syscall::sys_get_tuid(sender).unwrap_or((0, 0))
}

fn handle_open_ext2(sender: usize, msg: &Message) {
    let path = extract_path(&msg.data);
    if path.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    let trailing_slash = path.len() > 1 && path[path.len() - 1] == b'/';
    let (uid, gid) = get_sender_uid_gid(sender);
    let e2 = ext2_state();

    match ext2_dir::resolve_path(e2, path, uid, gid) {
        Ok((inode_num, inode, parent_ino)) => {
            if trailing_slash && !inode.is_dir() {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            // Check read permission
            if !ext2::check_permission(&inode, uid, gid, 4) {
                error_reply(sender, ERR_PERMISSION);
                return;
            }
            let writable = ext2::check_permission(&inode, uid, gid, 2);
            match alloc_handle_ext2(sender, inode_num, &inode, parent_ino, writable) {
                Some(handle) => {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [
                            handle as u64,
                            inode.i_size as u64,
                            inode.is_dir() as u64,
                            0, 0, 0,
                        ],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                None => error_reply(sender, ERR_TOO_MANY_OPEN),
            }
        }
        Err(code) => error_reply(sender, code),
    }
}

fn handle_read_ext2(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let phys_addr = msg.data[1] as usize;
    let offset = msg.data[2] as u32;
    let max_bytes = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => {
            let inode = match &file.fs {
                FsFileData::Ext2 { inode, .. } => *inode,
                _ => { error_reply(sender, ERR_IO); return; }
            };
            let e2 = ext2_state();
            match ext2::read_file_data(e2, &inode, phys_addr, offset, max_bytes) {
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

fn handle_write_ext2(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let phys_addr = msg.data[1] as usize;
    let offset = msg.data[2] as u32;
    let len = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => {
            if !file.writable {
                error_reply(sender, ERR_PERMISSION);
                return;
            }
            let (inode_num, mut inode, _parent_inode) = match &file.fs {
                FsFileData::Ext2 { inode_num, inode, parent_inode } =>
                    (*inode_num, *inode, *parent_inode),
                _ => { error_reply(sender, ERR_IO); return; }
            };
            let e2 = ext2_state_mut();
            match ext2::write_file_data(e2, &mut inode, inode_num, phys_addr, offset, len) {
                Ok(bytes_written) => {
                    // Update cached inode and file size in handle
                    file.file_size = inode.i_size;
                    if let FsFileData::Ext2 { inode: ref mut ino, .. } = &mut file.fs {
                        *ino = inode;
                    }
                    let reply = Message {
                        sender: 0,
                        tag: TAG_OK,
                        data: [bytes_written as u64, 0, 0, 0, 0, 0],
                    };
                    let _ = syscall::sys_reply(sender, &reply);
                }
                Err(code) => error_reply(sender, code),
            }
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

fn handle_readdir_ext2(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let index = msg.data[1] as u32;

    let dir_inode = match get_handle(handle, sender) {
        Some(file) => {
            if !file.is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            match &file.fs {
                FsFileData::Ext2 { inode, .. } => *inode,
                _ => { error_reply(sender, ERR_IO); return; }
            }
        }
        None => {
            error_reply(sender, ERR_INVALID_HANDLE);
            return;
        }
    };

    let e2 = ext2_state();
    match ext2_dir::read_dir_entry(e2, &dir_inode, index) {
        Ok(Some(entry)) => {
            let is_dir = entry.file_type == ext2::FT_DIR;
            let attr: u8 = if is_dir { 0x10 } else { 0x20 };
            let name_len = entry.name_len.min(32) as u8; // 32 bytes fit in 4 u64 words

            // Pack name into 4 u64 words (32 bytes)
            let mut name_bytes = [0u8; 32];
            name_bytes[..name_len as usize].copy_from_slice(&entry.name[..name_len as usize]);
            let w0 = u64::from_le_bytes(name_bytes[0..8].try_into().unwrap());
            let w1 = u64::from_le_bytes(name_bytes[8..16].try_into().unwrap());
            let w2 = u64::from_le_bytes(name_bytes[16..24].try_into().unwrap());
            let w3 = u64::from_le_bytes(name_bytes[24..32].try_into().unwrap());

            let packed = name_len as u64 | ((attr as u64) << 8);

            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [w0, w1, w2, w3, packed, entry.file_size as u64],
            };
            let _ = syscall::sys_reply(sender, &reply);
        }
        Ok(None) => error_reply(sender, ERR_NOT_FOUND),
        Err(code) => error_reply(sender, code),
    }
}

fn handle_readdir_bulk_ext2(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let shmem_handle = msg.data[1] as usize;

    let dir_inode = match get_handle(handle, sender) {
        Some(file) => {
            if !file.is_dir {
                error_reply(sender, ERR_NOT_DIR);
                return;
            }
            match &file.fs {
                FsFileData::Ext2 { inode, .. } => *inode,
                _ => { error_reply(sender, ERR_IO); return; }
            }
        }
        None => {
            error_reply(sender, ERR_INVALID_HANDLE);
            return;
        }
    };

    // Map the shared memory page
    if syscall::sys_shmem_map(shmem_handle, SHMEM_BUF).is_err() {
        error_reply(sender, ERR_IO);
        return;
    }

    let buf = unsafe { core::slice::from_raw_parts_mut(SHMEM_BUF as *mut u8, 4096) };
    let max_entries = 4096 / 64; // 64
    let mut count: u32 = 0;
    let e2 = ext2_state();

    // Iterate directory entries
    let mut idx = 0u32;
    loop {
        if count as usize >= max_entries {
            break;
        }
        match ext2_dir::read_dir_entry(e2, &dir_inode, idx) {
            Ok(Some(entry)) => {
                // 64-byte entry: 48 name + 1 name_len + 1 attr + 2 pad + 4 size + 4 cluster + 4 pad
                let base = (count as usize) * 64;
                buf[base..base + 48].fill(0);
                let copy_len = entry.name_len.min(48);
                buf[base..base + copy_len].copy_from_slice(&entry.name[..copy_len]);
                buf[base + 48] = copy_len as u8;

                let attr: u8 = if entry.file_type == ext2::FT_DIR { 0x10 } else { 0x20 };
                buf[base + 49] = attr;
                buf[base + 50..base + 52].fill(0);
                buf[base + 52..base + 56].copy_from_slice(&entry.file_size.to_le_bytes());
                buf[base + 56..base + 60].copy_from_slice(&entry.inode_num.to_le_bytes());
                buf[base + 60..base + 64].fill(0);
                count += 1;
                idx += 1;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let _ = syscall::sys_shmem_unmap(shmem_handle, SHMEM_BUF);

    let reply = Message {
        sender: 0,
        tag: TAG_OK,
        data: [count as u64, 0, 0, 0, 0, 0],
    };
    let _ = syscall::sys_reply(sender, &reply);
}

fn handle_create_ext2(sender: usize, msg: &Message) {
    let flags = msg.data[5];
    let is_dir = flags & 1 != 0;

    // Limit to 40 bytes — data[5] holds flags, not path data
    let full_path = extract_path(&msg.data);
    let path = if full_path.len() > 40 { &full_path[..40] } else { full_path };
    if path.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    let path_trimmed = if !path.is_empty() && path[0] == b'/' {
        &path[1..]
    } else {
        path
    };

    // Split into parent path + file name
    let (parent_path, file_name) = match path_trimmed.iter().rposition(|&b| b == b'/') {
        Some(pos) => (&path[..pos + 1], &path_trimmed[pos + 1..]),
        None => (b"/" as &[u8], path_trimmed),
    };

    if file_name.is_empty() {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    let (uid, gid) = get_sender_uid_gid(sender);
    let e2 = ext2_state_mut();

    // Resolve parent directory
    let (parent_ino, mut parent_inode, _grandparent) =
        match ext2_dir::resolve_path(e2, parent_path, uid, gid) {
            Ok(result) => result,
            Err(code) => { error_reply(sender, code); return; }
        };

    if !parent_inode.is_dir() {
        error_reply(sender, ERR_NOT_DIR);
        return;
    }

    // Check write+execute permission on parent directory
    if !ext2::check_permission(&parent_inode, uid, gid, 3) {
        error_reply(sender, ERR_PERMISSION);
        return;
    }

    // Check if name already exists
    if let Ok(Some(_)) = ext2_dir::find_entry(e2, &parent_inode, file_name) {
        error_reply(sender, ERR_INVALID_PATH);
        return;
    }

    // Allocate new inode
    let new_ino = match ext2_alloc::alloc_inode(e2) {
        Ok(ino) => ino,
        Err(code) => { error_reply(sender, code); return; }
    };

    // Initialize the inode
    let mode = if is_dir {
        ext2::S_IFDIR | 0o755
    } else {
        ext2::S_IFREG | 0o644
    };

    let mut new_inode = ext2::Ext2Inode::empty();
    new_inode.i_mode = mode;
    new_inode.i_uid = uid as u16;
    new_inode.i_gid = gid as u16;
    new_inode.i_links_count = if is_dir { 2 } else { 1 };

    if is_dir {
        // Allocate a block for the directory and write . and .. entries
        let block = match ext2_alloc::alloc_block(e2) {
            Ok(b) => b,
            Err(code) => { error_reply(sender, code); return; }
        };
        new_inode.i_block[0] = block;
        new_inode.i_size = e2.block_size;
        new_inode.i_blocks = e2.block_size / 512;

        if ext2_dir::init_dir_block(e2, block, new_ino, parent_ino).is_err() {
            error_reply(sender, ERR_IO);
            return;
        }

        // Increment parent's link count (for ".." entry)
        parent_inode.i_links_count += 1;
        let _ = ext2::write_inode(e2, parent_ino, &parent_inode);

        // Increment bg_used_dirs_count for the block group containing the new inode
        let group = (new_ino - 1) / e2.inodes_per_group;
        e2.bgd_table[group as usize].bg_used_dirs_count += 1;
        let _ = ext2::flush_bgd(e2, group);
    }

    // Write the new inode to disk
    if ext2::write_inode(e2, new_ino, &new_inode).is_err() {
        error_reply(sender, ERR_IO);
        return;
    }

    // Create directory entry in parent
    let file_type = if is_dir { ext2::FT_DIR } else { ext2::FT_REG_FILE };
    if ext2_dir::create_dir_entry(e2, parent_ino, &mut parent_inode, file_name, new_ino, file_type)
        .is_err()
    {
        error_reply(sender, ERR_IO);
        return;
    }

    // Allocate handle
    match alloc_handle_ext2(sender, new_ino, &new_inode, parent_ino, true) {
        Some(handle) => {
            let reply = Message {
                sender: 0,
                tag: TAG_OK,
                data: [handle as u64, 0, is_dir as u64, 0, 0, 0],
            };
            let _ = syscall::sys_reply(sender, &reply);
        }
        None => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[vfs] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
