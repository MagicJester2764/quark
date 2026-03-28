/// ext2 filesystem structures and core operations.
///
/// Supports revision 0 ext2 with 1024-byte blocks, 128-byte inodes.
/// All on-disk structures are parsed from byte slices (no repr(C) transmute).

use crate::{
    read_u16, read_u32, CACHE_BUF_BASE, CLIENT_BUF, DISK_IO_BUF, ERR_IO, ERR_IS_DIR,
    ERR_NOT_FOUND, PAGE_SIZE, SECTOR_CACHE, TAG_DISK_OK, TAG_READ_SECTOR,
    TAG_READ_SECTORS, TAG_WRITE_SECTOR,
};
use quark_rt::ipc::Message;
use quark_rt::syscall;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const EXT2_MAGIC: u16 = 0xEF53;
pub const EXT2_ROOT_INO: u32 = 2;

// Inode mode: file type
pub const S_IFMT: u16 = 0xF000;
pub const S_IFDIR: u16 = 0x4000;
pub const S_IFREG: u16 = 0x8000;

// Directory entry file types
pub const FT_REG_FILE: u8 = 1;
pub const FT_DIR: u8 = 2;

// ---------------------------------------------------------------------------
// On-disk structures
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Ext2Inode {
    pub i_mode: u16,
    pub i_uid: u16,
    pub i_size: u32,
    pub i_atime: u32,
    pub i_ctime: u32,
    pub i_mtime: u32,
    pub i_dtime: u32,
    pub i_gid: u16,
    pub i_links_count: u16,
    pub i_blocks: u32, // in 512-byte units
    pub i_flags: u32,
    pub i_block: [u32; 15], // 0-11 direct, 12 indirect, 13 double, 14 triple
}

impl Ext2Inode {
    pub const fn empty() -> Self {
        Self {
            i_mode: 0,
            i_uid: 0,
            i_size: 0,
            i_atime: 0,
            i_ctime: 0,
            i_mtime: 0,
            i_dtime: 0,
            i_gid: 0,
            i_links_count: 0,
            i_blocks: 0,
            i_flags: 0,
            i_block: [0; 15],
        }
    }

    pub fn is_dir(&self) -> bool {
        self.i_mode & S_IFMT == S_IFDIR
    }

    pub fn is_regular(&self) -> bool {
        self.i_mode & S_IFMT == S_IFREG
    }

    /// Parse a 128-byte inode from raw bytes.
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut i_block = [0u32; 15];
        for j in 0..15 {
            i_block[j] = read_u32(data, 40 + j * 4);
        }
        Self {
            i_mode: read_u16(data, 0),
            i_uid: read_u16(data, 2),
            i_size: read_u32(data, 4),
            i_atime: read_u32(data, 8),
            i_ctime: read_u32(data, 12),
            i_mtime: read_u32(data, 16),
            i_dtime: read_u32(data, 20),
            i_gid: read_u16(data, 24),
            i_links_count: read_u16(data, 26),
            i_blocks: read_u32(data, 28),
            i_flags: read_u32(data, 32),
            i_block,
        }
    }

    /// Serialize to 128 bytes for writing back to disk.
    pub fn to_bytes(&self, out: &mut [u8]) {
        write_u16(out, 0, self.i_mode);
        write_u16(out, 2, self.i_uid);
        write_u32(out, 4, self.i_size);
        write_u32(out, 8, self.i_atime);
        write_u32(out, 12, self.i_ctime);
        write_u32(out, 16, self.i_mtime);
        write_u32(out, 20, self.i_dtime);
        write_u16(out, 24, self.i_gid);
        write_u16(out, 26, self.i_links_count);
        write_u32(out, 28, self.i_blocks);
        write_u32(out, 32, self.i_flags);
        // i_osd1 at bytes 36-39 (OS-dependent, zero for us)
        write_u32(out, 36, 0);
        for j in 0..15 {
            write_u32(out, 40 + j * 4, self.i_block[j]);
        }
        // Zero remaining bytes (100..128)
        for i in 100..128 {
            if i < out.len() {
                out[i] = 0;
            }
        }
    }
}

#[derive(Clone, Copy)]
pub struct BlockGroupDesc {
    pub bg_block_bitmap: u32,
    pub bg_inode_bitmap: u32,
    pub bg_inode_table: u32,
    pub bg_free_blocks_count: u16,
    pub bg_free_inodes_count: u16,
    pub bg_used_dirs_count: u16,
}

impl BlockGroupDesc {
    pub const fn empty() -> Self {
        Self {
            bg_block_bitmap: 0,
            bg_inode_bitmap: 0,
            bg_inode_table: 0,
            bg_free_blocks_count: 0,
            bg_free_inodes_count: 0,
            bg_used_dirs_count: 0,
        }
    }

    pub fn from_bytes(data: &[u8], off: usize) -> Self {
        Self {
            bg_block_bitmap: read_u32(data, off),
            bg_inode_bitmap: read_u32(data, off + 4),
            bg_inode_table: read_u32(data, off + 8),
            bg_free_blocks_count: read_u16(data, off + 12),
            bg_free_inodes_count: read_u16(data, off + 14),
            bg_used_dirs_count: read_u16(data, off + 16),
        }
    }

    pub fn write_to_bytes(&self, data: &mut [u8], off: usize) {
        write_u32(data, off, self.bg_block_bitmap);
        write_u32(data, off + 4, self.bg_inode_bitmap);
        write_u32(data, off + 8, self.bg_inode_table);
        write_u16(data, off + 12, self.bg_free_blocks_count);
        write_u16(data, off + 14, self.bg_free_inodes_count);
        write_u16(data, off + 16, self.bg_used_dirs_count);
    }
}

// ---------------------------------------------------------------------------
// Ext2 filesystem state
// ---------------------------------------------------------------------------

const MAX_BLOCK_GROUPS: usize = 128;

pub struct Ext2State {
    pub disk_tid: usize,
    pub buf_phys: usize,
    pub part_lba: u32,
    pub block_size: u32,
    pub sectors_per_block: u32,
    pub inodes_per_group: u32,
    pub blocks_per_group: u32,
    pub inode_size: u16,
    pub first_data_block: u32,
    pub num_block_groups: u32,
    pub total_blocks: u32,
    pub total_inodes: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub bgd_table: [BlockGroupDesc; MAX_BLOCK_GROUPS],
}

impl Ext2State {
    pub const fn empty() -> Self {
        Self {
            disk_tid: 0,
            buf_phys: 0,
            part_lba: 0,
            block_size: 1024,
            sectors_per_block: 2,
            inodes_per_group: 0,
            blocks_per_group: 0,
            inode_size: 128,
            first_data_block: 1,
            num_block_groups: 0,
            total_blocks: 0,
            total_inodes: 0,
            free_blocks_count: 0,
            free_inodes_count: 0,
            bgd_table: [BlockGroupDesc::empty(); MAX_BLOCK_GROUPS],
        }
    }

    /// Convert a block number to an absolute LBA.
    pub fn block_to_lba(&self, block: u32) -> u32 {
        self.part_lba + block * self.sectors_per_block
    }

    /// Read a sector via the sector cache.
    pub fn cached_read_sector(&self, abs_lba: u32) -> Result<&[u8], ()> {
        let cache = unsafe { &mut SECTOR_CACHE };
        let idx = if let Some(i) = cache.lookup(abs_lba) {
            i
        } else {
            raw_read_sector(self.disk_tid, self.buf_phys, abs_lba)?;
            cache.insert(abs_lba, DISK_IO_BUF)
        };
        Ok(unsafe { core::slice::from_raw_parts((CACHE_BUF_BASE + idx * 512) as *const u8, 512) })
    }

    /// Read a sector relative to partition start, through the cache.
    pub fn cached_read_part_sector(&self, lba: u32) -> Result<&[u8], ()> {
        self.cached_read_sector(self.part_lba + lba)
    }

    /// Write a sector (from DISK_IO_BUF) at an absolute LBA.
    pub fn write_sector_abs(&self, abs_lba: u32) -> Result<(), ()> {
        let msg = Message {
            sender: 0,
            tag: TAG_WRITE_SECTOR,
            data: [abs_lba as u64, self.buf_phys as u64, 0, 0, 0, 0],
        };
        let mut reply = Message::empty();
        if syscall::sys_call(self.disk_tid, &msg, &mut reply).is_err() {
            return Err(());
        }
        if reply.tag != TAG_DISK_OK {
            return Err(());
        }
        // Invalidate cache for this sector
        unsafe { SECTOR_CACHE.invalidate(abs_lba) };
        Ok(())
    }

    /// Read a block into a temporary buffer and return a copy of one sector within it.
    /// For 1K blocks with 512-byte sectors, a block spans 2 sectors.
    pub fn read_block_sector(&self, block: u32, sector_offset: u32) -> Result<&[u8], ()> {
        let abs_lba = self.block_to_lba(block) + sector_offset;
        self.cached_read_sector(abs_lba)
    }

    /// Prefetch consecutive sectors into the cache.
    pub fn prefetch_sectors(&self, start_abs_lba: u32, count: u32) {
        let count = count.min(8) as usize;
        let cache = unsafe { &mut SECTOR_CACHE };

        let mut all_cached = true;
        for i in 0..count {
            if cache.lookup(start_abs_lba + i as u32).is_none() {
                all_cached = false;
                break;
            }
        }
        if all_cached {
            return;
        }

        if raw_read_sectors(self.disk_tid, self.buf_phys, start_abs_lba, count as u32).is_err() {
            return;
        }

        for i in 0..count {
            let lba = start_abs_lba + i as u32;
            if cache.lookup(lba).is_none() {
                cache.insert(lba, DISK_IO_BUF + i * 512);
            }
        }
    }

    /// Prefetch all sectors of a block.
    pub fn prefetch_block(&self, block: u32) {
        self.prefetch_sectors(self.block_to_lba(block), self.sectors_per_block);
    }

    /// Number of u32 block pointers per indirect block.
    pub fn ptrs_per_block(&self) -> u32 {
        self.block_size / 4
    }
}

// ---------------------------------------------------------------------------
// Raw disk I/O helpers (standalone, for init before Ext2State exists)
// ---------------------------------------------------------------------------

pub fn raw_read_sector(disk_tid: usize, buf_phys: usize, lba: u32) -> Result<(), ()> {
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

pub fn raw_read_sectors(
    disk_tid: usize,
    buf_phys: usize,
    start_lba: u32,
    count: u32,
) -> Result<(), ()> {
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

// ---------------------------------------------------------------------------
// Byte helpers
// ---------------------------------------------------------------------------

pub fn write_u16(data: &mut [u8], off: usize, val: u16) {
    let bytes = val.to_le_bytes();
    data[off] = bytes[0];
    data[off + 1] = bytes[1];
}

pub fn write_u32(data: &mut [u8], off: usize, val: u32) {
    let bytes = val.to_le_bytes();
    data[off] = bytes[0];
    data[off + 1] = bytes[1];
    data[off + 2] = bytes[2];
    data[off + 3] = bytes[3];
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize ext2 state from the superblock at the given partition.
/// Superblock is at byte offset 1024 from partition start = sector 2 for 512-byte sectors.
pub fn init_ext2(disk_tid: usize, buf_phys: usize, part_lba: u32) -> Result<Ext2State, ()> {
    // Read superblock — it's at byte offset 1024, which is sector 2 (512*2=1024).
    raw_read_sector(disk_tid, buf_phys, part_lba + 2)?;
    let sb0 = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
    let mut sb_buf = [0u8; 1024];
    sb_buf[0..512].copy_from_slice(sb0);

    // Read second half of superblock (sector 3)
    raw_read_sector(disk_tid, buf_phys, part_lba + 3)?;
    let sb1 = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
    sb_buf[512..1024].copy_from_slice(sb1);

    let magic = read_u16(&sb_buf, 56);
    if magic != EXT2_MAGIC {
        return Err(());
    }

    let s_inodes_count = read_u32(&sb_buf, 0);
    let s_blocks_count = read_u32(&sb_buf, 4);
    let s_free_blocks_count = read_u32(&sb_buf, 12);
    let s_free_inodes_count = read_u32(&sb_buf, 16);
    let s_first_data_block = read_u32(&sb_buf, 20);
    let s_log_block_size = read_u32(&sb_buf, 24);
    let s_blocks_per_group = read_u32(&sb_buf, 32);
    let s_inodes_per_group = read_u32(&sb_buf, 40);
    let s_rev_level = read_u32(&sb_buf, 76);
    let s_inode_size = if s_rev_level >= 1 {
        read_u16(&sb_buf, 88)
    } else {
        128
    };

    let block_size = 1024u32 << s_log_block_size;
    let sectors_per_block = block_size / 512;

    let num_block_groups =
        (s_blocks_count + s_blocks_per_group - 1) / s_blocks_per_group;

    let mut ext2 = Ext2State {
        disk_tid,
        buf_phys,
        part_lba,
        block_size,
        sectors_per_block,
        inodes_per_group: s_inodes_per_group,
        blocks_per_group: s_blocks_per_group,
        inode_size: s_inode_size,
        first_data_block: s_first_data_block,
        num_block_groups,
        total_blocks: s_blocks_count,
        total_inodes: s_inodes_count,
        free_blocks_count: s_free_blocks_count,
        free_inodes_count: s_free_inodes_count,
        bgd_table: [BlockGroupDesc::empty(); MAX_BLOCK_GROUPS],
    };

    // Read block group descriptor table (starts at block after superblock).
    // For 1K blocks: superblock is block 1, BGD table starts at block 2.
    // For 4K blocks: superblock is in block 0 (bytes 1024-2047), BGD table at block 1.
    let bgd_block = if block_size == 1024 { 2 } else { 1 };
    let bgd_abs_lba = ext2.block_to_lba(bgd_block);

    // Each BGD is 32 bytes. With 512-byte sectors, 16 BGDs per sector.
    let bgd_sectors = ((num_block_groups * 32) + 511) / 512;
    let bgd_sectors = bgd_sectors.min(ext2.sectors_per_block * 4); // reasonable limit

    // Read BGD sectors
    let mut bgd_idx = 0u32;
    for s in 0..bgd_sectors {
        let data = ext2.cached_read_sector(bgd_abs_lba + s)?;
        let mut sec_buf = [0u8; 512];
        sec_buf.copy_from_slice(data);

        let entries_in_sector = (512 / 32).min((num_block_groups - bgd_idx) as usize);
        for e in 0..entries_in_sector {
            if (bgd_idx as usize) < MAX_BLOCK_GROUPS {
                ext2.bgd_table[bgd_idx as usize] =
                    BlockGroupDesc::from_bytes(&sec_buf, e * 32);
            }
            bgd_idx += 1;
        }
    }

    Ok(ext2)
}

// ---------------------------------------------------------------------------
// Inode read/write
// ---------------------------------------------------------------------------

/// Read an inode from disk.
pub fn read_inode(ext2: &Ext2State, inode_num: u32) -> Result<Ext2Inode, u64> {
    if inode_num == 0 || inode_num > ext2.total_inodes {
        return Err(ERR_NOT_FOUND);
    }

    let group = (inode_num - 1) / ext2.inodes_per_group;
    let index = (inode_num - 1) % ext2.inodes_per_group;

    if group as usize >= MAX_BLOCK_GROUPS {
        return Err(ERR_IO);
    }

    let inode_table_block = ext2.bgd_table[group as usize].bg_inode_table;
    let byte_offset = index * ext2.inode_size as u32;
    let block_offset = byte_offset / ext2.block_size;
    let offset_in_block = byte_offset % ext2.block_size;

    let block = inode_table_block + block_offset;
    let sector_in_block = offset_in_block / 512;
    let offset_in_sector = (offset_in_block % 512) as usize;

    let abs_lba = ext2.block_to_lba(block) + sector_in_block;

    // If inode spans two sectors, read both
    if offset_in_sector + 128 > 512 {
        // Inode spans a sector boundary — read two sectors and assemble
        let data0 = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
        let mut inode_buf = [0u8; 128];
        let first_part = 512 - offset_in_sector;
        inode_buf[..first_part].copy_from_slice(&data0[offset_in_sector..]);

        let data1 = ext2.cached_read_sector(abs_lba + 1).map_err(|_| ERR_IO)?;
        inode_buf[first_part..128].copy_from_slice(&data1[..128 - first_part]);

        Ok(Ext2Inode::from_bytes(&inode_buf))
    } else {
        let data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
        // Need to copy because cached data reference may be invalidated
        let mut sec_buf = [0u8; 512];
        sec_buf.copy_from_slice(data);
        Ok(Ext2Inode::from_bytes(&sec_buf[offset_in_sector..]))
    }
}

/// Write an inode back to disk.
pub fn write_inode(ext2: &Ext2State, inode_num: u32, inode: &Ext2Inode) -> Result<(), u64> {
    if inode_num == 0 || inode_num > ext2.total_inodes {
        return Err(ERR_NOT_FOUND);
    }

    let group = (inode_num - 1) / ext2.inodes_per_group;
    let index = (inode_num - 1) % ext2.inodes_per_group;

    let inode_table_block = ext2.bgd_table[group as usize].bg_inode_table;
    let byte_offset = index * ext2.inode_size as u32;
    let block_offset = byte_offset / ext2.block_size;
    let offset_in_block = byte_offset % ext2.block_size;

    let block = inode_table_block + block_offset;
    let sector_in_block = offset_in_block / 512;
    let offset_in_sector = (offset_in_block % 512) as usize;

    let abs_lba = ext2.block_to_lba(block) + sector_in_block;

    let mut inode_bytes = [0u8; 128];
    inode.to_bytes(&mut inode_bytes);

    if offset_in_sector + 128 > 512 {
        // Spans sector boundary
        let first_part = 512 - offset_in_sector;

        // Read-modify-write first sector
        raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
        let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        buf[offset_in_sector..].copy_from_slice(&inode_bytes[..first_part]);
        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

        // Read-modify-write second sector
        raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba + 1).map_err(|_| ERR_IO)?;
        let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        buf[..128 - first_part].copy_from_slice(&inode_bytes[first_part..]);
        ext2.write_sector_abs(abs_lba + 1).map_err(|_| ERR_IO)?;
    } else {
        // Read-modify-write single sector
        raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
        let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        buf[offset_in_sector..offset_in_sector + 128].copy_from_slice(&inode_bytes);
        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Block mapping
// ---------------------------------------------------------------------------

/// Map a logical file block number to a physical disk block number.
/// Returns 0 if the block is not allocated (sparse file).
pub fn block_map(ext2: &Ext2State, inode: &Ext2Inode, logical: u32) -> Result<u32, u64> {
    let ppb = ext2.ptrs_per_block(); // pointers per indirect block (256 for 1K, 1024 for 4K)

    if logical < 12 {
        // Direct block
        return Ok(inode.i_block[logical as usize]);
    }

    let logical = logical - 12;
    if logical < ppb {
        // Single indirect
        let indirect_block = inode.i_block[12];
        if indirect_block == 0 {
            return Ok(0);
        }
        return read_block_ptr(ext2, indirect_block, logical);
    }

    let logical = logical - ppb;
    if logical < ppb * ppb {
        // Double indirect
        let dind_block = inode.i_block[13];
        if dind_block == 0 {
            return Ok(0);
        }
        let idx1 = logical / ppb;
        let idx2 = logical % ppb;
        let ind_block = read_block_ptr(ext2, dind_block, idx1)?;
        if ind_block == 0 {
            return Ok(0);
        }
        return read_block_ptr(ext2, ind_block, idx2);
    }

    let logical = logical - ppb * ppb;
    // Triple indirect
    let tind_block = inode.i_block[14];
    if tind_block == 0 {
        return Ok(0);
    }
    let idx1 = logical / (ppb * ppb);
    let idx2 = (logical / ppb) % ppb;
    let idx3 = logical % ppb;
    let dind = read_block_ptr(ext2, tind_block, idx1)?;
    if dind == 0 {
        return Ok(0);
    }
    let ind = read_block_ptr(ext2, dind, idx2)?;
    if ind == 0 {
        return Ok(0);
    }
    read_block_ptr(ext2, ind, idx3)
}

/// Read a u32 block pointer from an indirect block at the given index.
fn read_block_ptr(ext2: &Ext2State, block: u32, index: u32) -> Result<u32, u64> {
    let byte_offset = index * 4;
    let sector_in_block = byte_offset / 512;
    let offset_in_sector = (byte_offset % 512) as usize;

    let abs_lba = ext2.block_to_lba(block) + sector_in_block;
    let data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
    let mut sec_buf = [0u8; 512];
    sec_buf.copy_from_slice(data);
    Ok(read_u32(&sec_buf, offset_in_sector))
}

// ---------------------------------------------------------------------------
// File data read
// ---------------------------------------------------------------------------

/// Read file data into a client's physical page.
pub fn read_file_data(
    ext2: &Ext2State,
    inode: &Ext2Inode,
    client_phys: usize,
    offset: u32,
    max_bytes: u32,
) -> Result<u32, u64> {
    if inode.is_dir() {
        return Err(ERR_IS_DIR);
    }
    if offset >= inode.i_size {
        return Ok(0);
    }

    let available = inode.i_size - offset;
    let to_read = max_bytes.min(available).min(PAGE_SIZE as u32);
    if to_read == 0 {
        return Ok(0);
    }

    // Map client's physical page
    if syscall::sys_map_phys(client_phys, CLIENT_BUF, 1).is_err() {
        return Err(ERR_IO);
    }

    let bs = ext2.block_size;
    let mut written = 0u32;

    while written < to_read {
        let cur_offset = offset + written;
        let logical_block = cur_offset / bs;
        let offset_in_block = cur_offset % bs;

        let phys_block = block_map(ext2, inode, logical_block)?;
        if phys_block == 0 {
            // Sparse block — fill with zeros
            let copy_len = (bs - offset_in_block).min(to_read - written) as usize;
            unsafe {
                core::ptr::write_bytes(
                    (CLIENT_BUF + written as usize) as *mut u8,
                    0,
                    copy_len,
                );
            }
            written += copy_len as u32;
            continue;
        }

        // Prefetch the block's sectors
        ext2.prefetch_block(phys_block);

        // Read from the correct sector within this block
        let remaining_in_block = bs - offset_in_block;
        let mut to_copy_from_block = remaining_in_block.min(to_read - written);

        // Copy sector by sector within this block
        let mut block_pos = offset_in_block;
        while to_copy_from_block > 0 {
            let sec = block_pos / 512;
            let sec_off = (block_pos % 512) as usize;
            let abs_lba = ext2.block_to_lba(phys_block) + sec;
            let sec_data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;

            let copy_len = (512 - sec_off).min(to_copy_from_block as usize);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    sec_data.as_ptr().add(sec_off),
                    (CLIENT_BUF + written as usize) as *mut u8,
                    copy_len,
                );
            }

            written += copy_len as u32;
            block_pos += copy_len as u32;
            to_copy_from_block -= copy_len as u32;
        }
    }

    Ok(written)
}

// ---------------------------------------------------------------------------
// File data write
// ---------------------------------------------------------------------------

/// Write data from client's physical page into a file.
/// May extend the file if offset + len > current size.
pub fn write_file_data(
    ext2: &mut Ext2State,
    inode: &mut Ext2Inode,
    inode_num: u32,
    client_phys: usize,
    offset: u32,
    len: u32,
) -> Result<u32, u64> {
    if inode.is_dir() {
        return Err(ERR_IS_DIR);
    }

    let to_write = len.min(PAGE_SIZE as u32);
    if to_write == 0 {
        return Ok(0);
    }

    // Map client's physical page
    if syscall::sys_map_phys(client_phys, CLIENT_BUF, 1).is_err() {
        return Err(ERR_IO);
    }

    let bs = ext2.block_size;

    // Extend file if needed
    let end_offset = offset + to_write;
    let blocks_needed = (end_offset + bs - 1) / bs;
    let current_blocks = (inode.i_size + bs - 1) / bs;

    if blocks_needed > current_blocks {
        // Allocate blocks for extension
        for logical in current_blocks..blocks_needed {
            let existing = block_map(ext2, inode, logical)?;
            if existing == 0 {
                let new_block = crate::ext2_alloc::alloc_block(ext2).map_err(|_| ERR_IO)?;
                // Zero the new block
                zero_block(ext2, new_block)?;
                set_block_ptr(ext2, inode, logical, new_block)?;
                // Update i_blocks (in 512-byte units)
                inode.i_blocks += ext2.block_size / 512;
            }
        }
    }

    let mut written = 0u32;

    while written < to_write {
        let cur_offset = offset + written;
        let logical_block = cur_offset / bs;
        let offset_in_block = cur_offset % bs;

        let phys_block = block_map(ext2, inode, logical_block)?;
        if phys_block == 0 {
            return Err(ERR_IO);
        }

        let remaining_in_block = bs - offset_in_block;
        let mut to_write_in_block = remaining_in_block.min(to_write - written);

        // Write sector by sector
        let mut block_pos = offset_in_block;
        while to_write_in_block > 0 {
            let sec = block_pos / 512;
            let sec_off = (block_pos % 512) as usize;
            let abs_lba = ext2.block_to_lba(phys_block) + sec;

            // Read-modify-write
            raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
            let buf =
                unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

            let copy_len = (512 - sec_off).min(to_write_in_block as usize);
            let src = unsafe {
                core::slice::from_raw_parts((CLIENT_BUF + written as usize) as *const u8, copy_len)
            };
            buf[sec_off..sec_off + copy_len].copy_from_slice(src);

            ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

            written += copy_len as u32;
            block_pos += copy_len as u32;
            to_write_in_block -= copy_len as u32;
        }
    }

    // Update file size if we wrote past the end
    if end_offset > inode.i_size {
        inode.i_size = end_offset;
    }

    // Write inode back
    write_inode(ext2, inode_num, inode)?;

    Ok(written)
}

/// Zero a disk block.
pub fn zero_block(ext2: &Ext2State, block: u32) -> Result<(), u64> {
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
    for i in 0..512 {
        buf[i] = 0;
    }
    for s in 0..ext2.sectors_per_block {
        let abs_lba = ext2.block_to_lba(block) + s;
        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;
    }
    Ok(())
}

/// Set a block pointer in the inode's block map (handles indirect allocation).
pub fn set_block_ptr(
    ext2: &mut Ext2State,
    inode: &mut Ext2Inode,
    logical: u32,
    phys_block: u32,
) -> Result<(), u64> {
    let ppb = ext2.ptrs_per_block();

    if logical < 12 {
        inode.i_block[logical as usize] = phys_block;
        return Ok(());
    }

    let logical = logical - 12;
    if logical < ppb {
        // Single indirect
        if inode.i_block[12] == 0 {
            let ind = crate::ext2_alloc::alloc_block(ext2).map_err(|_| ERR_IO)?;
            zero_block(ext2, ind)?;
            inode.i_block[12] = ind;
            inode.i_blocks += ext2.block_size / 512;
        }
        write_block_ptr(ext2, inode.i_block[12], logical, phys_block)?;
        return Ok(());
    }

    let logical = logical - ppb;
    if logical < ppb * ppb {
        // Double indirect
        if inode.i_block[13] == 0 {
            let dind = crate::ext2_alloc::alloc_block(ext2).map_err(|_| ERR_IO)?;
            zero_block(ext2, dind)?;
            inode.i_block[13] = dind;
            inode.i_blocks += ext2.block_size / 512;
        }
        let idx1 = logical / ppb;
        let idx2 = logical % ppb;
        let mut ind = read_block_ptr(ext2, inode.i_block[13], idx1)?;
        if ind == 0 {
            ind = crate::ext2_alloc::alloc_block(ext2).map_err(|_| ERR_IO)?;
            zero_block(ext2, ind)?;
            write_block_ptr(ext2, inode.i_block[13], idx1, ind)?;
            inode.i_blocks += ext2.block_size / 512;
        }
        write_block_ptr(ext2, ind, idx2, phys_block)?;
        return Ok(());
    }

    // Triple indirect — unlikely for our rootfs but handle it
    Err(ERR_IO) // too deep for now
}

/// Write a u32 block pointer into an indirect block.
pub fn write_block_ptr(ext2: &Ext2State, block: u32, index: u32, value: u32) -> Result<(), u64> {
    let byte_offset = index * 4;
    let sector_in_block = byte_offset / 512;
    let offset_in_sector = (byte_offset % 512) as usize;

    let abs_lba = ext2.block_to_lba(block) + sector_in_block;

    // Read-modify-write
    raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
    let bytes = value.to_le_bytes();
    buf[offset_in_sector..offset_in_sector + 4].copy_from_slice(&bytes);
    ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Permission checking
// ---------------------------------------------------------------------------

/// Check if a user has the required permission bits on an inode.
/// `required` uses standard rwx bit values: 4=read, 2=write, 1=execute.
pub fn check_permission(inode: &Ext2Inode, uid: u32, gid: u32, required: u16) -> bool {
    // Root bypasses all checks
    if uid == 0 {
        return true;
    }

    let mode = inode.i_mode & 0o777;
    let bits = if uid == inode.i_uid as u32 {
        (mode >> 6) & 7
    } else if gid == inode.i_gid as u32 {
        (mode >> 3) & 7
    } else {
        mode & 7
    };

    bits & required == required
}

// ---------------------------------------------------------------------------
// Superblock flush
// ---------------------------------------------------------------------------

/// Write back the superblock's free block/inode counts.
pub fn flush_superblock(ext2: &Ext2State) -> Result<(), u64> {
    // Read superblock sector (offset 1024 = abs sector part_lba+2)
    let abs_lba = ext2.part_lba + 2;
    raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };

    // Update free counts (offsets within the first 512 bytes of the superblock)
    write_u32(buf, 12, ext2.free_blocks_count);
    write_u32(buf, 16, ext2.free_inodes_count);

    ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;
    Ok(())
}

/// Write back a block group descriptor.
pub fn flush_bgd(ext2: &Ext2State, group: u32) -> Result<(), u64> {
    let bgd_block = if ext2.block_size == 1024 { 2 } else { 1 };
    let bgd_byte_offset = group * 32;
    let bgd_sector = bgd_byte_offset / 512;
    let bgd_offset_in_sector = (bgd_byte_offset % 512) as usize;

    let abs_lba = ext2.block_to_lba(bgd_block) + bgd_sector;

    // Read-modify-write
    raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
    ext2.bgd_table[group as usize].write_to_bytes(buf, bgd_offset_in_sector);
    ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;

    Ok(())
}
