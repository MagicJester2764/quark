/// ext2 directory operations: find entry, resolve path, read dir entries, create entries.
///
/// ext2 directory entries are variable-length:
///   inode(4) + rec_len(2) + name_len(1) + file_type(1) + name(name_len)
/// Minimum entry size is 8 bytes. Entries are 4-byte aligned via rec_len padding.

use crate::ext2::{
    block_map, check_permission, read_inode, write_inode, zero_block,
    Ext2Inode, Ext2State,
    EXT2_ROOT_INO, FT_DIR,
};
use crate::devices::{self, Device};
use crate::protocol::{MAX_NAME, MAX_PATH};
use crate::{
    read_u16, read_u32, DISK_IO_BUF, ERR_IO, ERR_LOOP, ERR_NAME_TOO_LONG, ERR_NOT_DIR,
    ERR_NOT_FOUND, ERR_PERMISSION,
};
use core::sync::atomic::{AtomicU32, Ordering};
use crate::ext2::{write_u16, write_u32};

// ---------------------------------------------------------------------------
// Find a named entry in a directory
// ---------------------------------------------------------------------------

/// Search a directory inode for an entry with the given name.
/// Returns (inode_number, file_type) on success, None if not found.
pub fn find_entry(
    ext2: &Ext2State,
    dir_inode: &Ext2Inode,
    name: &[u8],
) -> Result<Option<(u32, u8)>, u64> {
    let bs = ext2.block_size;
    let total_blocks = (dir_inode.i_size + bs - 1) / bs;

    for logical in 0..total_blocks {
        let phys_block = block_map(ext2, dir_inode, logical)?;
        if phys_block == 0 {
            continue;
        }

        // Read entire block into local buffer to avoid sector-boundary issues
        let block_buf = read_block_buf(ext2, phys_block)?;

        let mut pos = 0u32;
        while pos < bs {
            let off = pos as usize;
            let entry_inode = read_u32(&block_buf, off);
            let rec_len = read_u16(&block_buf, off + 4) as u32;
            let name_len = block_buf[off + 6] as usize;
            let file_type = block_buf[off + 7];

            if rec_len == 0 {
                break;
            }

            if entry_inode != 0 && name_len == name.len() {
                let name_start = off + 8;
                if &block_buf[name_start..name_start + name_len] == name {
                    return Ok(Some((entry_inode, file_type)));
                }
            }

            pos += rec_len;
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Symbolic links one lookup may follow before it is taken for a loop, as
/// Linux counts them.
const MAX_LINKS: usize = 40;

/// The root's `dev` directory, whose names are the server's devices rather
/// than anything on the disk. 0 if the filesystem has none.
static DEV_DIR: AtomicU32 = AtomicU32::new(0);

/// Find the root's `dev` directory. Called once the filesystem is mounted.
pub fn note_dev_dir(ext2: &Ext2State) {
    let found = read_inode(ext2, EXT2_ROOT_INO)
        .and_then(|root| find_entry(ext2, &root, b"dev"))
        .ok()
        .flatten()
        .and_then(|(ino, _)| match read_inode(ext2, ino) {
            Ok(inode) if inode.is_dir() => Some(ino),
            _ => None,
        });
    DEV_DIR.store(found.unwrap_or(0), Ordering::Relaxed);
}

/// The inode of the root's `dev` directory, or 0.
pub fn dev_dir() -> u32 {
    DEV_DIR.load(Ordering::Relaxed)
}

/// What a lookup found.
pub enum Found {
    /// An inode: its number, itself, and the directory that holds it.
    Inode(u32, Ext2Inode, u32),
    /// One of the server's devices, reached through the root's `dev`.
    Device(Device),
}

/// The path being walked, rewritten in place as links expand. The server
/// serves one request at a time, so one is enough.
static mut WALK: [u8; MAX_PATH + 1] = [0; MAX_PATH + 1];

/// Look `path` up from directory `base` (a path starting with `/` from the
/// root), checking search permission on every directory it passes through.
///
/// A symbolic link met on the way is followed: its target replaces the part
/// of the path that named it, from the root if the target is absolute and
/// from the directory holding the link if not. The last component is
/// followed only if `follow_last` says so, or if a slash comes after it. More
/// than [`MAX_LINKS`] links in one lookup is `ERR_LOOP`.
pub fn resolve(
    ext2: &Ext2State,
    base: u32,
    path: &[u8],
    uid: u32,
    gid: u32,
    follow_last: bool,
) -> Result<Found, u64> {
    if path.len() > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    let walk = unsafe { &mut *core::ptr::addr_of_mut!(WALK) };
    walk[..path.len()].copy_from_slice(path);
    let mut len = path.len();
    let mut pos = 0usize;
    let mut links = 0usize;
    let mut cur_ino = if path.first() == Some(&b'/') { EXT2_ROOT_INO } else { base };
    let mut cur = read_inode(ext2, cur_ino)?;
    let mut holder = 0u32;

    loop {
        while pos < len && walk[pos] == b'/' {
            pos += 1;
        }
        if pos == len {
            return Ok(Found::Inode(cur_ino, cur, holder));
        }
        if !cur.is_dir() {
            return Err(ERR_NOT_DIR);
        }
        if !check_permission(&cur, uid, gid, 1) {
            return Err(ERR_PERMISSION);
        }
        let start = pos;
        while pos < len && walk[pos] != b'/' {
            pos += 1;
        }
        let name_len = pos - start;
        if name_len > MAX_NAME {
            return Err(ERR_NAME_TOO_LONG);
        }
        let last = walk[pos..len].iter().all(|&b| b == b'/');
        let slash_after = pos < len;

        let dev = dev_dir();
        let is_dot = matches!(&walk[start..pos], b"." | b"..");
        if dev != 0 && cur_ino == dev && !is_dot {
            // The devices are the whole of /dev, whatever the disk holds.
            return match devices::by_name(&walk[start..pos]) {
                Some(d) if last && !slash_after => Ok(Found::Device(d)),
                Some(_) => Err(ERR_NOT_DIR),
                None => Err(ERR_NOT_FOUND),
            };
        }

        let (child_ino, _) = find_entry(ext2, &cur, &walk[start..pos])?.ok_or(ERR_NOT_FOUND)?;
        let child = read_inode(ext2, child_ino)?;

        if child.is_symlink() && (!last || follow_last || slash_after) {
            links += 1;
            if links > MAX_LINKS {
                return Err(ERR_LOOP);
            }
            let target_len = crate::ext2_ops::read_link(ext2, &child)?;
            let target = crate::ext2_ops::link_target(target_len);
            if target.is_empty() {
                return Err(ERR_NOT_FOUND);
            }
            // The target takes the place of what named the link; what came
            // after it follows.
            let rest = len - pos;
            if target_len + rest > MAX_PATH {
                return Err(ERR_NAME_TOO_LONG);
            }
            walk.copy_within(pos..len, target_len);
            walk[..target_len].copy_from_slice(target);
            len = target_len + rest;
            pos = 0;
            if walk[0] == b'/' {
                cur_ino = EXT2_ROOT_INO;
                cur = read_inode(ext2, cur_ino)?;
                holder = 0;
            }
            continue;
        }

        if last {
            // A trailing slash on something that is not a directory is the
            // caller's to refuse; it knows whether it wanted one.
            return Ok(Found::Inode(child_ino, child, cur_ino));
        }
        holder = cur_ino;
        cur_ino = child_ino;
        cur = child;
    }
}

/// [`resolve`] for a caller that wants an inode: a device is not one it may
/// change.
pub fn resolve_inode(
    ext2: &Ext2State,
    base: u32,
    path: &[u8],
    uid: u32,
    gid: u32,
    follow_last: bool,
) -> Result<(u32, Ext2Inode, u32), u64> {
    match resolve(ext2, base, path, uid, gid, follow_last)? {
        Found::Inode(ino, inode, holder) => Ok((ino, inode, holder)),
        Found::Device(_) => Err(ERR_PERMISSION),
    }
}

// ---------------------------------------------------------------------------
// Read directory entries by index (for TAG_READDIR)
// ---------------------------------------------------------------------------

/// Directory entry info returned to the caller.
pub struct DirEntryInfo {
    pub name: [u8; 255],
    pub name_len: usize,
    pub inode_num: u32,
    pub file_type: u8,
    pub file_size: u32,
}

/// Read the `index`-th valid directory entry from a directory inode.
/// Skips `.` and `..` entries (optional — we include them for now).
/// Returns None when no more entries.
pub fn read_dir_entry(
    ext2: &Ext2State,
    dir_inode: &Ext2Inode,
    index: u32,
) -> Result<Option<DirEntryInfo>, u64> {
    let bs = ext2.block_size;
    let total_blocks = (dir_inode.i_size + bs - 1) / bs;
    let mut current_idx = 0u32;

    for logical in 0..total_blocks {
        let phys_block = block_map(ext2, dir_inode, logical)?;
        if phys_block == 0 {
            continue;
        }

        let block_buf = read_block_buf(ext2, phys_block)?;

        let mut pos = 0u32;
        while pos < bs {
            let off = pos as usize;
            let entry_inode = read_u32(&block_buf, off);
            let rec_len = read_u16(&block_buf, off + 4) as u32;
            let name_len = block_buf[off + 6] as usize;
            let file_type = block_buf[off + 7];

            if rec_len == 0 {
                break;
            }

            if entry_inode != 0 {
                if current_idx == index {
                    let mut name = [0u8; 255];
                    name[..name_len].copy_from_slice(&block_buf[off + 8..off + 8 + name_len]);

                    let file_size = read_inode(ext2, entry_inode)
                        .map(|ino| ino.i_size)
                        .unwrap_or(0);

                    return Ok(Some(DirEntryInfo {
                        name,
                        name_len,
                        inode_num: entry_inode,
                        file_type,
                        file_size,
                    }));
                }
                current_idx += 1;
            }

            pos += rec_len;
        }
    }

    Ok(None)
}

// ---------------------------------------------------------------------------
// Create directory entry
// ---------------------------------------------------------------------------

/// Add a new directory entry to a directory.
/// Finds space in existing entries or extends the directory with a new block.
pub fn create_dir_entry(
    ext2: &mut Ext2State,
    dir_inode_num: u32,
    dir_inode: &mut Ext2Inode,
    name: &[u8],
    new_inode: u32,
    file_type: u8,
) -> Result<(), u64> {
    let bs = ext2.block_size;
    // The last twelve bytes of every block may be a checksum tail. It is
    // disguised as an unused entry, so a scan that treats it as free space
    // will happily allocate over it — and the block's checksum with it.
    let usable = crate::csum::dir_usable_len(ext2, bs);
    let needed = align4(8 + name.len() as u32);
    let total_blocks = (dir_inode.i_size + bs - 1) / bs;
    drop_index(ext2, dir_inode_num, dir_inode)?;

    // Try to find space in existing blocks by splitting an entry with excess rec_len
    for logical in 0..total_blocks {
        let phys_block = block_map(ext2, dir_inode, logical)?;
        if phys_block == 0 {
            continue;
        }

        // Read the entire block into the static buffer
        read_block_buf_mut(ext2, phys_block)?;
        let block_buf = unsafe { &mut DIR_BLOCK_BUF };

        let mut pos = 0u32;
        while pos < usable {
            let off = pos as usize;
            let entry_inode = read_u32(block_buf, off);
            let rec_len = read_u16(block_buf, off + 4) as u32;
            let entry_name_len = block_buf[off + 6] as u32;

            if rec_len == 0 {
                break;
            }

            let actual_size = if entry_inode == 0 {
                0 // deleted entry — full rec_len is available
            } else {
                align4(8 + entry_name_len)
            };

            // An entry may claim the tail's bytes if the block has none, but
            // never past it if it has.
            let entry_end = (pos + rec_len).min(usable);
            let free_space = entry_end.saturating_sub(pos + actual_size);

            if free_space >= needed {
                if entry_inode != 0 {
                    // Shrink existing entry
                    write_u16(block_buf, off + 4, actual_size as u16);
                }

                // Write new entry at pos + actual_size
                let new_off = (pos + actual_size) as usize;
                let new_rec_len = entry_end - (pos + actual_size);
                write_u32(block_buf, new_off, new_inode);
                write_u16(block_buf, new_off + 4, new_rec_len as u16);
                block_buf[new_off + 6] = name.len() as u8;
                block_buf[new_off + 7] = file_type;
                block_buf[new_off + 8..new_off + 8 + name.len()].copy_from_slice(name);

                write_dir_block(ext2, phys_block, dir_inode_num, dir_inode, block_buf)?;
                return Ok(());
            }

            pos += rec_len;
        }
    }

    // No space found — extend the directory with a new block
    let new_block = crate::ext2_alloc::alloc_block(ext2).map_err(|_| ERR_IO)?;
    zero_block(ext2, new_block)?;

    let logical = total_blocks;
    crate::ext2::set_block_ptr(ext2, dir_inode, logical, new_block)?;
    dir_inode.i_blocks += ext2.block_size / 512;
    dir_inode.i_size += bs;

    // Write the new entry as the only entry in the new block (rec_len = block_size)
    let block_buf = unsafe { &mut DIR_BLOCK_BUF };
    block_buf.fill(0);
    write_u32(block_buf, 0, new_inode);
    write_u16(block_buf, 4, usable as u16);
    block_buf[6] = name.len() as u8;
    block_buf[7] = file_type;
    block_buf[8..8 + name.len()].copy_from_slice(name);
    if usable != bs {
        crate::csum::init_dirent_tail(&mut block_buf[..bs as usize]);
    }

    write_dir_block(ext2, new_block, dir_inode_num, dir_inode, block_buf)?;

    // Update directory inode on disk
    write_inode(ext2, dir_inode_num, dir_inode)?;

    Ok(())
}

/// Remove the entry called `name` from a directory.
///
/// An entry that follows another is folded into it; the first in a block
/// keeps its length and loses its inode, which is how ext2 marks one unused.
pub fn remove_entry(
    ext2: &Ext2State,
    dir_ino: u32,
    dir_inode: &mut Ext2Inode,
    name: &[u8],
) -> Result<(), u64> {
    drop_index(ext2, dir_ino, dir_inode)?;
    let bs = ext2.block_size;
    let usable = crate::csum::dir_usable_len(ext2, bs);
    let blocks = (dir_inode.i_size + bs - 1) / bs;
    for logical in 0..blocks {
        let phys = block_map(ext2, dir_inode, logical)?;
        if phys == 0 {
            continue;
        }
        read_block_buf_mut(ext2, phys)?;
        let buf = unsafe { &mut DIR_BLOCK_BUF };
        let mut pos = 0u32;
        let mut prev: Option<usize> = None;
        while pos < usable {
            let off = pos as usize;
            let ino = read_u32(buf, off);
            let rec_len = read_u16(buf, off + 4) as u32;
            let len = buf[off + 6] as usize;
            if rec_len == 0 {
                break;
            }
            if ino != 0 && len == name.len() && &buf[off + 8..off + 8 + len] == name {
                match prev {
                    Some(p) => {
                        let prev_len = read_u16(buf, p + 4) as u32;
                        write_u16(buf, p + 4, (prev_len + rec_len) as u16);
                    }
                    None => write_u32(buf, off, 0),
                }
                return write_dir_block(ext2, phys, dir_ino, dir_inode, buf);
            }
            prev = Some(off);
            pos += rec_len;
        }
    }
    Err(ERR_NOT_FOUND)
}

/// Whether a directory holds nothing but `.` and `..`.
pub fn is_empty(ext2: &Ext2State, dir_inode: &Ext2Inode) -> Result<bool, u64> {
    let mut empty = true;
    for_each_entry(ext2, dir_inode, |_, _, _, name| {
        if name != b"." && name != b".." {
            empty = false;
        }
        empty
    })?;
    Ok(empty)
}

/// Call `f` with each in-use entry of a directory, in order: its index, inode,
/// file type and name. Stops when `f` returns false.
pub fn for_each_entry(
    ext2: &Ext2State,
    dir_inode: &Ext2Inode,
    mut f: impl FnMut(u32, u32, u8, &[u8]) -> bool,
) -> Result<(), u64> {
    let bs = ext2.block_size;
    let blocks = (dir_inode.i_size + bs - 1) / bs;
    let mut index = 0u32;
    for logical in 0..blocks {
        let phys = block_map(ext2, dir_inode, logical)?;
        if phys == 0 {
            continue;
        }
        // A copy: `f` may read other blocks, and DIR_BLOCK_BUF is shared.
        let mut block = [0u8; 4096];
        block[..bs as usize].copy_from_slice(read_block_buf(ext2, phys)?);
        let mut pos = 0u32;
        while pos < bs {
            let off = pos as usize;
            let ino = read_u32(&block, off);
            let rec_len = read_u16(&block, off + 4) as u32;
            let len = block[off + 6] as usize;
            let kind = block[off + 7];
            if rec_len == 0 {
                break;
            }
            if ino != 0 {
                if !f(index, ino, kind, &block[off + 8..off + 8 + len]) {
                    return Ok(());
                }
                index += 1;
            }
            pos += rec_len;
        }
    }
    Ok(())
}

/// Point a directory's `..` at `parent`.
pub fn set_dotdot(ext2: &Ext2State, dir_ino: u32, dir_inode: &Ext2Inode, parent: u32) -> Result<(), u64> {
    let bs = ext2.block_size;
    let phys = block_map(ext2, dir_inode, 0)?;
    if phys == 0 {
        return Err(ERR_IO);
    }
    read_block_buf_mut(ext2, phys)?;
    let buf = unsafe { &mut DIR_BLOCK_BUF };
    let mut pos = 0u32;
    while pos < bs {
        let off = pos as usize;
        let rec_len = read_u16(buf, off + 4) as u32;
        if rec_len == 0 {
            break;
        }
        if read_u32(buf, off) != 0 && buf[off + 6] == 2 && &buf[off + 8..off + 10] == b".." {
            write_u32(buf, off, parent);
            return write_dir_block(ext2, phys, dir_ino, dir_inode, buf);
        }
        pos += rec_len;
    }
    Err(ERR_IO)
}

/// Initialize a new directory block with `.` and `..` entries.
pub fn init_dir_block(
    ext2: &mut Ext2State,
    block: u32,
    self_ino: u32,
    parent_ino: u32,
    generation: u32,
) -> Result<(), u64> {
    let bs = ext2.block_size;
    let usable = crate::csum::dir_usable_len(ext2, bs);
    let buf = unsafe { &mut DIR_BLOCK_BUF };
    buf.fill(0);

    // "." entry
    let dot_rec_len = 12u32; // align4(8 + 1) = 12
    write_u32(buf, 0, self_ino);
    write_u16(buf, 4, dot_rec_len as u16);
    buf[6] = 1; // name_len = 1
    buf[7] = FT_DIR;
    buf[8] = b'.';

    // ".." entry — takes the rest, short of the checksum tail if there is one.
    let dotdot_rec_len = usable - dot_rec_len;
    let off = dot_rec_len as usize;
    write_u32(buf, off, parent_ino);
    write_u16(buf, off + 4, dotdot_rec_len as u16);
    buf[off + 6] = 2; // name_len = 2
    buf[off + 7] = FT_DIR;
    buf[off + 8] = b'.';
    buf[off + 9] = b'.';

    if usable != bs {
        crate::csum::init_dirent_tail(&mut buf[..bs as usize]);
    }

    // The block belongs to the directory it describes, so its checksum is
    // seeded by that inode — which is `self_ino`, not the parent.
    let seed = crate::csum::inode_seed(ext2, self_ino, generation);
    crate::csum::set_dirblock(ext2, seed, &mut buf[..bs as usize]);

    for s in 0..ext2.sectors_per_block {
        let abs_lba = ext2.block_to_lba(block) + s;
        let o = (s * 512) as usize;
        let disk_buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        disk_buf.copy_from_slice(&buf[o..o + 512]);
        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Forget a directory's htree index before its entries change.
///
/// This server keeps no index, so an entry it adds or removes would leave the
/// index describing a directory that no longer exists. Without the flag every
/// reader, Linux's included, searches the entries themselves, and the blocks
/// that held the index read as empty entries.
pub fn drop_index(ext2: &Ext2State, dir_ino: u32, dir: &mut Ext2Inode) -> Result<(), u64> {
    if dir.i_flags & crate::ext2::EXT2_INDEX_FL != 0 {
        dir.i_flags &= !crate::ext2::EXT2_INDEX_FL;
        write_inode(ext2, dir_ino, dir)?;
    }
    Ok(())
}

/// Align a value up to a 4-byte boundary.
fn align4(val: u32) -> u32 {
    (val + 3) & !3
}

/// Static buffer for reading directory blocks (VFS is single-threaded).
static mut DIR_BLOCK_BUF: [u8; 4096] = [0u8; 4096];

/// Read an entire block into the static DIR_BLOCK_BUF.
/// Returns a reference to the filled portion (block_size bytes).
fn read_block_buf(ext2: &Ext2State, phys_block: u32) -> Result<&'static [u8], u64> {
    read_block_buf_mut(ext2, phys_block)?;
    Ok(unsafe { &DIR_BLOCK_BUF[..ext2.block_size as usize] })
}

/// Read an entire block into the static DIR_BLOCK_BUF (mutable access via unsafe).
fn read_block_buf_mut(ext2: &Ext2State, phys_block: u32) -> Result<(), u64> {
    let buf = unsafe { &mut DIR_BLOCK_BUF };
    ext2.prefetch_block(phys_block);
    for s in 0..ext2.sectors_per_block {
        let abs_lba = ext2.block_to_lba(phys_block) + s;
        let data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
        let off = (s * 512) as usize;
        buf[off..off + 512].copy_from_slice(data);
    }
    Ok(())
}

/// Write a block buffer back to disk, sector by sector.
/// Write a directory block back, checksumming it first.
///
/// The checksum is seeded by the directory's own inode, so a block cannot be
/// moved into another directory and still verify — which is why this needs to
/// know whose block it is rather than just where it goes.
fn write_dir_block(
    ext2: &Ext2State,
    block: u32,
    dir_inode_num: u32,
    dir_inode: &Ext2Inode,
    buf: &mut [u8],
) -> Result<(), u64> {
    let bs = ext2.block_size as usize;
    let seed = crate::csum::inode_seed(ext2, dir_inode_num, dir_inode.i_generation);
    crate::csum::set_dirblock(ext2, seed, &mut buf[..bs]);

    for s in 0..ext2.sectors_per_block {
        let abs_lba = ext2.block_to_lba(block) + s;
        let off = (s * 512) as usize;
        let disk_buf =
            unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        disk_buf.copy_from_slice(&buf[off..off + 512]);
        ext2.write_sector_abs(abs_lba).map_err(|_| ERR_IO)?;
    }
    Ok(())
}

/// Count valid entries in a directory (for TAG_READDIR_BULK).
pub fn count_dir_entries(ext2: &Ext2State, dir_inode: &Ext2Inode) -> Result<u32, u64> {
    let bs = ext2.block_size;
    let total_blocks = (dir_inode.i_size + bs - 1) / bs;
    let mut count = 0u32;

    for logical in 0..total_blocks {
        let phys_block = block_map(ext2, dir_inode, logical)?;
        if phys_block == 0 {
            continue;
        }

        let block_buf = read_block_buf(ext2, phys_block)?;

        let mut pos = 0u32;
        while pos < bs {
            let off = pos as usize;
            let entry_inode = read_u32(&block_buf, off);
            let rec_len = read_u16(&block_buf, off + 4) as u32;

            if rec_len == 0 {
                break;
            }
            if entry_inode != 0 {
                count += 1;
            }
            pos += rec_len;
        }
    }

    Ok(count)
}
