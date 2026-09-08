//! ext4: feature flags and extent trees.
//!
//! ext4 is ext2 with the same superblock, the same block groups, the same
//! inode table and the same directory entries — so most of this server did not
//! have to change. What changed is how an inode says where its data is.
//!
//! ext2 stores fifteen block pointers: twelve direct, then single, double and
//! triple indirect. A large file therefore costs an extra read per indirect
//! level, and a contiguous file wastes a pointer per block describing what a
//! range could have said in one entry.
//!
//! ext4 reuses the same sixty bytes as the root of a B-tree of *extents*, each
//! naming a run of contiguous blocks. A file laid out contiguously needs one
//! entry however large it is, and four of them fit in the inode itself, so
//! most files need no extra read at all.
//!
//! ```text
//!     i_block[15], 60 bytes:
//!     +--------------+----------+----------+----------+----------+
//!     | extent hdr   | entry 0  | entry 1  | entry 2  | entry 3  |
//!     | magic 0xF30A |          |          |          |          |
//!     | entries,depth|          |          |          |          |
//!     +--------------+----------+----------+----------+----------+
//!        depth == 0: entries are extents, naming physical runs
//!        depth  > 0: entries are indices, naming blocks of more entries
//! ```

use crate::ext2::{read_block_bytes, Ext2Inode, Ext2State};
use crate::{ERR_IO, ERR_NOT_FOUND};

// ---------------------------------------------------------------------------
// Feature flags
// ---------------------------------------------------------------------------
//
// Three sets, and the difference between them is the whole compatibility
// story. A *compat* feature can be ignored entirely. A *ro_compat* feature
// changes something a writer must maintain, so an implementation that does not
// know it may still read. An *incompat* feature changes the format itself, so
// an implementation that does not know it must not touch the filesystem at all
// — reading would return the wrong bytes, silently.

pub const COMPAT_HAS_JOURNAL: u32 = 0x0004;
pub const COMPAT_DIR_INDEX: u32 = 0x0020;

pub const INCOMPAT_FILETYPE: u32 = 0x0002;
/// The journal holds committed data the filesystem does not yet reflect.
pub const INCOMPAT_RECOVER: u32 = 0x0004;
pub const INCOMPAT_META_BG: u32 = 0x0010;
pub const INCOMPAT_EXTENTS: u32 = 0x0040;
pub const INCOMPAT_64BIT: u32 = 0x0080;
pub const INCOMPAT_MMP: u32 = 0x0100;
pub const INCOMPAT_FLEX_BG: u32 = 0x0200;
/// The checksum seed is stored in the superblock rather than derived from the
/// UUID. Incompatible because a writer that did not know it would compute
/// checksums against the wrong seed.
pub const INCOMPAT_CSUM_SEED: u32 = 0x2000;
pub const INCOMPAT_INLINE_DATA: u32 = 0x8000;

pub const RO_COMPAT_SPARSE_SUPER: u32 = 0x0001;
pub const RO_COMPAT_LARGE_FILE: u32 = 0x0002;
pub const RO_COMPAT_HUGE_FILE: u32 = 0x0008;
pub const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
pub const RO_COMPAT_DIR_NLINK: u32 = 0x0020;
pub const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
pub const RO_COMPAT_BIGALLOC: u32 = 0x0200;
pub const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

/// Incompatible features this server understands well enough to mount.
///
/// `FILETYPE` and `FLEX_BG` cost nothing: the first is a field this already
/// reads, and the second only moves where a group's bitmaps live, which the
/// descriptor already says. `EXTENTS` and `64BIT` are what this module adds.
///
/// `CSUM_SEED` is here on a condition rather than on its own merits: it says
/// only where the checksum seed comes from, which matters to a writer of
/// checksums and to nobody else. `METADATA_CSUM` is not in
/// [`RO_COMPAT_WRITE_SUPPORTED`], so a filesystem using either is mounted
/// read-only and this server never computes a checksum at all. Implementing
/// checksums means honouring the seed in the same change.
pub const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE
    | INCOMPAT_EXTENTS
    | INCOMPAT_64BIT
    | INCOMPAT_FLEX_BG
    | INCOMPAT_CSUM_SEED;

/// Read-only-compatible features that also do not stop us *writing*.
///
/// The rest are not refusals of the filesystem, only of modifying it: an
/// unlisted one means something a writer would have to maintain and this does
/// not, so the mount stays read-only rather than quietly corrupting it.
pub const RO_COMPAT_WRITE_SUPPORTED: u32 = RO_COMPAT_SPARSE_SUPER
    | RO_COMPAT_LARGE_FILE
    | RO_COMPAT_HUGE_FILE
    | RO_COMPAT_DIR_NLINK
    | RO_COMPAT_EXTRA_ISIZE;

/// Set in `i_flags` when an inode's `i_block` is an extent tree.
pub const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
/// Set when the inode's data lives in the inode itself.
pub const EXT4_INLINE_DATA_FL: u32 = 0x1000_0000;

// ---------------------------------------------------------------------------
// Extent tree
// ---------------------------------------------------------------------------

const EXTENT_MAGIC: u16 = 0xF30A;
const EXTENT_HEADER_SIZE: usize = 12;
const EXTENT_ENTRY_SIZE: usize = 12;

/// A length above this marks the extent *uninitialised*: the blocks are
/// allocated but have never been written, and read as zeros.
const INIT_MAX_LEN: u16 = 32768;

/// How deep a tree this will walk before deciding it is looping.
///
/// ext4 never builds deeper than five levels, and a corrupt `eh_depth` that
/// pointed at itself would otherwise spin forever inside a filesystem server
/// every other program is blocked on.
const MAX_DEPTH: u16 = 5;

struct ExtentHeader {
    entries: u16,
    max: u16,
    depth: u16,
}

fn read_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn parse_header(buf: &[u8]) -> Result<ExtentHeader, u64> {
    if buf.len() < EXTENT_HEADER_SIZE || read_u16(buf, 0) != EXTENT_MAGIC {
        return Err(ERR_IO);
    }
    Ok(ExtentHeader {
        entries: read_u16(buf, 2),
        max: read_u16(buf, 4),
        depth: read_u16(buf, 6),
    })
}

/// Does this inode use extents rather than the ext2 pointer array?
pub fn uses_extents(inode: &Ext2Inode) -> bool {
    inode.i_flags & EXT4_EXTENTS_FL != 0
}

/// The inode's `i_block` as the 60 raw bytes the extent root occupies.
fn root_bytes(inode: &Ext2Inode) -> [u8; 60] {
    let mut out = [0u8; 60];
    for j in 0..15 {
        out[j * 4..j * 4 + 4].copy_from_slice(&inode.i_block[j].to_le_bytes());
    }
    out
}

/// Physical block holding logical block `logical` of `inode`, or 0 for a hole.
///
/// Walks from the root in the inode down to a leaf, choosing at each level the
/// last entry whose first logical block is not past the target. That is the
/// only entry that can cover it: entries are sorted and cover disjoint ranges.
pub fn extent_lookup(
    ext2: &Ext2State,
    inode: &Ext2Inode,
    logical: u32,
) -> Result<u32, u64> {
    let root = root_bytes(inode);
    let mut header = parse_header(&root)?;

    // The root lives in 60 bytes of inode, so it holds at most four entries.
    let entry_at = |i: usize, out: &mut [u8; EXTENT_ENTRY_SIZE]| -> Result<(), u64> {
        let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
        if off + EXTENT_ENTRY_SIZE > root.len() {
            return Err(ERR_IO);
        }
        out.copy_from_slice(&root[off..off + EXTENT_ENTRY_SIZE]);
        Ok(())
    };

    let mut in_root = true;
    let mut block: u32 = 0;
    let mut depth_guard = 0u16;

    loop {
        if depth_guard > MAX_DEPTH {
            return Err(ERR_IO);
        }
        depth_guard += 1;

        // An interior node's entry count must fit the space it has, or a
        // corrupt value would walk us off the end of the block.
        let capacity = if in_root {
            (60 - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE
        } else {
            (ext2.block_size as usize - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE
        };
        if header.entries as usize > capacity || header.max as usize > capacity {
            return Err(ERR_IO);
        }

        // Last entry whose first block is <= the one we want.
        let mut chosen: Option<[u8; EXTENT_ENTRY_SIZE]> = None;
        let mut buf = [0u8; EXTENT_ENTRY_SIZE];
        for i in 0..header.entries as usize {
            if in_root {
                entry_at(i, &mut buf)?;
            } else {
                let off = EXTENT_HEADER_SIZE + i * EXTENT_ENTRY_SIZE;
                read_block_bytes(ext2, block, off, &mut buf)?;
            }
            if read_u32(&buf, 0) > logical {
                break;
            }
            chosen = Some(buf);
        }

        let entry = match chosen {
            Some(e) => e,
            // Before the first extent: a hole at the front of the file.
            None => return Ok(0),
        };

        if header.depth == 0 {
            // Leaf. ee_block, ee_len, ee_start_hi, ee_start_lo.
            let ee_block = read_u32(&entry, 0);
            let raw_len = read_u16(&entry, 4);
            let start_hi = read_u16(&entry, 6) as u64;
            let start_lo = read_u32(&entry, 8) as u64;

            // An uninitialised extent is allocated but unwritten. Reporting a
            // hole is exactly right for a reader: its blocks read as zeros,
            // and that is what a hole already means here.
            if raw_len > INIT_MAX_LEN {
                return Ok(0);
            }
            let len = raw_len as u32;
            if len == 0 || logical < ee_block || logical - ee_block >= len {
                return Ok(0); // hole between extents
            }

            let start = (start_hi << 32) | start_lo;
            let phys = start + (logical - ee_block) as u64;
            return u32::try_from(phys).map_err(|_| ERR_IO);
        }

        // Interior. ei_block, ei_leaf_lo, ei_leaf_hi.
        let leaf_lo = read_u32(&entry, 4) as u64;
        let leaf_hi = read_u16(&entry, 8) as u64;
        let next = (leaf_hi << 32) | leaf_lo;
        block = u32::try_from(next).map_err(|_| ERR_IO)?;
        if block == 0 {
            return Ok(0);
        }

        let mut hdr_buf = [0u8; EXTENT_HEADER_SIZE];
        read_block_bytes(ext2, block, 0, &mut hdr_buf)?;
        let next_header = parse_header(&hdr_buf)?;

        // Depth must strictly decrease on the way down.
        if next_header.depth >= header.depth {
            return Err(ERR_IO);
        }
        header = next_header;
        in_root = false;
    }
}

/// Set up an inode's `i_block` as an empty extent root.
///
/// A file created on an ext4 filesystem must use extents: the block pointer
/// array is not a valid alternative once `INCOMPAT_EXTENTS` is set, and a
/// reader would try to parse the pointers as an extent header.
pub fn init_extent_root(inode: &mut Ext2Inode) {
    let mut root = [0u8; 60];
    root[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
    root[2..4].copy_from_slice(&0u16.to_le_bytes()); // entries
    let max = ((60 - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE) as u16;
    root[4..6].copy_from_slice(&max.to_le_bytes());
    root[6..8].copy_from_slice(&0u16.to_le_bytes()); // depth: a leaf
    root[8..12].copy_from_slice(&0u32.to_le_bytes()); // generation

    for j in 0..15 {
        inode.i_block[j] = read_u32(&root, j * 4);
    }
    inode.i_flags |= EXT4_EXTENTS_FL;
}

/// Record that logical block `logical` of `inode` now lives at `phys`.
///
/// Only the root leaf is written, which is what a file that fits in four
/// extents needs. Growing past that wants a tree, and [`can_add_extent`] says
/// when a caller has run out of room rather than letting it corrupt one.
///
/// Extending the last extent by one block is the common case — a file written
/// sequentially into freshly allocated, mostly contiguous blocks — and costs
/// no new entry at all.
pub fn extent_insert(
    inode: &mut Ext2Inode,
    logical: u32,
    phys: u32,
) -> Result<(), u64> {
    let mut root = root_bytes(inode);
    let header = parse_header(&root)?;
    if header.depth != 0 {
        // A tree deep enough to have interior nodes needs a real insert, and
        // guessing at one would corrupt a file rather than fail to grow it.
        return Err(ERR_NOT_FOUND);
    }

    let capacity = ((60 - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE) as u16;
    let entries = header.entries;
    if entries > capacity {
        return Err(ERR_IO);
    }

    // Extend the last extent if this block continues it, both logically and
    // physically. Anything else needs an entry of its own.
    if entries > 0 {
        let off = EXTENT_HEADER_SIZE + (entries as usize - 1) * EXTENT_ENTRY_SIZE;
        let ee_block = read_u32(&root, off);
        let raw_len = read_u16(&root, off + 4);
        if raw_len < INIT_MAX_LEN {
            let start = ((read_u16(&root, off + 6) as u64) << 32) | read_u32(&root, off + 8) as u64;
            let len = raw_len as u32;
            if logical == ee_block + len && phys as u64 == start + len as u64 && raw_len < INIT_MAX_LEN - 1
            {
                root[off + 4..off + 6].copy_from_slice(&(raw_len + 1).to_le_bytes());
                store_root(inode, &root);
                return Ok(());
            }
        }
    }

    if entries >= capacity {
        return Err(ERR_NOT_FOUND);
    }

    let off = EXTENT_HEADER_SIZE + entries as usize * EXTENT_ENTRY_SIZE;
    root[off..off + 4].copy_from_slice(&logical.to_le_bytes());
    root[off + 4..off + 6].copy_from_slice(&1u16.to_le_bytes());
    root[off + 6..off + 8].copy_from_slice(&0u16.to_le_bytes()); // start_hi
    root[off + 8..off + 12].copy_from_slice(&phys.to_le_bytes());
    root[2..4].copy_from_slice(&(entries + 1).to_le_bytes());
    store_root(inode, &root);
    Ok(())
}

/// Whether [`extent_insert`] can still take a block for this inode.
///
/// Appending to the last extent always can; a new entry needs a free slot in
/// the root. A caller that has run out should stop growing the file rather
/// than allocate a block it cannot record.
pub fn can_add_extent(inode: &Ext2Inode, logical: u32, phys: u32) -> bool {
    let root = root_bytes(inode);
    let Ok(header) = parse_header(&root) else {
        return false;
    };
    if header.depth != 0 {
        return false;
    }
    let capacity = ((60 - EXTENT_HEADER_SIZE) / EXTENT_ENTRY_SIZE) as u16;
    if header.entries < capacity {
        return true;
    }
    if header.entries == 0 {
        return false;
    }
    let off = EXTENT_HEADER_SIZE + (header.entries as usize - 1) * EXTENT_ENTRY_SIZE;
    let ee_block = read_u32(&root, off);
    let raw_len = read_u16(&root, off + 4);
    if raw_len >= INIT_MAX_LEN - 1 {
        return false;
    }
    let start = ((read_u16(&root, off + 6) as u64) << 32) | read_u32(&root, off + 8) as u64;
    logical == ee_block + raw_len as u32 && phys as u64 == start + raw_len as u64
}

fn store_root(inode: &mut Ext2Inode, root: &[u8; 60]) {
    for j in 0..15 {
        inode.i_block[j] = read_u32(root, j * 4);
    }
}
