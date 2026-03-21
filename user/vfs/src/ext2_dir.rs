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
use crate::{
    read_u16, read_u32, DISK_IO_BUF, ERR_IO, ERR_NOT_DIR, ERR_NOT_FOUND, ERR_PERMISSION,
};
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

/// Resolve a path from the root directory, checking execute permission on each
/// intermediate directory.
/// Returns (inode_num, inode, parent_inode_num).
pub fn resolve_path(
    ext2: &Ext2State,
    path: &[u8],
    caller_uid: u32,
    caller_gid: u32,
) -> Result<(u32, Ext2Inode, u32), u64> {
    let path = if !path.is_empty() && path[0] == b'/' {
        &path[1..]
    } else {
        path
    };

    let root_inode = read_inode(ext2, EXT2_ROOT_INO)?;

    if path.is_empty() {
        return Ok((EXT2_ROOT_INO, root_inode, 0));
    }

    let mut current_ino = EXT2_ROOT_INO;
    let mut current_inode = root_inode;
    let mut parent_ino = 0u32;

    let mut remaining = path;
    loop {
        // Check execute permission on current directory for traversal
        if !check_permission(&current_inode, caller_uid, caller_gid, 1) {
            return Err(ERR_PERMISSION);
        }

        // Find next component
        let (component, rest) = match remaining.iter().position(|&b| b == b'/') {
            Some(pos) => (&remaining[..pos], &remaining[pos + 1..]),
            None => (remaining, &[] as &[u8]),
        };

        if component.is_empty() {
            remaining = rest;
            if remaining.is_empty() {
                return Ok((current_ino, current_inode, parent_ino));
            }
            continue;
        }

        let is_last = rest.is_empty();

        match find_entry(ext2, &current_inode, component)? {
            Some((child_ino, _file_type)) => {
                let child_inode = read_inode(ext2, child_ino)?;
                if is_last {
                    return Ok((child_ino, child_inode, current_ino));
                }
                // Intermediate component must be a directory
                if !child_inode.is_dir() {
                    return Err(ERR_NOT_DIR);
                }
                parent_ino = current_ino;
                current_ino = child_ino;
                current_inode = child_inode;
                remaining = rest;
            }
            None => return Err(ERR_NOT_FOUND),
        }
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
    let needed = align4(8 + name.len() as u32);
    let total_blocks = (dir_inode.i_size + bs - 1) / bs;

    // Try to find space in existing blocks by splitting an entry with excess rec_len
    for logical in 0..total_blocks {
        let phys_block = block_map(ext2, dir_inode, logical)?;
        if phys_block == 0 {
            continue;
        }

        // Read the entire block into a local buffer (up to 4096 bytes)
        let mut block_buf = [0u8; 4096];
        for s in 0..ext2.sectors_per_block {
            let abs_lba = ext2.block_to_lba(phys_block) + s;
            let data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
            let off = (s * 512) as usize;
            block_buf[off..off + 512].copy_from_slice(data);
        }

        let mut pos = 0u32;
        while pos < bs {
            let off = pos as usize;
            let entry_inode = read_u32(&block_buf, off);
            let rec_len = read_u16(&block_buf, off + 4) as u32;
            let entry_name_len = block_buf[off + 6] as u32;

            if rec_len == 0 {
                break;
            }

            let actual_size = if entry_inode == 0 {
                0 // deleted entry — full rec_len is available
            } else {
                align4(8 + entry_name_len)
            };

            let free_space = rec_len - actual_size;

            if free_space >= needed {
                if entry_inode != 0 {
                    // Shrink existing entry
                    write_u16(&mut block_buf, off + 4, actual_size as u16);
                }

                // Write new entry at pos + actual_size
                let new_off = (pos + actual_size) as usize;
                let new_rec_len = rec_len - actual_size;
                write_u32(&mut block_buf, new_off, new_inode);
                write_u16(&mut block_buf, new_off + 4, new_rec_len as u16);
                block_buf[new_off + 6] = name.len() as u8;
                block_buf[new_off + 7] = file_type;
                block_buf[new_off + 8..new_off + 8 + name.len()].copy_from_slice(name);

                // Write block back to disk
                write_block_buf(ext2, phys_block, &block_buf)?;
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
    let mut block_buf = [0u8; 4096];
    write_u32(&mut block_buf, 0, new_inode);
    write_u16(&mut block_buf, 4, bs as u16);
    block_buf[6] = name.len() as u8;
    block_buf[7] = file_type;
    block_buf[8..8 + name.len()].copy_from_slice(name);

    write_block_buf(ext2, new_block, &block_buf)?;

    // Update directory inode on disk
    write_inode(ext2, dir_inode_num, dir_inode)?;

    Ok(())
}

/// Initialize a new directory block with `.` and `..` entries.
pub fn init_dir_block(
    ext2: &mut Ext2State,
    block: u32,
    self_ino: u32,
    parent_ino: u32,
) -> Result<(), u64> {
    let bs = ext2.block_size;
    let mut buf = [0u8; 4096];

    // "." entry
    let dot_rec_len = 12u32; // align4(8 + 1) = 12
    write_u32(&mut buf, 0, self_ino);
    write_u16(&mut buf, 4, dot_rec_len as u16);
    buf[6] = 1; // name_len = 1
    buf[7] = FT_DIR;
    buf[8] = b'.';

    // ".." entry — takes remaining space in block
    let dotdot_rec_len = bs - dot_rec_len;
    let off = dot_rec_len as usize;
    write_u32(&mut buf, off, parent_ino);
    write_u16(&mut buf, off + 4, dotdot_rec_len as u16);
    buf[off + 6] = 2; // name_len = 2
    buf[off + 7] = FT_DIR;
    buf[off + 8] = b'.';
    buf[off + 9] = b'.';

    write_block_buf(ext2, block, &buf)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Align a value up to a 4-byte boundary.
fn align4(val: u32) -> u32 {
    (val + 3) & !3
}

/// Read an entire block into a local [u8; 4096] buffer.
fn read_block_buf(ext2: &Ext2State, phys_block: u32) -> Result<[u8; 4096], u64> {
    let mut buf = [0u8; 4096];
    ext2.prefetch_block(phys_block);
    for s in 0..ext2.sectors_per_block {
        let abs_lba = ext2.block_to_lba(phys_block) + s;
        let data = ext2.cached_read_sector(abs_lba).map_err(|_| ERR_IO)?;
        let off = (s * 512) as usize;
        buf[off..off + 512].copy_from_slice(data);
    }
    Ok(buf)
}

/// Write a block buffer back to disk, sector by sector.
fn write_block_buf(ext2: &Ext2State, block: u32, buf: &[u8]) -> Result<(), u64> {
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
