//! Changing the namespace of an ext2 or ext4 filesystem: making files and
//! directories.
//!
//! Every function here is one step of a request the caller has already wrapped
//! in a transaction, and checks the caller's permission on each directory it
//! changes: write and search, as POSIX has it.

use crate::ext2::{self, Ext2Inode, Ext2State};
use crate::{ext2_alloc, ext2_dir, ext4};
use crate::{ERR_EXISTS, ERR_INVALID_PATH, ERR_NAME_TOO_LONG, ERR_NOT_DIR, ERR_PERMISSION};
use crate::protocol::MAX_NAME;

/// Split a path into its parent directory and its last name.
///
/// Trailing slashes are dropped. An empty name, `.`, `..` and a name longer
/// than an ext2 entry can hold are refused: none of them can be made.
pub fn split_path(path: &[u8]) -> Result<(&[u8], &[u8]), u64> {
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    let path = &path[..end];
    let (parent, name) = match path.iter().rposition(|&b| b == b'/') {
        Some(0) => (&path[..1], &path[1..]),
        Some(pos) => (&path[..pos], &path[pos + 1..]),
        None => (&b"/"[..], path),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(ERR_INVALID_PATH);
    }
    if name.len() > MAX_NAME {
        return Err(ERR_NAME_TOO_LONG);
    }
    Ok((parent, name))
}

/// Make `path` a new file or directory owned by `uid`/`gid`, and return its
/// inode. The name must be free.
pub fn create(
    e2: &mut Ext2State,
    path: &[u8],
    uid: u32,
    gid: u32,
    is_dir: bool,
) -> Result<(u32, Ext2Inode), u64> {
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent, _) = ext2_dir::resolve_path(e2, parent_path, uid, gid)?;
    if !parent.is_dir() {
        return Err(ERR_NOT_DIR);
    }
    if !ext2::check_permission(&parent, uid, gid, 3) {
        return Err(ERR_PERMISSION);
    }
    if ext2_dir::find_entry(e2, &parent, name)?.is_some() {
        return Err(ERR_EXISTS);
    }

    // The number comes back but the bytes are still the last file's, and
    // write_inode overlays rather than overwrites, so wipe it before anything
    // reads a generation or a checksum out of it.
    let ino = ext2_alloc::alloc_inode(e2)?;
    ext2::zero_inode(e2, ino)?;

    let t = ext2::now();
    let mut inode = Ext2Inode::empty();
    inode.i_mode = if is_dir { ext2::S_IFDIR | 0o755 } else { ext2::S_IFREG | 0o644 };
    inode.i_uid = uid as u16;
    inode.i_gid = gid as u16;
    inode.i_links_count = if is_dir { 2 } else { 1 };
    inode.i_atime = t;
    inode.i_ctime = t;
    inode.i_mtime = t;
    // On a volume with extents the pointer array is not an alternative: a
    // reader takes i_block as an extent header whatever is there.
    if e2.is_ext4() {
        ext4::init_extent_root(&mut inode);
    }

    if is_dir {
        let block = ext2_alloc::alloc_block(e2)?;
        if e2.is_ext4() {
            ext4::extent_insert(&mut inode, 0, block)?;
        } else {
            inode.i_block[0] = block;
        }
        inode.i_size = e2.block_size;
        inode.i_blocks = e2.block_size / 512;
        ext2_dir::init_dir_block(e2, block, ino, parent_ino, inode.i_generation)?;
        // The new directory's ".." is a link to its parent.
        parent.i_links_count += 1;
        let group = (ino - 1) / e2.inodes_per_group;
        e2.bgd_table[group as usize].bg_used_dirs_count += 1;
        ext2::flush_bgd(e2, group)?;
    }

    ext2::write_inode(e2, ino, &inode)?;
    let kind = if is_dir { ext2::FT_DIR } else { ext2::FT_REG_FILE };
    ext2_dir::create_dir_entry(e2, parent_ino, &mut parent, name, ino, kind)?;
    parent.i_mtime = t;
    parent.i_ctime = t;
    ext2::write_inode(e2, parent_ino, &parent)?;
    Ok((ino, inode))
}
