#![no_std]
#![no_main]
#![allow(dead_code)]
#![allow(static_mut_refs)]

pub mod ext2;
pub mod ext2_alloc;
pub mod ext2_dir;
pub mod ext4;
pub mod csum;
pub mod cwd;
pub mod devices;
pub mod disk;
pub mod ext2_ops;
pub mod handles;
pub mod journal;
pub mod locks;
pub mod pager;
pub mod protocol;

pub use protocol::*;
use handles::{FsFileData, OpenFile};

use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::nameserver;
use quark_rt::{println, syscall};

use quark_rt::manifest::CapReq;

// A server, and nothing else. It used to hold all of physical memory in order
// to map the page each client named for its data, and frames of its own for
// its buffers; clients lend their buffers with the call now, and the buffers
// here are ordinary memory.
quark_rt::manifest!([
    CapReq::priority(quark_rt::syscall::PRIO_SERVER),
]);

pub const PAGE_SIZE: usize = 4096;

// Disk driver protocol
pub const TAG_READ_SECTOR: u64 = 1;
pub const TAG_WRITE_SECTOR: u64 = 2;
pub const TAG_DISK_OK: u64 = 0;
pub const TAG_READ_SECTORS: u64 = 4;

// The VFS protocol's own numbers live in `protocol`.

// ---------------------------------------------------------------------------
// Filesystem type detection
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum FsType {
    Fat32,
    Ext2,
}

static mut FS_TYPE: FsType = FsType::Fat32;
/// The journal, if the mounted filesystem has one.
///
/// A global for the same reason as the filesystem state: the sector read and
/// write paths that have to consult it are free functions several layers below
/// anything holding a reference.
static mut JOURNAL: journal::Journal = journal::Journal::empty();

pub fn journal_ref() -> &'static journal::Journal {
    unsafe { &*core::ptr::addr_of!(JOURNAL) }
}

pub fn journal_mut() -> &'static mut journal::Journal {
    unsafe { &mut *core::ptr::addr_of_mut!(JOURNAL) }
}

/// The mounted filesystem's state.
///
/// A flat static rather than an `Option`, and never assigned as a whole: it
/// holds the entire block group descriptor table, so moving one through a
/// local overflows this server's stack. `FS_TYPE` says whether it is mounted.
static mut EXT2_STATE: ext2::Ext2State = ext2::Ext2State::empty();

fn ext2_state() -> &'static ext2::Ext2State {
    unsafe { &*core::ptr::addr_of!(EXT2_STATE) }
}

fn ext2_state_mut() -> &'static mut ext2::Ext2State {
    unsafe { &mut *core::ptr::addr_of_mut!(EXT2_STATE) }
}

// Where this server keeps its buffers.
pub const DISK_IO_BUF: usize = 0x86_0000_0000;
/// A page of this server's own that file data passes through on its way to or
/// from the buffer a client lent: filled and then copied out for a read,
/// copied in and then written for a write.
pub const CLIENT_BUF: usize = 0x87_0000_0000;
pub const CACHE_BUF_BASE: usize = 0x8A_0000_0000;
/// Pages behind the sector cache: 256 sectors of 512 bytes.
const CACHE_PAGES: usize = 32;

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

    /// Drop everything. Used when an abandoned transaction means the cache
    /// may hold writes that are never going to reach the disk.
    pub fn flush_all(&mut self) {
        for i in 0..CACHE_ENTRIES {
            if self.entries[i].valid {
                self.invalidate(self.entries[i].lba);
            }
        }
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
    part_lba: u32,
    bpb: Bpb,
}

impl DiskState {
    fn raw_read_sector(disk_tid: usize, lba: u32) -> Result<(), ()> {
        disk::read(disk_tid, lba, 1)
    }

    fn read_sector(&self, lba: u32) -> Result<(), ()> {
        Self::raw_read_sector(self.disk_tid, self.part_lba + lba)
    }

    /// Read a sector through the cache. Returns a slice to cached data.
    fn cached_read_sector(&self, lba: u32) -> Result<&[u8], ()> {
        let abs_lba = self.part_lba + lba;
        let cache = unsafe { &mut SECTOR_CACHE };
        let idx = if let Some(i) = cache.lookup(abs_lba) {
            i
        } else {
            // Cache miss — read from disk into DISK_IO_BUF, then insert into cache
            Self::raw_read_sector(self.disk_tid, abs_lba)?;
            cache.insert(abs_lba, DISK_IO_BUF)
        };
        Ok(unsafe { core::slice::from_raw_parts((CACHE_BUF_BASE + idx * 512) as *const u8, 512) })
    }

    /// Read multiple consecutive sectors into DISK_IO_BUF (up to 8, fitting one 4K page).
    fn raw_read_sectors(disk_tid: usize, start_lba: u32, count: u32) -> Result<(), ()> {
        disk::read(disk_tid, start_lba, count)
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
        if Self::raw_read_sectors(self.disk_tid, abs_start, count as u32).is_err() {
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
        disk::write(self.disk_tid, self.part_lba + lba)
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

    fn find_rootfs_partition(disk_tid: usize) -> Result<u32, ()> {
        Self::raw_read_sector(disk_tid, 0)?;
        let sec0 = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        let has_mbr = sec0[510] == 0x55 && sec0[511] == 0xAA;
        let bps = read_u16(sec0, 11);
        let is_fat = bps == 512 || bps == 1024 || bps == 2048 || bps == 4096;

        if !has_mbr || is_fat {
            return Ok(0);
        }

        // Read GPT header (LBA 1)
        Self::raw_read_sector(disk_tid, 1)?;
        let hdr = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };

        if &hdr[0..8] != b"EFI PART" {
            // Try MBR partition 1
            Self::raw_read_sector(disk_tid, 0)?;
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
        Self::raw_read_sector(disk_tid, entry_start_lba)?;
        let entries = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let entries_per_sector = 512 / entry_size as usize;
        let part_idx = 1;
        let sector_of_entry = part_idx / entries_per_sector;
        let offset_in_sector = (part_idx % entries_per_sector) * entry_size as usize;

        if sector_of_entry > 0 {
            Self::raw_read_sector(disk_tid, entry_start_lba + sector_of_entry as u32)?;
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
// Open file table (see `handles`)
// ---------------------------------------------------------------------------

/// What this caller may do with an inode, as the rwx bits `access(2)` asks
/// about: 4 read, 2 write, 1 execute.
///
/// The client asks the server rather than working it out from a mode, because
/// the answer depends on the file's owner and the caller's identity and the
/// server is the only party that knows both. A client that recomputed it would
/// be keeping a second copy of this system's permission policy.
fn access_bits(inode: &ext2::Ext2Inode, uid: u32, gid: u32) -> u64 {
    let mut bits = 0;
    for probe in [4u16, 2, 1] {
        if ext2::check_permission(inode, uid, gid, probe) {
            bits |= probe as u64;
        }
    }
    bits
}

/// FAT has no owners and no mode bits, and this server checks nothing on it.
/// Saying "anyone may do anything" is a description of what will actually
/// happen rather than a default standing in for information we lost.
const FAT_FILE_MODE: u64 = 0o100777;
const FAT_DIR_MODE: u64 = 0o040777;
const FAT_ACCESS: u64 = 7;
/// A FAT32 cluster's size, which is what a file's blocks come in.
static mut FAT_CLUSTER_BYTES: u32 = 512;

fn alloc_handle_fat32(
    tid: usize,
    cluster: u32,
    size: u32,
    is_dir: bool,
    dir_cluster: u32,
    fat_name: &[u8; 11],
) -> Option<usize> {
    handles::alloc(OpenFile {
        in_use: true,
        owner: space_of(tid),
        file_size: size,
        is_dir,
        writable: true, // FAT32: no permission checks
        link: false,
        read_offset: 0,
        fs: FsFileData::Fat32 {
            first_cluster: cluster,
            cur_cluster: cluster,
            cur_cluster_offset: 0,
            dir_cluster,
            fat_name: *fat_name,
        },
    })
}

/// The program a caller belongs to, or 0 if it has none (and so owns nothing).
fn space_of(sender: usize) -> u64 {
    syscall::sys_task_space(sender).unwrap_or(0)
}

/// `sender`'s program's handle `handle`.
fn get_handle(handle: usize, sender: usize) -> Option<&'static mut OpenFile> {
    handles::get(handle, space_of(sender))
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
// Read file data into CLIENT_BUF
// ---------------------------------------------------------------------------

/// Read up to `max_bytes` (at most a page) from a file at `offset` into
/// `CLIENT_BUF`. Returns bytes actually read.
fn read_file_data(
    disk: &DiskState,
    file: &mut OpenFile,
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
// Write file data from CLIENT_BUF
// ---------------------------------------------------------------------------

/// Write up to `len` bytes (at most a page) from `CLIENT_BUF` to a file at
/// `offset`. Returns bytes actually written.
fn write_file_data(
    disk: &DiskState,
    file: &mut OpenFile,
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

fn error_reply(sender: usize, err_code: u64) {
    let reply = Message {
        sender: 0,
        tag: TAG_ERROR,
        data: [err_code, 0, 0, 0, 0, 0],
    };
    let _ = syscall::sys_reply(sender, &reply);
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
    let disk_tid = match nameserver::lookup_retry(b"disk", 20) {
        Some(tid) => tid,
        None => {
            println!("[vfs] Disk service not found. Exiting.");
            syscall::sys_exit();
        }
    };
    println!("[vfs] Found disk at TID {}", disk_tid);

    // The page every sector passes through, and the sector cache (32 pages,
    // 256 sectors). Ordinary memory: the disk driver is lent the one and
    // never sees the other.
    if syscall::sys_mmap(DISK_IO_BUF, 1).is_err()
        || syscall::sys_mmap(CLIENT_BUF, 1).is_err()
        || syscall::sys_mmap(protocol::PATH_BUF, protocol::PATH_BUF_PAGES).is_err()
        || syscall::sys_mmap(CACHE_BUF_BASE, CACHE_PAGES).is_err()
    {
        println!("[vfs] No memory for disk buffers.");
        syscall::sys_exit();
    }

    // Find rootfs partition
    let part_lba = match DiskState::find_rootfs_partition(disk_tid) {
        Ok(lba) => lba,
        Err(()) => {
            println!("[vfs] Failed to find rootfs partition.");
            syscall::sys_exit();
        }
    };
    println!("[vfs] Rootfs partition at LBA {}", part_lba);

    // Detect filesystem type: check for ext2 magic at partition offset 1024 (sector 2)
    if DiskState::raw_read_sector(disk_tid, part_lba + 2).is_ok() {
        let sb_data = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let magic = read_u16(sb_data, 56);
        if magic == ext2::EXT2_MAGIC {
            match ext2::init_ext2(ext2_state_mut(), disk_tid, part_lba) {
                Ok(()) => {
                    let state = ext2_state();
                    println!(
                        "[vfs] {} detected: blocks={} inodes={} block_size={} groups={}{}",
                        if state.is_ext4() { "ext4" } else { "ext2" },
                        state.total_blocks, state.total_inodes,
                        state.block_size, state.num_block_groups,
                        if state.read_only { " (read-only)" } else { "" }
                    );
                    unsafe { FS_TYPE = FsType::Ext2 };

                    // The journal, and whatever it says did not finish.
                    if journal::map_buffers().is_err() {
                        println!("[vfs] could not map journal buffers; mounting read-only");
                        ext2_state_mut().read_only = true;
                    } else {
                        match journal::load(journal_mut(), ext2_state()) {
                            Ok(true) => {
                                if let Err(e) = journal::recover(journal_mut(), ext2_state()) {
                                    println!("[vfs] journal recovery failed ({}); read-only", e);
                                    ext2_state_mut().read_only = true;
                                } else if let Err(e) =
                                    journal::checkpoint_done(journal_mut(), ext2_state())
                                {
                                    println!("[vfs] could not clear the journal ({}); read-only", e);
                                    ext2_state_mut().read_only = true;
                                } else {
                                    println!("[vfs] journal ready");
                                }
                            }
                            Ok(false) => {
                                // No journal. If the filesystem says it needs
                                // recovering, nothing here can do it.
                                if ext2_state().feature_incompat & ext4::INCOMPAT_RECOVER != 0 {
                                    println!(
                                        "[vfs] needs recovery but has no journal; read-only"
                                    );
                                    ext2_state_mut().read_only = true;
                                }
                            }
                            Err(e) => {
                                println!("[vfs] journal unusable ({}); mounting read-only", e);
                                ext2_state_mut().read_only = true;
                            }
                        }
                    }
                }
                Err(()) => {
                    println!("[vfs] mount failed, falling back to FAT32.");
                }
            }
        }
    }

    // Create a dummy DiskState for FAT32 (needed even in ext2 mode for the service loop signature)
    let disk = if unsafe { FS_TYPE } == FsType::Fat32 {
        // Read BPB
        if part_lba > 0 {
            if DiskState::raw_read_sector(disk_tid, part_lba).is_err() {
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

        unsafe {
            FAT_CLUSTER_BYTES = bpb.bytes_per_sector as u32 * bpb.sectors_per_cluster as u32;
        }
        let d = DiskState { disk_tid, part_lba, bpb };
        warm_cache(&d);
        d
    } else {
        // Dummy — won't be used for ext2 path
        DiskState {
            disk_tid,
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

    if unsafe { FS_TYPE } == FsType::Ext2 {
        ext2_dir::note_dev_dir(ext2_state());
        if !ext2_state().read_only {
            recover_orphans();
        }
    }

    // Register with nameserver
    if nameserver::register(b"vfs").is_ok() {
        println!("[vfs] Registered with nameserver.");
    } else {
        println!("[vfs] Failed to register with nameserver.");
    }

    // Service loop
    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        let sender = msg.sender;

        match msg.tag {
            TAG_READ | TAG_WRITE | TAG_STAT | TAG_READDIR_BULK | TAG_TRUNCATE
                if devices::is_ours(sender, &msg) =>
            {
                devices::serve(sender, &msg)
            }
            // A link opened as itself answers STAT and nothing else.
            TAG_READ | TAG_WRITE | TAG_READDIR_BULK | TAG_TRUNCATE
                if get_handle(msg.data[0] as usize, sender).is_some_and(|f| f.link) =>
            {
                error_reply(sender, ERR_NOT_SUPPORTED)
            }
            TAG_OPEN if msg.data[1] & (OPEN_CREATE | OPEN_TRUNCATE) != 0 => {
                transacted(|| handle_open(&disk, sender, &msg))
            }
            TAG_OPEN => handle_open(&disk, sender, &msg),
            TAG_READ => handle_read(&disk, sender, &msg),
            TAG_CLOSE => handle_close(sender, &msg),
            TAG_STAT => handle_stat(sender, &msg),
            TAG_WRITE => transacted(|| handle_write(&disk, sender, &msg)),
            TAG_MKDIR => transacted(|| handle_mkdir(&disk, sender, &msg)),
            TAG_UNLINK | TAG_RMDIR | TAG_RENAME | TAG_LINK | TAG_SYMLINK => {
                transacted(|| handle_namespace(sender, &msg))
            }
            TAG_READLINK => handle_readlink(&disk, sender, &msg),
            TAG_CHDIR | TAG_FCHDIR => handle_chdir(&disk, sender, &msg),
            TAG_GETCWD => handle_getcwd(sender),
            TAG_GIVE_CWD => handle_give_cwd(sender, &msg),
            TAG_LOCK => handle_lock(sender, &msg),
            TAG_MAP if unsafe { FS_TYPE } == FsType::Ext2 => pager::handle_map(sender, &msg),
            TAG_MAP => error_reply(sender, ERR_NOT_SUPPORTED),
            // From the kernel alone: nobody else can set the pager bit.
            quark_rt::ipc::TAG_PAGE_IN if sender & quark_rt::ipc::PAGER_BIT != 0 => {
                pager::page_in(sender, &msg)
            }
            quark_rt::ipc::TAG_OBJECT_IDLE if sender == 0 => pager::idle(msg.data[1]),
            // A task waiting for a lock has gone; nobody is left to answer.
            quark_rt::ipc::TAG_TASK_DIED if sender == 0 => locks::drop_task(msg.data[0] as usize),
            TAG_TRUNCATE => transacted(|| handle_truncate(sender, &msg)),
            TAG_STATFS => handle_statfs(sender),
            // From the kernel, which is not waiting for an answer.
            quark_rt::ipc::TAG_SPACE_DIED if sender == 0 => client_died(msg.data[0]),
            TAG_READDIR_BULK => handle_readdir_bulk(&disk, sender, &msg),
            quark_rt::ipc::TAG_PING => {
                // Liveness probe: reply immediately, touching no disk state.
                let reply = Message {
                    sender: 0,
                    tag: quark_rt::ipc::TAG_PING,
                    data: [0; 6],
                };
                let _ = syscall::sys_reply(sender, &reply);
            }
            _ => error_reply(sender, 0xFF),
        }
    }
}

// ---------------------------------------------------------------------------
// Request handlers
// ---------------------------------------------------------------------------

/// TAG_OPEN: data[0] = path length, data[1] = flags; the path is lent.
/// Reply: [handle, size, is_dir, mode with its type bits, access, inode id].
fn handle_open(disk: &DiskState, sender: usize, msg: &Message) {
    let flags = msg.data[1];
    let path = match protocol::lent_path(sender, 0, msg.data[0] as usize, 0) {
        Ok(p) => p,
        Err(code) => return error_reply(sender, code),
    };
    if unsafe { FS_TYPE } == FsType::Ext2 {
        let base = match base_of(sender, msg.data[5]) {
            Ok(b) => b,
            Err(code) => return error_reply(sender, code),
        };
        // The lookup itself finds /dev, through links and all, when the root
        // has one. Without it, the path is matched as written.
        if ext2_dir::dev_dir() == 0 {
            let whole = match ext2_whole_path(base, path) {
                Ok(p) => p,
                Err(code) => return error_reply(sender, code),
            };
            match devices::lookup(whole) {
                devices::Lookup::Elsewhere => {}
                found => return devices::open(sender, whole, found, flags),
            }
        }
        open_ext2(sender, base, path, flags);
    } else {
        let path = match fat_path(sender, msg.data[5], path) {
            Ok(p) => p,
            Err(code) => return error_reply(sender, code),
        };
        match devices::lookup(path) {
            devices::Lookup::Elsewhere => {}
            found => return devices::open(sender, path, found, flags),
        }
        open_fat32(disk, sender, path, flags);
    }
}

/// The ext2 directory a relative path starts from. `word` is 0 for the
/// program's working directory, or one more than an open directory handle.
fn base_of(sender: usize, word: u64) -> Result<u32, u64> {
    if word == 0 {
        return Ok(match cwd::get(space_of(sender)).0 {
            cwd::Where::Inode(ino) => ino,
            _ => ext2::EXT2_ROOT_INO,
        });
    }
    let file = get_handle((word - 1) as usize, sender).ok_or(ERR_INVALID_HANDLE)?;
    match file.fs {
        FsFileData::Ext2 { inode_num } if file.is_dir => Ok(inode_num),
        FsFileData::DevDir if ext2_dir::dev_dir() != 0 => Ok(ext2_dir::dev_dir()),
        _ => Err(ERR_NOT_DIR),
    }
}

/// `path` from `base` written out whole, for a root with no /dev to find.
fn ext2_whole_path(base: u32, path: &'static [u8]) -> Result<&'static [u8], u64> {
    if path.first() == Some(&b'/') {
        return Ok(path);
    }
    let dir = ext2_dir::path_of(ext2_state(), base)?;
    // path_of's buffer is overwritten by nothing join does.
    cwd::join(dir, path)
}

/// A FAT32 path made absolute: FAT32 keeps a program's directory as a path,
/// and has no handles to start from.
fn fat_path(sender: usize, word: u64, path: &[u8]) -> Result<&'static [u8], u64> {
    if word != 0 && path.first() != Some(&b'/') {
        return Err(ERR_NOT_SUPPORTED);
    }
    let (_, dir) = cwd::get(space_of(sender));
    cwd::join(dir, path)
}

fn reply_opened(sender: usize, words: [u64; 6]) {
    let reply = Message { sender: 0, tag: TAG_OK, data: words };
    let _ = syscall::sys_reply(sender, &reply);
}

fn open_ext2(sender: usize, base: u32, path: &[u8], flags: u64) {
    let (uid, gid) = get_sender_uid_gid(sender);
    let trailing = path.len() > 1 && path[path.len() - 1] == b'/';
    let wants_dir = flags & OPEN_DIRECTORY != 0 || trailing;
    let follow = flags & OPEN_NOFOLLOW == 0;
    let found = ext2_dir::resolve(ext2_state(), base, path, uid, gid, follow);
    let found = match found {
        Ok(ext2_dir::Found::Device(dev)) => {
            return devices::open(sender, path, devices::Lookup::Device(dev), flags);
        }
        Ok(ext2_dir::Found::Inode(ino, _, _)) if ino == ext2_dir::dev_dir() => {
            return devices::open(sender, path, devices::Lookup::Dir, flags);
        }
        Ok(ext2_dir::Found::Inode(ino, inode, holder)) => Ok((ino, inode, holder)),
        Err(code) => Err(code),
    };
    let (ino, inode) = match found {
        Ok((ino, inode, _)) => {
            if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 {
                return error_reply(sender, ERR_EXISTS);
            }
            (ino, inode)
        }
        Err(ERR_NOT_FOUND) if flags & OPEN_CREATE != 0 => {
            if trailing {
                return error_reply(sender, ERR_IS_DIR);
            }
            if ext2_state().read_only {
                return error_reply(sender, ERR_READ_ONLY);
            }
            match ext2_ops::create(ext2_state_mut(), base, path, uid, gid, false) {
                Ok(made) => made,
                Err(code) => return error_reply(sender, code),
            }
        }
        Err(code) => return error_reply(sender, code),
    };
    if wants_dir && !inode.is_dir() {
        return error_reply(sender, ERR_NOT_DIR);
    }
    // Only OPEN_NOFOLLOW gets this far with a link.
    let link = inode.is_symlink();
    if !link && !ext2::check_permission(&inode, uid, gid, 4) {
        return error_reply(sender, ERR_PERMISSION);
    }
    let writable =
        !link && !ext2_state().read_only && ext2::check_permission(&inode, uid, gid, 2);
    let mut size = inode.size64();
    if flags & OPEN_TRUNCATE != 0 && inode.is_regular() {
        if !writable {
            return error_reply(sender, ERR_PERMISSION);
        }
        if let Err(code) = ext2_ops::truncate(ext2_state_mut(), ino, 0) {
            return error_reply(sender, code);
        }
        pager::resized(ino, 0);
        size = 0;
    }
    let file = OpenFile {
        in_use: true,
        owner: space_of(sender),
        file_size: 0,
        is_dir: inode.is_dir(),
        writable,
        link,
        read_offset: 0,
        fs: FsFileData::Ext2 { inode_num: ino },
    };
    match handles::alloc(file) {
        Some(handle) => reply_opened(sender, [
            handle as u64,
            size,
            inode.is_dir() as u64,
            inode.i_mode as u64,
            access_bits(&inode, uid, gid),
            ino as u64,
        ]),
        None => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}

/// Whether `name` is a FAT short name: at most eight characters, a dot and
/// three more. Anything longer would be squeezed into one by `to_fat83` and
/// name a different file.
fn fits_fat83(name: &[u8]) -> bool {
    let (base, ext) = match name.iter().position(|&b| b == b'.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, &[][..]),
    };
    !base.is_empty() && base.len() <= 8 && ext.len() <= 3 && !ext.contains(&b'.')
}

/// Split a FAT32 path into its parent directory's cluster and the new name.
fn fat32_parent(disk: &DiskState, path: &[u8]) -> Result<(u32, [u8; 11]), u64> {
    let (parent, name) = ext2_ops::split_path(path)?;
    if !fits_fat83(name) {
        return Err(ERR_NAME_TOO_LONG);
    }
    let (cluster, _, is_dir, _, _) = resolve_path(disk, parent)?;
    if !is_dir {
        return Err(ERR_NOT_DIR);
    }
    let mut fat_name = [0u8; 11];
    to_fat83(name, &mut fat_name);
    Ok((cluster, fat_name))
}

fn open_fat32(disk: &DiskState, sender: usize, path: &[u8], flags: u64) {
    let trailing = path.len() > 1 && path[path.len() - 1] == b'/';
    let wants_dir = flags & OPEN_DIRECTORY != 0 || trailing;
    if flags & OPEN_TRUNCATE != 0 {
        return error_reply(sender, ERR_NOT_SUPPORTED);
    }
    let found = match resolve_path(disk, path) {
        Ok(found) => {
            if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 {
                return error_reply(sender, ERR_EXISTS);
            }
            found
        }
        Err(ERR_NOT_FOUND) if flags & OPEN_CREATE != 0 && !trailing => {
            let (parent, fat_name) = match fat32_parent(disk, path) {
                Ok(p) => p,
                Err(code) => return error_reply(sender, code),
            };
            match create_dir_entry(disk, parent, &fat_name, false) {
                Ok(cluster) => (cluster, 0, false, parent, fat_name),
                Err(code) => return error_reply(sender, code),
            }
        }
        Err(code) => return error_reply(sender, code),
    };
    let (cluster, size, is_dir, dir_cluster, fat_name) = found;
    if wants_dir && !is_dir {
        return error_reply(sender, ERR_NOT_DIR);
    }
    match alloc_handle_fat32(sender, cluster, size, is_dir, dir_cluster, &fat_name) {
        Some(handle) => {
            let mode = if is_dir { FAT_DIR_MODE } else { FAT_FILE_MODE };
            reply_opened(sender, [
                handle as u64, size as u64, is_dir as u64, mode, FAT_ACCESS, cluster as u64,
            ]);
        }
        None => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}

/// TAG_MKDIR: data[0] = path length; the path is lent.
fn handle_mkdir(disk: &DiskState, sender: usize, msg: &Message) {
    let path = match protocol::lent_path(sender, 0, msg.data[0] as usize, 0) {
        Ok(p) => p,
        Err(code) => return error_reply(sender, code),
    };
    let made = if unsafe { FS_TYPE } == FsType::Ext2 {
        if ext2_state().read_only {
            Err(ERR_READ_ONLY)
        } else if ext2_dir::dev_dir() == 0 && devices::refuses(path) {
            Err(ERR_PERMISSION)
        } else {
            let (uid, gid) = get_sender_uid_gid(sender);
            base_of(sender, msg.data[5]).and_then(|base| {
                ext2_ops::create(ext2_state_mut(), base, path, uid, gid, true).map(|_| ())
            })
        }
    } else {
        match fat_path(sender, msg.data[5], path) {
            Ok(path) if devices::refuses(path) => Err(ERR_PERMISSION),
            Ok(path) => fat32_mkdir(disk, path),
            Err(code) => Err(code),
        }
    };
    match made {
        Ok(()) => reply_opened(sender, [0; 6]),
        Err(code) => error_reply(sender, code),
    }
}

fn fat32_mkdir(disk: &DiskState, path: &[u8]) -> Result<(), u64> {
    {
        match resolve_path(disk, path) {
            Ok(_) => Err(ERR_EXISTS),
            Err(ERR_NOT_FOUND) => fat32_parent(disk, path)
                .and_then(|(parent, name)| create_dir_entry(disk, parent, &name, true))
                .map(|_| ()),
            Err(code) => Err(code),
        }
    }
}
/// Copy the first `n` bytes of `CLIENT_BUF` into what `sender` lent.
fn lend_out(sender: usize, n: usize) -> bool {
    let data = unsafe { core::slice::from_raw_parts(CLIENT_BUF as *const u8, n) };
    n == 0 || syscall::sys_lent_write(sender, 0, data) == Ok(n)
}

/// Copy `n` bytes of what `sender` lent into `CLIENT_BUF`.
fn lend_in(sender: usize, n: usize) -> bool {
    let buf = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, n) };
    n == 0 || syscall::sys_lent_read(sender, 0, buf) == Ok(n)
}

/// Reply to a read: the bytes go into what the caller lent, and the count into
/// the reply. A caller that lent too little gets an error, not a short read.
fn reply_read(sender: usize, result: Result<u32, u64>) {
    match result {
        Ok(n) if lend_out(sender, n as usize) => {
            let reply = Message { sender: 0, tag: TAG_OK, data: [n as u64, 0, 0, 0, 0, 0] };
            let _ = syscall::sys_reply(sender, &reply);
        }
        Ok(_) => error_reply(sender, ERR_IO),
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_READ: data[0]=handle, data[2]=offset, data[3]=max_bytes (at most a
/// page), with a buffer that long lent for writing.
/// Reply: tag=TAG_OK, data[0]=bytes_read  OR  tag=TAG_ERROR, data[0]=error_code
fn handle_read(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_read_ext2(sender, msg);
        return;
    }

    let handle = msg.data[0] as usize;
    let offset = msg.data[2] as u32;
    let max_bytes = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => reply_read(sender, read_file_data(disk, file, offset, max_bytes)),
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// TAG_CLOSE: data[0]=handle
/// Reply: tag=TAG_OK  OR  tag=TAG_ERROR
fn handle_close(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let space = space_of(sender);
    // Closing any handle on a file drops every lock the program holds on it,
    // as POSIX has it; the handle's own locks go with the handle.
    let key = get_handle(handle, sender).and_then(|f| handles::lock_key(f));
    match handles::close(handle, space) {
        Some(ino) => {
            reply_opened(sender, [0; 6]);
            // Another thread may be waiting for a lock through this handle.
            while let Some(w) = locks::drop_handle(handle) {
                error_reply(w.sender, ERR_INVALID_HANDLE);
            }
            if let Some(key) = key {
                locks::release(locks::Owner::Program(space), Some(key));
            }
            grant_waiters();
            settle(&[ino]);
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// TAG_LOCK: `[handle, kind, start, len, flags]`. `kind` is 0 to unlock, 1
/// shared, 2 exclusive; `len` 0 runs to the end of the file and beyond. With
/// `LOCK_QUERY` the reply is `[kind, start, len, holder]` of the first lock in
/// the way (`kind` 0 if none, `holder` the program, or all ones for a
/// handle's). With `LOCK_WAIT` a lock that cannot be granted yet is answered
/// when it can be.
fn handle_lock(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let [_, kind, start, len, flags, _] = msg.data;
    let space = space_of(sender);
    let Some(file) = get_handle(handle, sender) else {
        return error_reply(sender, ERR_INVALID_HANDLE);
    };
    let Some(inode) = handles::lock_key(file) else {
        return error_reply(sender, ERR_NOT_SUPPORTED);
    };
    if kind > 2 || flags & !(LOCK_WAIT | LOCK_OFD | LOCK_QUERY) != 0 {
        return error_reply(sender, ERR_INVALID_PATH);
    }
    let end = if len == 0 {
        u64::MAX
    } else {
        match start.checked_add(len) {
            Some(end) => end,
            None => return error_reply(sender, ERR_INVALID_PATH),
        }
    };
    let owner = if flags & LOCK_OFD != 0 {
        locks::Owner::Handle(handle)
    } else {
        locks::Owner::Program(space)
    };
    let want = locks::Range { inode, owner, start, end, exclusive: kind == 2 };

    if flags & LOCK_QUERY != 0 {
        if kind == 0 {
            return error_reply(sender, ERR_INVALID_PATH);
        }
        let answer = match locks::conflict(&want) {
            Some(held) => [
                if held.exclusive { 2 } else { 1 },
                held.start,
                if held.end == u64::MAX { 0 } else { held.end - held.start },
                match held.owner {
                    locks::Owner::Program(s) => s,
                    locks::Owner::Handle(_) => u64::MAX,
                },
                0,
                0,
            ],
            None => [0; 6],
        };
        return reply_opened(sender, answer);
    }

    if kind == 0 {
        match locks::apply(&want, true) {
            Ok(()) => reply_opened(sender, [0; 6]),
            Err(code) => error_reply(sender, code),
        }
        return grant_waiters();
    }
    match locks::conflict(&want) {
        None => match locks::apply(&want, false) {
            // An exclusive lock made shared may let a waiter in.
            Ok(()) => {
                reply_opened(sender, [0; 6]);
                grant_waiters();
            }
            Err(code) => error_reply(sender, code),
        },
        Some(_) if flags & LOCK_WAIT != 0 => {
            match locks::wait(locks::Waiter { sender, space, want }) {
                // No answer until it is granted. If the task waiting goes
                // first, the server is told, and forgets the request.
                Ok(()) => {
                    let _ = syscall::sys_task_watch(sender);
                }
                Err(code) => error_reply(sender, code),
            }
        }
        Some(_) => error_reply(sender, ERR_WOULD_BLOCK),
    }
}

/// Answer every waiting lock request that can now be granted.
fn grant_waiters() {
    while let Some(w) = locks::grantable() {
        match locks::apply(&w.want, false) {
            Ok(()) => reply_opened(w.sender, [0; 6]),
            Err(code) => error_reply(w.sender, code),
        }
    }
}

/// A program has gone: its handles go with it.
fn client_died(space: u64) {
    let mut closed = [0u32; handles::MAX_OPEN_FILES];
    let n = handles::close_all(space, &mut closed);
    locks::drop_space(space);
    grant_waiters();
    settle(&closed[..n]);
    if let cwd::Where::Inode(ino) = cwd::forget(space) {
        settle(&[ino]);
    }
}

/// Free what a machine stopped while files were removed but in use left on
/// the orphan list. Before the first request: nothing can hold them now.
fn recover_orphans() {
    let mut freed = 0usize;
    let mut failed = false;
    // Bounded by the list's own bound; each pop is its own transaction.
    for _ in 0..ext2_state().total_inodes {
        let mut more = false;
        transacted(|| match ext2_ops::recover_orphan(ext2_state_mut()) {
            Ok(m) => more = m,
            Err(code) => {
                println!("[vfs] could not free an orphaned inode ({})", code);
                failed = true;
            }
        });
        if failed || !more {
            break;
        }
        freed += 1;
    }
    if freed > 0 {
        println!(
            "[vfs] freed {} orphaned inode{}",
            freed,
            if freed == 1 { "" } else { "s" }
        );
    }
}

/// Free any of these inodes that lost their last name while open and have
/// now lost their last handle.
fn settle(inodes: &[u32]) {
    for &ino in inodes {
        if !handles::is_orphan(ino) || handles::inode_is_open(ino) {
            continue;
        }
        handles::forget_orphan(ino);
        transacted(|| {
            if let Err(code) = ext2_ops::release(ext2_state_mut(), ino) {
                println!("[vfs] could not free inode {} ({})", ino, code);
            }
        });
    }
}

/// TAG_UNLINK and TAG_RMDIR lend a path, `data[0]` long. TAG_RENAME and
/// TAG_LINK lend two, end to end, `data[0]` and `data[1]` long (LINK's
/// `data[2]` may ask to follow a link at the source); TAG_SYMLINK lends the
/// target, then the new path.
fn handle_namespace(sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } != FsType::Ext2 {
        return error_reply(sender, ERR_NOT_SUPPORTED);
    }
    if ext2_state().read_only {
        return error_reply(sender, ERR_READ_ONLY);
    }
    let first = match protocol::lent_path(sender, 0, msg.data[0] as usize, 0) {
        Ok(p) => p,
        Err(code) => return error_reply(sender, code),
    };
    // With a /dev on the disk, the lookup keeps everything out of it. A link's
    // target is only text, and may name a device.
    let lexical = ext2_dir::dev_dir() == 0;
    if lexical && msg.tag != TAG_SYMLINK && devices::refuses(first) {
        return error_reply(sender, ERR_PERMISSION);
    }
    let base = match base_of(sender, msg.data[5]) {
        Ok(b) => b,
        Err(code) => return error_reply(sender, code),
    };
    let (uid, gid) = get_sender_uid_gid(sender);
    let e2 = ext2_state_mut();
    let done = match msg.tag {
        TAG_UNLINK => ext2_ops::unlink(e2, base, first, uid, gid),
        TAG_RMDIR => ext2_ops::rmdir(e2, base, first, uid, gid),
        tag => match protocol::lent_path(sender, msg.data[0] as usize, msg.data[1] as usize, 4096) {
            Ok(second) if lexical && devices::refuses(second) => Err(ERR_PERMISSION),
            Ok(second) => match tag {
                // The second path's own base; SYMLINK resolves only that path,
                // and takes the ordinary one for it.
                TAG_SYMLINK => ext2_ops::symlink(e2, first, base, second, uid, gid),
                _ => match base_of(sender, msg.data[4]) {
                    Ok(to_base) if tag == TAG_LINK => {
                        let follow = msg.data[2] & LINK_FOLLOW != 0;
                        ext2_ops::link(e2, base, first, to_base, second, uid, gid, follow)
                    }
                    Ok(to_base) => ext2_ops::rename(e2, base, first, to_base, second, uid, gid),
                    Err(code) => Err(code),
                },
            },
            Err(code) => Err(code),
        },
    };
    match done {
        Ok(()) => reply_opened(sender, [0; 6]),
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_READLINK: `data[0]` = the path's length, `data[1]` = the room after
/// it. One buffer is lent for reading and writing: the path, then that room,
/// where the target is written. The reply is the target's whole length, which
/// may be more than there was room for.
fn handle_readlink(disk: &DiskState, sender: usize, msg: &Message) {
    let path_len = msg.data[0] as usize;
    let path = match protocol::lent_path(sender, 0, path_len, 0) {
        Ok(p) => p,
        Err(code) => return error_reply(sender, code),
    };
    if unsafe { FS_TYPE } != FsType::Ext2 {
        // No links on FAT32: whatever is there is not one.
        return match fat_path(sender, msg.data[5], path).and_then(|p| resolve_path(disk, p)) {
            Ok(_) => error_reply(sender, ERR_INVALID_PATH),
            Err(code) => error_reply(sender, code),
        };
    }
    let base = match base_of(sender, msg.data[5]) {
        Ok(b) => b,
        Err(code) => return error_reply(sender, code),
    };
    let (uid, gid) = get_sender_uid_gid(sender);
    let e2 = ext2_state();
    let inode = match ext2_dir::resolve(e2, base, path, uid, gid, false) {
        Ok(ext2_dir::Found::Inode(_, inode, _)) => inode,
        Ok(ext2_dir::Found::Device(_)) => return error_reply(sender, ERR_INVALID_PATH),
        Err(code) => return error_reply(sender, code),
    };
    let len = match ext2_ops::read_link(e2, &inode) {
        Ok(len) => len,
        Err(code) => return error_reply(sender, code),
    };
    let target = ext2_ops::link_target(len);
    // What does not fit is not written; the length says there was more.
    let room = (msg.data[1] as usize).min(len);
    if room > 0 && syscall::sys_lent_write(sender, path_len, &target[..room]) != Ok(room) {
        return error_reply(sender, ERR_IO);
    }
    reply_opened(sender, [len as u64, 0, 0, 0, 0, 0]);
}

/// TAG_CHDIR: `data[0]` = the path's length, lent, from `data[5]`'s base.
/// TAG_FCHDIR: `data[0]` = an open directory handle. Either way the program
/// is now in that directory, which it must be able to search.
fn handle_chdir(disk: &DiskState, sender: usize, msg: &Message) {
    let space = space_of(sender);
    let (uid, gid) = get_sender_uid_gid(sender);
    let to = if unsafe { FS_TYPE } == FsType::Ext2 {
        let found = if msg.tag == TAG_FCHDIR {
            base_of(sender, msg.data[0] + 1)
        } else {
            protocol::lent_path(sender, 0, msg.data[0] as usize, 0).and_then(|path| {
                let base = base_of(sender, msg.data[5])?;
                ext2_dir::resolve_inode(ext2_state(), base, path, uid, gid, true).map(|f| f.0)
            })
        };
        found.and_then(|ino| {
            let dir = ext2::read_inode(ext2_state(), ino)?;
            if !dir.is_dir() {
                Err(ERR_NOT_DIR)
            } else if !ext2::check_permission(&dir, uid, gid, 1) {
                Err(ERR_PERMISSION)
            } else if ino == ext2::EXT2_ROOT_INO {
                cwd::set(space, cwd::Where::Root, b"")
            } else {
                cwd::set(space, cwd::Where::Inode(ino), b"")
            }
        })
    } else if msg.tag == TAG_FCHDIR {
        Err(ERR_NOT_SUPPORTED)
    } else {
        protocol::lent_path(sender, 0, msg.data[0] as usize, 0)
            .and_then(|path| fat_path(sender, msg.data[5], path))
            .and_then(|path| match resolve_path(disk, path)? {
                (_, _, true, _, _) if path == b"/" => cwd::set(space, cwd::Where::Root, b""),
                (_, _, true, _, _) => cwd::set(space, cwd::Where::Path(path.len()), path),
                _ => Err(ERR_NOT_DIR),
            })
    };
    match to {
        Ok(left) => {
            reply_opened(sender, [0; 6]);
            // The directory left may have been removed while the program was
            // in it, and now nothing holds it.
            if let cwd::Where::Inode(ino) = left {
                settle(&[ino]);
            }
        }
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_GETCWD: 4096 bytes lent for writing. Reply: the path's length.
fn handle_getcwd(sender: usize) {
    let path = match cwd::get(space_of(sender)) {
        (cwd::Where::Inode(ino), _) => ext2_dir::path_of(ext2_state(), ino),
        (_, path) => Ok(path),
    };
    match path {
        Ok(p) if syscall::sys_lent_write(sender, 0, p) == Ok(p.len()) => {
            reply_opened(sender, [p.len() as u64, 0, 0, 0, 0, 0])
        }
        Ok(_) => error_reply(sender, ERR_IO),
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_GIVE_CWD: `data[0]` = a task the caller's program made for another
/// program, not yet necessarily running. That program starts in the
/// caller's directory.
fn handle_give_cwd(sender: usize, msg: &Message) {
    let me = space_of(sender);
    let child = msg.data[0] as usize;
    let theirs = space_of(child);
    let parent = syscall::sys_task_info(child).map(|(_, parent, _)| parent);
    let allowed = me != 0
        && theirs != 0
        && theirs != me
        && parent.is_ok_and(|p| p != 0 && space_of(p) == me);
    if !allowed {
        return error_reply(sender, ERR_PERMISSION);
    }
    let (at, path) = cwd::get(me);
    match cwd::set(theirs, at, path) {
        Ok(left) => {
            reply_opened(sender, [0; 6]);
            if let cwd::Where::Inode(ino) = left {
                settle(&[ino]);
            }
        }
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_TRUNCATE: data[0] = handle, data[1] = the new size.
fn handle_truncate(sender: usize, msg: &Message) {
    let Some(file) = get_handle(msg.data[0] as usize, sender) else {
        return error_reply(sender, ERR_INVALID_HANDLE);
    };
    if unsafe { FS_TYPE } != FsType::Ext2 {
        return error_reply(sender, ERR_NOT_SUPPORTED);
    }
    if !file.writable {
        return error_reply(sender, ERR_PERMISSION);
    }
    let ino = file.inode_num();
    match ext2_ops::truncate(ext2_state_mut(), ino, msg.data[1]) {
        Ok(()) => {
            pager::resized(ino, msg.data[1]);
            reply_opened(sender, [0; 6])
        }
        Err(code) => error_reply(sender, code),
    }
}

/// TAG_STAT: data[0]=handle, with an 88-byte buffer lent for writing.
/// Reply: tag=TAG_OK, data[0]=88, the record in the buffer.
fn handle_stat(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let Some(file) = get_handle(handle, sender) else {
        return error_reply(sender, ERR_INVALID_HANDLE);
    };
    let record = match &file.fs {
        FsFileData::Fat32 { first_cluster, .. } => {
            let cluster = unsafe { FAT_CLUSTER_BYTES } as u64;
            StatRecord {
                id: *first_cluster as u64,
                size: file.file_size as u64,
                mode: if file.is_dir { FAT_DIR_MODE } else { FAT_FILE_MODE },
                links: 1,
                uid: 0,
                gid: 0,
                atime: 0,
                mtime: 0,
                ctime: 0,
                blocks: (file.file_size as u64 + 511) / 512,
                block_size: cluster,
            }
        }
        FsFileData::Ext2 { inode_num } => {
            let e2 = ext2_state();
            let inode = match ext2::read_inode(e2, *inode_num) {
                Ok(i) => i,
                Err(code) => return error_reply(sender, code),
            };
            StatRecord {
                id: *inode_num as u64,
                size: inode.size64(),
                mode: inode.i_mode as u64,
                links: inode.i_links_count as u64,
                uid: inode.i_uid as u64,
                gid: inode.i_gid as u64,
                atime: inode.i_atime as u64,
                mtime: inode.i_mtime as u64,
                ctime: inode.i_ctime as u64,
                blocks: inode.i_blocks as u64,
                block_size: e2.block_size as u64,
            }
        }
        FsFileData::Device(_) | FsFileData::DevDir | FsFileData::None => {
            return error_reply(sender, ERR_INVALID_HANDLE)
        }
    };
    match syscall::sys_lent_write(sender, 0, &record.to_bytes()) {
        Ok(n) if n == STAT_LEN => reply_opened(sender, [STAT_LEN as u64, 0, 0, 0, 0, 0]),
        _ => error_reply(sender, ERR_IO),
    }
}
/// TAG_WRITE: data[0]=handle, data[2]=offset, data[3]=len (at most a page),
/// with a buffer that long lent for reading.
/// Reply: tag=TAG_OK, data[0]=bytes_written  OR  tag=TAG_ERROR, data[0]=error_code
fn handle_write(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_write_ext2(sender, msg);
        return;
    }

    let handle = msg.data[0] as usize;
    let offset = msg.data[2] as u32;
    let len = (msg.data[3] as u32).min(PAGE_SIZE as u32);
    if !lend_in(sender, len as usize) {
        error_reply(sender, ERR_IO);
        return;
    }

    match get_handle(handle, sender) {
        Some(file) => {
            let (dir_cluster, fat_name) = match &file.fs {
                FsFileData::Fat32 { dir_cluster, fat_name, .. } => (*dir_cluster, *fat_name),
                _ => { error_reply(sender, ERR_IO); return; }
            };
            match write_file_data(disk, file, offset, len) {
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


/// TAG_READDIR_BULK: data[0] = handle, data[1] = the index of the first entry
/// wanted, data[2] = the length of the buffer lent for writing. Fills it with
/// directory records (see `protocol::put_dirent`) and replies
/// `[bytes, next index, end]`.
fn handle_readdir_bulk(disk: &DiskState, sender: usize, msg: &Message) {
    if unsafe { FS_TYPE } == FsType::Ext2 {
        handle_readdir_bulk_ext2(sender, msg);
        return;
    }

    let start = msg.data[1];
    let room = (msg.data[2] as usize).min(PAGE_SIZE);
    let dir_cluster = match get_handle(msg.data[0] as usize, sender) {
        Some(file) if file.is_dir => match &file.fs {
            FsFileData::Fat32 { first_cluster, .. } => *first_cluster,
            _ => return error_reply(sender, ERR_IO),
        },
        Some(_) => return error_reply(sender, ERR_NOT_DIR),
        None => return error_reply(sender, ERR_INVALID_HANDLE),
    };

    let buf = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, PAGE_SIZE) };
    let spc = disk.bpb.sectors_per_cluster;
    let mut cluster = dir_cluster;
    let mut index = 0u64;
    let mut used = 0usize;
    let mut next = start;
    let mut end = true;

    'outer: loop {
        let start_lba = disk.cluster_start_lba(cluster);
        disk.prefetch_sectors(start_lba, spc);
        for sector in 0..spc {
            let sec_data = match disk.cached_read_sector(start_lba + sector) {
                Ok(d) => d,
                // What was read so far is still good; the next request
                // starts at the sector that failed and reports it.
                Err(_) if used > 0 => {
                    end = false;
                    break 'outer;
                }
                Err(_) => return error_reply(sender, ERR_IO),
            };
            let mut sec_buf = [0u8; 512];
            sec_buf.copy_from_slice(sec_data);

            for e in 0..16 {
                let off = e * 32;
                let first_byte = sec_buf[off];
                if first_byte == 0x00 {
                    break 'outer;
                }
                let attr = sec_buf[off + 11];
                // Deleted, long-name pieces and the volume label are not entries.
                if first_byte == 0xE5 || attr & 0x0F == 0x0F || attr & 0x08 != 0 {
                    continue;
                }
                if index < start {
                    index += 1;
                    continue;
                }
                let mut name = [0u8; 12];
                let name_len = fat_display_name(&sec_buf[off..off + 11], &mut name);
                let hi = read_u16(&sec_buf, off + 20) as u64;
                let lo = read_u16(&sec_buf, off + 26) as u64;
                let size = read_u32(&sec_buf, off + 28) as u64;
                let kind = if attr & 0x10 != 0 { DT_DIR } else { DT_REG };
                match put_dirent(&mut buf[..room], used, (hi << 16) | lo, index + 1, size, kind, &name[..name_len]) {
                    Some(len) => {
                        used += len;
                        next = index + 1;
                        index += 1;
                    }
                    None => {
                        end = false;
                        break 'outer;
                    }
                }
            }
        }
        match disk.fat_next(cluster) {
            Some(n) => cluster = n,
            None => break,
        }
    }

    reply_dirents(sender, used, next, end);
}

/// A FAT short name as a name: `HELLO   ELF` is `HELLO.ELF`, `USR        `
/// is `USR`.
fn fat_display_name(raw: &[u8], out: &mut [u8; 12]) -> usize {
    let base = raw[..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    let ext = raw[8..11].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
    out[..base].copy_from_slice(&raw[..base]);
    if ext == 0 {
        return base;
    }
    out[base] = b'.';
    out[base + 1..base + 1 + ext].copy_from_slice(&raw[8..8 + ext]);
    base + 1 + ext
}

/// Reply to a bulk readdir: `used` bytes of records from `CLIENT_BUF` into
/// what the caller lent, and where to carry on.
fn reply_dirents(sender: usize, used: usize, next: u64, end: bool) {
    if !lend_out(sender, used) {
        return error_reply(sender, ERR_IO);
    }
    reply_opened(sender, [used as u64, next, end as u64, 0, 0, 0]);
}

/// TAG_STATFS, with 64 bytes lent for writing.
fn handle_statfs(sender: usize) {
    let words: [u64; 8] = if unsafe { FS_TYPE } == FsType::Ext2 {
        let e2 = ext2_state();
        let free = e2.free_blocks_count as u64;
        [
            0xEF53,
            e2.block_size as u64,
            e2.total_blocks as u64,
            free,
            free.saturating_sub(e2.reserved_blocks as u64),
            e2.total_inodes as u64,
            e2.free_inodes_count as u64,
            MAX_NAME as u64,
        ]
    } else {
        // FAT keeps its free count in a hint nothing here reads; say what is
        // known and leave the counts empty.
        [0x4d44, unsafe { FAT_CLUSTER_BYTES } as u64, 0, 0, 0, 0, 0, 12]
    };
    let mut record = [0u8; STATFS_LEN];
    for (i, w) in words.iter().enumerate() {
        record[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    match syscall::sys_lent_write(sender, 0, &record) {
        Ok(n) if n == STATFS_LEN => reply_opened(sender, [STATFS_LEN as u64, 0, 0, 0, 0, 0]),
        _ => error_reply(sender, ERR_IO),
    }
}
// ---------------------------------------------------------------------------
// ext2 IPC handlers
// ---------------------------------------------------------------------------

fn get_sender_uid_gid(sender: usize) -> (u32, u32) {
    syscall::sys_get_tuid(sender).unwrap_or((0, 0))
}


fn handle_read_ext2(sender: usize, msg: &Message) {
    let handle = msg.data[0] as usize;
    let offset = msg.data[2] as u32;
    let max_bytes = msg.data[3] as u32;

    match get_handle(handle, sender) {
        Some(file) => {
            let inode = match ext2::read_inode(ext2_state(), file.inode_num()) {
                Ok(inode) => inode,
                Err(code) => return error_reply(sender, code),
            };
            reply_read(sender, ext2::read_file_data(ext2_state(), &inode, offset, max_bytes));
        }
        None => error_reply(sender, ERR_INVALID_HANDLE),
    }
}

/// Run one filesystem-modifying operation as a single transaction.
///
/// Everything it writes is held in the journal until it is complete, so a
/// machine that stops half way through leaves the filesystem as it was rather
/// than as neither one thing nor the other.
fn transacted<F: FnOnce()>(body: F) {
    journal::begin(journal_mut());
    body();
    match journal::commit(journal_mut(), ext2_state()) {
        Ok(_) => {
            if let Err(e) = journal::checkpoint(journal_mut(), ext2_state()) {
                println!("[vfs] checkpoint failed ({}); the journal will replay it", e);
            }
        }
        Err(e) => {
            println!("[vfs] commit failed ({}); the change was abandoned", e);
            journal::abort(journal_mut());
        }
    }
}

fn handle_write_ext2(sender: usize, msg: &Message) {
    if ext2_state().read_only {
        error_reply(sender, ERR_READ_ONLY);
        return;
    }
    let handle = msg.data[0] as usize;
    let offset = msg.data[2] as u32;
    let len = (msg.data[3] as u32).min(PAGE_SIZE as u32);

    match get_handle(handle, sender) {
        Some(file) => {
            if !file.writable {
                error_reply(sender, ERR_PERMISSION);
                return;
            }
            let inode_num = file.inode_num();
            let mut inode = match ext2::read_inode(ext2_state(), inode_num) {
                Ok(inode) => inode,
                Err(code) => return error_reply(sender, code),
            };
            if !lend_in(sender, len as usize) {
                error_reply(sender, ERR_IO);
                return;
            }
            let e2 = ext2_state_mut();
            match ext2::write_file_data(e2, &mut inode, inode_num, offset, len) {
                Ok(bytes_written) => {
                    // A mapping of the file sees what was written.
                    pager::wrote(inode_num, offset as u64, bytes_written as usize);
                    pager::resized(inode_num, inode.size64());
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


fn handle_readdir_bulk_ext2(sender: usize, msg: &Message) {
    let start = msg.data[1];
    let room = (msg.data[2] as usize).min(PAGE_SIZE);
    let e2 = ext2_state();
    let dir = match get_handle(msg.data[0] as usize, sender) {
        Some(file) if file.is_dir => match ext2::read_inode(e2, file.inode_num()) {
            Ok(inode) => inode,
            Err(code) => return error_reply(sender, code),
        },
        Some(_) => return error_reply(sender, ERR_NOT_DIR),
        None => return error_reply(sender, ERR_INVALID_HANDLE),
    };

    let buf = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, PAGE_SIZE) };
    let mut used = 0usize;
    let mut next = start;
    let mut end = true;
    let walked = ext2_dir::for_each_entry(e2, &dir, |index, ino, kind, name| {
        if (index as u64) < start {
            return true;
        }
        // The inode says what the entry is even where the entry does not.
        let (size, dt) = match ext2::read_inode(e2, ino) {
            Ok(i) if i.is_dir() => (i.size64(), DT_DIR),
            Ok(i) if i.is_regular() => (i.size64(), DT_REG),
            Ok(i) if i.i_mode & ext2::S_IFMT == ext2::S_IFLNK => (i.size64(), DT_LNK),
            Ok(i) => (i.size64(), DT_UNKNOWN),
            Err(_) => (0, if kind == ext2::FT_DIR { DT_DIR } else { DT_UNKNOWN }),
        };
        match put_dirent(&mut buf[..room], used, ino as u64, index as u64 + 1, size, dt, name) {
            Some(len) => {
                used += len;
                next = index as u64 + 1;
                true
            }
            None => {
                end = false;
                false
            }
        }
    });
    if let Err(code) = walked {
        return error_reply(sender, code);
    }
    reply_dirents(sender, used, next, end);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[vfs] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
