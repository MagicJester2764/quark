//! ext4 metadata checksums.
//!
//! With `RO_COMPAT_METADATA_CSUM` every piece of filesystem metadata carries a
//! CRC-32C of itself: the superblock, each group descriptor, each allocation
//! bitmap, each inode, each directory block. A reader may ignore them, which
//! is why the feature is only read-only-compatible — but a writer that ignores
//! them leaves behind metadata that says it is corrupt. `mkfs.ext4` turns this
//! on by default, so implementing it is what makes a modern ext4 filesystem
//! writable rather than merely readable.
//!
//! Each checksum is seeded by the one for the thing that contains it, which is
//! what stops a valid block being moved somewhere it does not belong:
//!
//! ```text
//!     filesystem seed  = crc32c(~0, s_uuid)      (or s_checksum_seed)
//!            |
//!            +--> group descriptor = crc32c(seed, group#) over the descriptor
//!            +--> bitmaps          = crc32c(seed, bitmap bytes)
//!            |
//!            +--> inode seed = crc32c(crc32c(seed, inode#), i_generation)
//!                     |
//!                     +--> inode          = over the whole inode
//!                     +--> directory block= over the block but its tail
//! ```
//!
//! A checksum field is always zeroed for the purpose of computing it, and the
//! result stored afterwards. Getting that wrong produces a checksum that is
//! stable and wrong, which `e2fsck` reports and nothing else notices.

use crate::ext2::{raw_read_sector, Ext2State};
use crate::ext4;
use crate::{DISK_IO_BUF, ERR_IO};

// ---------------------------------------------------------------------------
// CRC-32C
// ---------------------------------------------------------------------------

/// Castagnoli's polynomial, reflected. Not the CRC-32 of zip and Ethernet:
/// ext4 uses this one, and the two produce different values for everything.
const POLY: u32 = 0x82F6_3B78;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ POLY } else { crc >> 1 };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// CRC-32C of `data`, continuing from `seed`.
///
/// Chains: `crc32c(crc32c(s, a), b)` equals the checksum of `a` followed by
/// `b`, which is what lets a checksum be seeded by its container's.
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    let mut crc = seed;
    for &b in data {
        crc = (crc >> 8) ^ TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    crc
}

fn crc32c_u32(seed: u32, v: u32) -> u32 {
    crc32c(seed, &v.to_le_bytes())
}

// ---------------------------------------------------------------------------
// Where each checksum lives
// ---------------------------------------------------------------------------

/// `s_checksum`, at the very end of the 1024-byte superblock.
const SB_CSUM_OFFSET: usize = 0x3FC;
const SB_UUID_OFFSET: usize = 0x68;
const SB_CSUM_SEED_OFFSET: usize = 0x270;

/// `bg_checksum` within a group descriptor.
const BG_CSUM_OFFSET: usize = 0x1E;
const BG_BLOCK_BITMAP_CSUM_LO: usize = 0x18;
const BG_INODE_BITMAP_CSUM_LO: usize = 0x1A;
const BG_BLOCK_BITMAP_CSUM_HI: usize = 0x38;
const BG_INODE_BITMAP_CSUM_HI: usize = 0x3A;

/// `i_checksum_lo`, in the OS-dependent area of every inode.
const INODE_CSUM_LO: usize = 0x7C;
/// `i_checksum_hi`, past the 128-byte inode, present only if `i_extra_isize`
/// reaches it.
const INODE_CSUM_HI: usize = 0x82;
const GOOD_OLD_INODE_SIZE: usize = 128;
/// `i_extra_isize`, the first field of the area past the classic inode.
const INODE_EXTRA_ISIZE: usize = 128;
const INODE_GENERATION: usize = 100;

/// The fake directory entry that carries a directory block's checksum.
pub const DIRENT_TAIL_SIZE: usize = 12;
/// Its `file_type`, chosen so no real entry can be mistaken for it.
const DIRENT_TAIL_FT: u8 = 0xDE;

// ---------------------------------------------------------------------------
// Seeds
// ---------------------------------------------------------------------------

/// Does this filesystem checksum its metadata?
pub fn enabled(ext2: &Ext2State) -> bool {
    ext2.feature_ro_compat & ext4::RO_COMPAT_METADATA_CSUM != 0
}

/// Work out the filesystem's seed from the superblock, once at mount.
///
/// `INCOMPAT_CSUM_SEED` means the seed is stored rather than derived, so that
/// the UUID can be changed without rewriting every checksum on the volume.
pub fn filesystem_seed(sb: &[u8], feature_incompat: u32) -> u32 {
    if feature_incompat & ext4::INCOMPAT_CSUM_SEED != 0 {
        u32::from_le_bytes([
            sb[SB_CSUM_SEED_OFFSET],
            sb[SB_CSUM_SEED_OFFSET + 1],
            sb[SB_CSUM_SEED_OFFSET + 2],
            sb[SB_CSUM_SEED_OFFSET + 3],
        ])
    } else {
        crc32c(!0, &sb[SB_UUID_OFFSET..SB_UUID_OFFSET + 16])
    }
}

/// The seed for everything belonging to one inode.
///
/// Includes the generation, so that a block belonging to a deleted file cannot
/// pass as one belonging to the file that reuses its inode number.
pub fn inode_seed(ext2: &Ext2State, inode_num: u32, generation: u32) -> u32 {
    crc32c_u32(crc32c_u32(ext2.csum_seed, inode_num), generation)
}

// ---------------------------------------------------------------------------
// Superblock
// ---------------------------------------------------------------------------

/// Set `s_checksum` over the 1020 bytes preceding it.
///
/// `sb` must be the whole 1024-byte superblock. Seeded with `~0` rather than
/// the filesystem seed, since the superblock is what the seed comes from.
pub fn set_superblock(sb: &mut [u8]) {
    if sb.len() < 1024 {
        return;
    }
    let csum = crc32c(!0, &sb[..SB_CSUM_OFFSET]);
    sb[SB_CSUM_OFFSET..SB_CSUM_OFFSET + 4].copy_from_slice(&csum.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Group descriptor
// ---------------------------------------------------------------------------

/// Set `bg_checksum` over the descriptor, with the field itself read as zero.
pub fn set_group_desc(ext2: &Ext2State, group: u32, desc: &mut [u8]) {
    if !enabled(ext2) || desc.len() < BG_CSUM_OFFSET + 2 {
        return;
    }
    let mut csum = crc32c_u32(ext2.csum_seed, group);
    csum = crc32c(csum, &desc[..BG_CSUM_OFFSET]);
    csum = crc32c(csum, &[0u8, 0u8]);
    if desc.len() > BG_CSUM_OFFSET + 2 {
        csum = crc32c(csum, &desc[BG_CSUM_OFFSET + 2..]);
    }
    let v = (csum & 0xFFFF) as u16;
    desc[BG_CSUM_OFFSET..BG_CSUM_OFFSET + 2].copy_from_slice(&v.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Bitmaps
// ---------------------------------------------------------------------------

/// Scratch for reading a whole allocation bitmap.
///
/// A bitmap covers one block, and the largest ext4 block is 64 KiB — but this
/// server only mounts filesystems whose blocks it reads a sector at a time,
/// and 4 KiB blocks give 32768 blocks per group, which is 4 KiB of bitmap.
static mut BITMAP_BUF: [u8; 4096] = [0; 4096];

fn read_bitmap(ext2: &Ext2State, block: u32, bytes: usize) -> Result<&'static [u8], u64> {
    let bytes = bytes.min(unsafe { (*core::ptr::addr_of!(BITMAP_BUF)).len() });
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP_BUF) };
    let sectors = bytes.div_ceil(512);
    for s in 0..sectors {
        let abs_lba = ext2.block_to_lba(block) + s as u32;
        raw_read_sector(ext2.disk_tid, ext2.buf_phys, abs_lba).map_err(|_| ERR_IO)?;
        let disk = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
        let take = (bytes - s * 512).min(512);
        buf[s * 512..s * 512 + take].copy_from_slice(&disk[..take]);
    }
    Ok(&buf[..bytes])
}

/// Recompute both bitmap checksums for a group and store them in its
/// descriptor.
///
/// Reads the bitmaps back off the disk rather than tracking them: the
/// allocator writes a single sector of one, and the checksum covers all of it.
/// Call before writing the descriptor, since these fields are inside it and
/// the descriptor's own checksum covers them.
pub fn refresh_bitmaps(ext2: &mut Ext2State, group: u32, desc: &mut [u8]) -> Result<(), u64> {
    if !enabled(ext2) {
        return Ok(());
    }

    let bgd = ext2.bgd_table[group as usize];
    let block_bytes = (ext2.blocks_per_group / 8) as usize;
    let inode_bytes = (ext2.inodes_per_group / 8) as usize;

    let block_bitmap = ext2.block32(bgd.bg_block_bitmap)?;
    let csum = crc32c(ext2.csum_seed, read_bitmap(ext2, block_bitmap, block_bytes)?);
    store16(desc, BG_BLOCK_BITMAP_CSUM_LO, csum as u16);
    if desc.len() >= BG_BLOCK_BITMAP_CSUM_HI + 2 {
        store16(desc, BG_BLOCK_BITMAP_CSUM_HI, (csum >> 16) as u16);
    }

    let inode_bitmap = ext2.block32(bgd.bg_inode_bitmap)?;
    let csum = crc32c(ext2.csum_seed, read_bitmap(ext2, inode_bitmap, inode_bytes)?);
    store16(desc, BG_INODE_BITMAP_CSUM_LO, csum as u16);
    if desc.len() >= BG_INODE_BITMAP_CSUM_HI + 2 {
        store16(desc, BG_INODE_BITMAP_CSUM_HI, (csum >> 16) as u16);
    }

    Ok(())
}

fn store16(buf: &mut [u8], off: usize, v: u16) {
    if off + 2 <= buf.len() {
        buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Inode
// ---------------------------------------------------------------------------

/// Whether this inode is large enough to hold the high half of its checksum.
fn has_csum_hi(ext2: &Ext2State, raw: &[u8]) -> bool {
    if (ext2.inode_size as usize) <= GOOD_OLD_INODE_SIZE || raw.len() < INODE_EXTRA_ISIZE + 2 {
        return false;
    }
    let extra = u16::from_le_bytes([raw[INODE_EXTRA_ISIZE], raw[INODE_EXTRA_ISIZE + 1]]) as usize;
    GOOD_OLD_INODE_SIZE + extra >= INODE_CSUM_HI + 2
}

/// Set an inode's checksum. `raw` must be the whole inode, `inode_size` bytes.
pub fn set_inode(ext2: &Ext2State, inode_num: u32, raw: &mut [u8]) {
    if !enabled(ext2) || raw.len() < GOOD_OLD_INODE_SIZE {
        return;
    }

    let generation = u32::from_le_bytes([
        raw[INODE_GENERATION],
        raw[INODE_GENERATION + 1],
        raw[INODE_GENERATION + 2],
        raw[INODE_GENERATION + 3],
    ]);
    let hi = has_csum_hi(ext2, raw);

    // Everything up to the low half, then two zeros in its place, then on to
    // the end of the classic inode.
    let mut csum = inode_seed(ext2, inode_num, generation);
    csum = crc32c(csum, &raw[..INODE_CSUM_LO]);
    csum = crc32c(csum, &[0u8, 0u8]);
    csum = crc32c(csum, &raw[INODE_CSUM_LO + 2..GOOD_OLD_INODE_SIZE]);

    if raw.len() > GOOD_OLD_INODE_SIZE {
        csum = crc32c(csum, &raw[GOOD_OLD_INODE_SIZE..INODE_CSUM_HI]);
        let mut offset = INODE_CSUM_HI;
        if hi {
            csum = crc32c(csum, &[0u8, 0u8]);
            offset += 2;
        }
        csum = crc32c(csum, &raw[offset..]);
    }

    store16(raw, INODE_CSUM_LO, csum as u16);
    if hi {
        store16(raw, INODE_CSUM_HI, (csum >> 16) as u16);
    }
}

// ---------------------------------------------------------------------------
// Directory blocks
// ---------------------------------------------------------------------------

/// Do the last twelve bytes of this block look like a checksum tail?
///
/// The tail is disguised as an unused directory entry — inode 0, so every
/// reader skips it — with an impossible file type to tell it from a genuinely
/// deleted one.
pub fn has_dirent_tail(block: &[u8]) -> bool {
    let n = block.len();
    if n < DIRENT_TAIL_SIZE {
        return false;
    }
    let t = &block[n - DIRENT_TAIL_SIZE..];
    u32::from_le_bytes([t[0], t[1], t[2], t[3]]) == 0
        && u16::from_le_bytes([t[4], t[5]]) == DIRENT_TAIL_SIZE as u16
        && t[6] == 0
        && t[7] == DIRENT_TAIL_FT
}

/// Write the tail's fixed fields. Call when laying out a fresh directory
/// block, before [`set_dirblock`] fills in the checksum.
pub fn init_dirent_tail(block: &mut [u8]) {
    let n = block.len();
    if n < DIRENT_TAIL_SIZE {
        return;
    }
    let t = &mut block[n - DIRENT_TAIL_SIZE..];
    t[0..4].copy_from_slice(&0u32.to_le_bytes());
    t[4..6].copy_from_slice(&(DIRENT_TAIL_SIZE as u16).to_le_bytes());
    t[6] = 0;
    t[7] = DIRENT_TAIL_FT;
    t[8..12].copy_from_slice(&0u32.to_le_bytes());
}

/// Set a directory block's checksum, which covers everything but its tail.
pub fn set_dirblock(ext2: &Ext2State, dir_seed: u32, block: &mut [u8]) {
    if !enabled(ext2) || !has_dirent_tail(block) {
        return;
    }
    let n = block.len();
    let csum = crc32c(dir_seed, &block[..n - DIRENT_TAIL_SIZE]);
    block[n - 4..].copy_from_slice(&csum.to_le_bytes());
}

/// Bytes of a directory block a real entry may occupy.
///
/// The tail is not an entry the allocator may reuse, however much it looks
/// like free space: overwriting it destroys the checksum, and reusing it as an
/// entry destroys the block.
pub fn dir_usable_len(ext2: &Ext2State, block_size: u32) -> u32 {
    if enabled(ext2) {
        block_size - DIRENT_TAIL_SIZE as u32
    } else {
        block_size
    }
}
