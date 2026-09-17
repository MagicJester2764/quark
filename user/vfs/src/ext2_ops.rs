//! Changing the namespace of an ext2 or ext4 filesystem: making, removing,
//! renaming and shortening files and directories.
//!
//! Every function here is one step of a request the caller has already wrapped
//! in a transaction, and checks the caller's permission on each directory it
//! changes: write and search, as POSIX has it.
//!
//! A file whose last name goes while a handle still names it is not freed:
//! it becomes an orphan, and goes when its last handle closes.

use crate::ext2::{self, Ext2Inode, Ext2State};
use crate::protocol::MAX_NAME;
use crate::{ext2_alloc, ext2_dir, ext4, handles};
use crate::{
    DISK_IO_BUF, ERR_EXISTS, ERR_INVALID_PATH, ERR_IO, ERR_IS_DIR, ERR_NAME_TOO_LONG,
    ERR_NOT_DIR, ERR_NOT_EMPTY, ERR_NOT_FOUND, ERR_NOT_SUPPORTED, ERR_PERMISSION,
    ERR_TOO_MANY_LINKS,
};

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
        // A bare name is in the directory the request starts from.
        None => (&b"."[..], path),
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
    base: u32,
    path: &[u8],
    uid: u32,
    gid: u32,
    is_dir: bool,
) -> Result<(u32, Ext2Inode), u64> {
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent) = writable_dir(e2, base, parent_path, uid, gid)?;
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

/// A directory the caller may change: it exists, is a directory, and the
/// caller may write and search it. `/dev` is never one: its names are the
/// server's devices, and a name made there on the disk would be hidden.
fn writable_dir(
    e2: &Ext2State,
    base: u32,
    path: &[u8],
    uid: u32,
    gid: u32,
) -> Result<(u32, Ext2Inode), u64> {
    let (ino, dir, _) = ext2_dir::resolve_inode(e2, base, path, uid, gid, true)?;
    if !dir.is_dir() {
        return Err(ERR_NOT_DIR);
    }
    if ino == ext2_dir::dev_dir() || !ext2::check_permission(&dir, uid, gid, 3) {
        return Err(ERR_PERMISSION);
    }
    Ok((ino, dir))
}

/// Remove `path`'s name. The file goes with its last name, unless a handle
/// still names it.
pub fn unlink(e2: &mut Ext2State, base: u32, path: &[u8], uid: u32, gid: u32) -> Result<(), u64> {
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent) = writable_dir(e2, base, parent_path, uid, gid)?;
    let (ino, _) = ext2_dir::find_entry(e2, &parent, name)?.ok_or(ERR_NOT_FOUND)?;
    let mut inode = ext2::read_inode(e2, ino)?;
    if inode.is_dir() {
        return Err(ERR_IS_DIR);
    }
    ext2_dir::remove_entry(e2, parent_ino, &mut parent, name)?;
    let t = ext2::now();
    parent.i_mtime = t;
    parent.i_ctime = t;
    ext2::write_inode(e2, parent_ino, &parent)?;
    drop_link(e2, ino, &mut inode, t)
}

/// Remove the empty directory `path`.
pub fn rmdir(e2: &mut Ext2State, base: u32, path: &[u8], uid: u32, gid: u32) -> Result<(), u64> {
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent) = writable_dir(e2, base, parent_path, uid, gid)?;
    let (ino, _) = ext2_dir::find_entry(e2, &parent, name)?.ok_or(ERR_NOT_FOUND)?;
    let mut dir = ext2::read_inode(e2, ino)?;
    if !dir.is_dir() {
        return Err(ERR_NOT_DIR);
    }
    if !ext2_dir::is_empty(e2, &dir)? {
        return Err(ERR_NOT_EMPTY);
    }
    ext2_dir::remove_entry(e2, parent_ino, &mut parent, name)?;
    let t = ext2::now();
    // The directory's ".." was a link to its parent.
    parent.i_links_count = parent.i_links_count.saturating_sub(1);
    parent.i_mtime = t;
    parent.i_ctime = t;
    ext2::write_inode(e2, parent_ino, &parent)?;
    drop_dir(e2, ino, &mut dir, t)
}

/// Give the file at `from` the name `to`, replacing what had it.
pub fn rename(
    e2: &mut Ext2State,
    from_base: u32,
    from: &[u8],
    to_base: u32,
    to: &[u8],
    uid: u32,
    gid: u32,
) -> Result<(), u64> {
    let (from_parent, from_name) = split_path(from)?;
    let (to_parent, to_name) = split_path(to)?;
    let (fpi, fparent) = writable_dir(e2, from_base, from_parent, uid, gid)?;
    let (tpi, tparent) = writable_dir(e2, to_base, to_parent, uid, gid)?;
    let (ino, kind) = ext2_dir::find_entry(e2, &fparent, from_name)?.ok_or(ERR_NOT_FOUND)?;
    let mut inode = ext2::read_inode(e2, ino)?;
    let is_dir = inode.is_dir();
    // A directory cannot be moved inside itself.
    if is_dir && within(e2, tpi, ino)? {
        return Err(ERR_INVALID_PATH);
    }
    let t = ext2::now();

    if let Some((existing, _)) = ext2_dir::find_entry(e2, &tparent, to_name)? {
        if existing == ino {
            return Ok(()); // two names for one file: POSIX says do nothing
        }
        let mut victim = ext2::read_inode(e2, existing)?;
        if is_dir {
            if !victim.is_dir() {
                return Err(ERR_NOT_DIR);
            }
            if !ext2_dir::is_empty(e2, &victim)? {
                return Err(ERR_NOT_EMPTY);
            }
        } else if victim.is_dir() {
            return Err(ERR_IS_DIR);
        }
        let mut tp = ext2::read_inode(e2, tpi)?;
        ext2_dir::remove_entry(e2, tpi, &mut tp, to_name)?;
        if victim.is_dir() {
            tp.i_links_count = tp.i_links_count.saturating_sub(1);
            ext2::write_inode(e2, tpi, &tp)?;
            drop_dir(e2, existing, &mut victim, t)?;
        } else {
            ext2::write_inode(e2, tpi, &tp)?;
            drop_link(e2, existing, &mut victim, t)?;
        }
    }

    // The new name first, so that a failure part way leaves the file with two
    // names rather than none. Each parent is read afresh before it changes:
    // they may be the same directory, and each step writes it.
    let mut tp = ext2::read_inode(e2, tpi)?;
    ext2_dir::create_dir_entry(e2, tpi, &mut tp, to_name, ino, kind)?;
    if is_dir && fpi != tpi {
        tp.i_links_count += 1;
    }
    tp.i_mtime = t;
    tp.i_ctime = t;
    ext2::write_inode(e2, tpi, &tp)?;

    let mut fp = ext2::read_inode(e2, fpi)?;
    ext2_dir::remove_entry(e2, fpi, &mut fp, from_name)?;
    if is_dir && fpi != tpi {
        fp.i_links_count = fp.i_links_count.saturating_sub(1);
    }
    fp.i_mtime = t;
    fp.i_ctime = t;
    ext2::write_inode(e2, fpi, &fp)?;

    if is_dir && fpi != tpi {
        ext2_dir::set_dotdot(e2, ino, &inode, tpi)?;
    }
    inode.i_ctime = t;
    ext2::write_inode(e2, ino, &inode)
}

/// Give the file at `from` a second name, `to`.
///
/// Directories have exactly one name, so they are refused. The new name's
/// directory must be one the caller may change, as for `create`.
pub fn link(
    e2: &mut Ext2State,
    from_base: u32,
    from: &[u8],
    to_base: u32,
    to: &[u8],
    uid: u32,
    gid: u32,
    follow: bool,
) -> Result<(), u64> {
    let (ino, mut inode, _) = ext2_dir::resolve_inode(e2, from_base, from, uid, gid, follow)?;
    if inode.is_dir() {
        return Err(ERR_IS_DIR);
    }
    // What e2fsck accepts: ext2's own limit, and ext4's.
    let most = if e2.is_ext4() { 65000 } else { 32000 };
    if inode.i_links_count >= most {
        return Err(ERR_TOO_MANY_LINKS);
    }
    let (to_parent, to_name) = split_path(to)?;
    let (tpi, mut tp) = writable_dir(e2, to_base, to_parent, uid, gid)?;
    if ext2_dir::find_entry(e2, &tp, to_name)?.is_some() {
        return Err(ERR_EXISTS);
    }
    let t = ext2::now();
    // The count first: a failure after it leaves a file with one name too
    // few, which e2fsck mends, rather than a name the count does not know.
    inode.i_links_count += 1;
    inode.i_ctime = t;
    ext2::write_inode(e2, ino, &inode)?;
    ext2_dir::create_dir_entry(e2, tpi, &mut tp, to_name, ino, ext2::file_type_of(&inode))?;
    tp.i_mtime = t;
    tp.i_ctime = t;
    ext2::write_inode(e2, tpi, &tp)
}

/// Where [`read_link`] leaves a link's target.
static mut LINK_BUF: [u8; 4096] = [0; 4096];

/// The target [`read_link`] just read, `len` bytes long.
pub fn link_target(len: usize) -> &'static [u8] {
    let buf: &'static [u8; 4096] = unsafe { &*core::ptr::addr_of!(LINK_BUF) };
    &buf[..len.min(4096)]
}

/// Read symbolic link `inode`'s target into [`link_target`], and say how long
/// it is.
pub fn read_link(e2: &Ext2State, inode: &Ext2Inode) -> Result<usize, u64> {
    if !inode.is_symlink() {
        return Err(ERR_INVALID_PATH);
    }
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LINK_BUF) };
    let len = inode.i_size as usize;
    if inode.is_fast_symlink() {
        for (j, word) in inode.i_block.iter().enumerate() {
            buf[j * 4..j * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        return Ok(len);
    }
    // A slow link is one block, with the target and a NUL: e2fsck holds a
    // target that fills its block to be broken, and so does this.
    let bs = e2.block_size as usize;
    if len >= bs {
        return Err(ERR_IO);
    }
    let phys = ext2::block_map(e2, inode, 0)?;
    if phys == 0 {
        return Err(ERR_IO);
    }
    let mut done = 0;
    for s in 0..e2.sectors_per_block {
        if done >= len {
            break;
        }
        let data = e2.cached_read_sector(e2.block_to_lba(phys) + s).map_err(|_| ERR_IO)?;
        let n = (len - done).min(512);
        buf[done..done + n].copy_from_slice(&data[..n]);
        done += n;
    }
    Ok(len)
}

/// Make `path` a symbolic link to `target`, which is stored as given.
///
/// A target shorter than 60 bytes is kept in the inode (a fast link, with no
/// extent root even on ext4); a longer one takes a block, written the way a
/// file's data is, and must leave room in it for a NUL.
pub fn symlink(
    e2: &mut Ext2State,
    target: &[u8],
    base: u32,
    path: &[u8],
    uid: u32,
    gid: u32,
) -> Result<(), u64> {
    if target.is_empty() {
        return Err(ERR_NOT_FOUND);
    }
    if target.len() >= e2.block_size as usize {
        return Err(ERR_NAME_TOO_LONG);
    }
    let (parent_path, name) = split_path(path)?;
    let (parent_ino, mut parent) = writable_dir(e2, base, parent_path, uid, gid)?;
    if ext2_dir::find_entry(e2, &parent, name)?.is_some() {
        return Err(ERR_EXISTS);
    }

    let ino = ext2_alloc::alloc_inode(e2)?;
    ext2::zero_inode(e2, ino)?;
    let t = ext2::now();
    let mut inode = Ext2Inode::empty();
    inode.i_mode = ext2::S_IFLNK | 0o777;
    inode.i_uid = uid as u16;
    inode.i_gid = gid as u16;
    inode.i_links_count = 1;
    inode.i_atime = t;
    inode.i_ctime = t;
    inode.i_mtime = t;

    let made = if target.len() < 60 {
        let mut raw = [0u8; 60];
        raw[..target.len()].copy_from_slice(target);
        for (j, word) in inode.i_block.iter_mut().enumerate() {
            *word = u32::from_le_bytes([raw[j * 4], raw[j * 4 + 1], raw[j * 4 + 2], raw[j * 4 + 3]]);
        }
        inode.i_size = target.len() as u32;
        ext2::write_inode(e2, ino, &inode)
    } else {
        if e2.is_ext4() {
            ext4::init_extent_root(&mut inode);
        }
        let data = unsafe {
            core::slice::from_raw_parts_mut(crate::CLIENT_BUF as *mut u8, target.len())
        };
        data.copy_from_slice(target);
        ext2::write_inode(e2, ino, &inode)
            .and_then(|()| ext2::write_file_data(e2, &mut inode, ino, 0, target.len() as u32))
            .map(|_| ())
    }
    .and_then(|()| ext2_dir::create_dir_entry(e2, parent_ino, &mut parent, name, ino, ext2::FT_SYMLINK));
    if let Err(code) = made {
        // Nothing names it: give back what it took.
        inode.i_links_count = 0;
        let _ = release_inode(e2, ino, &mut inode, t);
        return Err(code);
    }
    parent.i_mtime = t;
    parent.i_ctime = t;
    ext2::write_inode(e2, parent_ino, &parent)
}

/// Whether directory `dir` is `ancestor` or somewhere beneath it.
fn within(e2: &Ext2State, mut dir: u32, ancestor: u32) -> Result<bool, u64> {
    for _ in 0..256 {
        if dir == ancestor {
            return Ok(true);
        }
        if dir == ext2::EXT2_ROOT_INO {
            return Ok(false);
        }
        let inode = ext2::read_inode(e2, dir)?;
        match ext2_dir::find_entry(e2, &inode, b"..")? {
            Some((parent, _)) if parent != dir => dir = parent,
            _ => return Ok(false),
        }
    }
    Err(ERR_IO)
}

/// Make regular file `ino` `size` bytes long.
pub fn truncate(e2: &mut Ext2State, ino: u32, size: u64) -> Result<(), u64> {
    let mut inode = ext2::read_inode(e2, ino)?;
    if inode.is_dir() {
        return Err(ERR_IS_DIR);
    }
    // File sizes here are 32-bit.
    if !inode.is_regular() || size > u32::MAX as u64 {
        return Err(ERR_NOT_SUPPORTED);
    }
    let bs = e2.block_size as u64;
    if size < inode.size64() {
        let keep = ((size + bs - 1) / bs) as u32;
        free_blocks_from(e2, &mut inode, keep)?;
        // The kept block's tail must read as zeros if the file grows again.
        if size % bs != 0 {
            let phys = ext2::block_map(e2, &inode, (size / bs) as u32)?;
            if phys != 0 {
                zero_tail(e2, phys, (size % bs) as u32)?;
            }
        }
    }
    // Growing only moves the size: the blocks in between are holes, which
    // read as zeros and are filled when written.
    inode.i_size = size as u32;
    inode.i_size_high = 0;
    let t = ext2::now();
    inode.i_mtime = t;
    inode.i_ctime = t;
    ext2::write_inode(e2, ino, &inode)
}

/// Zero `block` from byte `from` to its end.
fn zero_tail(e2: &Ext2State, block: u32, from: u32) -> Result<(), u64> {
    let first = from / 512;
    for sector in first..e2.sectors_per_block {
        let lba = e2.block_to_lba(block) + sector;
        ext2::raw_read_sector(e2.disk_tid, lba).map_err(|_| ERR_IO)?;
        let buf = unsafe { core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, 512) };
        let start = if sector == first { (from % 512) as usize } else { 0 };
        buf[start..].fill(0);
        e2.write_sector_abs(lba).map_err(|_| ERR_IO)?;
    }
    Ok(())
}

/// One name fewer for file `ino`. With none left it is freed — or, while a
/// handle still names it, left for that handle's close to free.
fn drop_link(e2: &mut Ext2State, ino: u32, inode: &mut Ext2Inode, t: u32) -> Result<(), u64> {
    inode.i_links_count = inode.i_links_count.saturating_sub(1);
    inode.i_ctime = t;
    if inode.i_links_count > 0 {
        return ext2::write_inode(e2, ino, inode);
    }
    if handles::inode_is_open(ino) {
        handles::add_orphan(ino);
        return ext2::write_inode(e2, ino, inode);
    }
    release_inode(e2, ino, inode, t)
}

/// Directory `ino` has lost its only name.
fn drop_dir(e2: &mut Ext2State, ino: u32, dir: &mut Ext2Inode, t: u32) -> Result<(), u64> {
    dir.i_links_count = 0;
    dir.i_ctime = t;
    let group = (ino - 1) / e2.inodes_per_group;
    let used = &mut e2.bgd_table[group as usize].bg_used_dirs_count;
    *used = used.saturating_sub(1);
    ext2::flush_bgd(e2, group)?;
    if handles::inode_is_open(ino) {
        handles::add_orphan(ino);
        return ext2::write_inode(e2, ino, dir);
    }
    release_inode(e2, ino, dir, t)
}

/// Free inode `ino`, which has no names left, if it still has none.
pub fn release(e2: &mut Ext2State, ino: u32) -> Result<(), u64> {
    let mut inode = ext2::read_inode(e2, ino)?;
    if inode.i_links_count != 0 {
        return Ok(());
    }
    release_inode(e2, ino, &mut inode, ext2::now())
}

/// Free everything `ino` holds, and `ino`.
fn release_inode(e2: &mut Ext2State, ino: u32, inode: &mut Ext2Inode, t: u32) -> Result<(), u64> {
    if inode.is_fast_symlink() {
        // Its i_block is the target's text, not a map of blocks.
        inode.i_block = [0; 15];
    } else {
        free_blocks_from(e2, inode, 0)?;
    }
    inode.i_size = 0;
    inode.i_size_high = 0;
    // A deletion time below the inode count is how ext4's orphan list links
    // inodes, and fsck reads it so. A machine with no clock would write one.
    inode.i_dtime = t.max(e2.total_inodes);
    ext2::write_inode(e2, ino, inode)?;
    ext2_alloc::free_inode(e2, ino)
}

/// Free every block of `inode` from logical block `first` on, and whatever
/// indirect or tree blocks then map nothing. `i_blocks` follows.
pub fn free_blocks_from(e2: &mut Ext2State, inode: &mut Ext2Inode, first: u32) -> Result<(), u64> {
    let unit = e2.block_size / 512;
    if ext4::uses_extents(inode) {
        let freed = if first == 0 {
            ext4::free_tree(e2, inode)?
        } else {
            ext4::truncate_root(e2, inode, first)?
        };
        inode.i_blocks = inode.i_blocks.saturating_sub(freed * unit);
        return Ok(());
    }
    for l in (first as usize).min(12)..12 {
        let b = inode.i_block[l];
        if b != 0 {
            ext2_alloc::free_block(e2, b)?;
            inode.i_block[l] = 0;
            inode.i_blocks = inode.i_blocks.saturating_sub(unit);
        }
    }
    let ppb = e2.ptrs_per_block() as u64;
    let mut base = 12u64;
    let mut covers = ppb; // data blocks the single-indirect tree maps
    for level in 1..=3u32 {
        let slot = 11 + level as usize;
        let top = inode.i_block[slot];
        if top != 0 && (first as u64) < base + covers {
            let rel = (first as u64).saturating_sub(base);
            let (freed, empty) = free_indirect(e2, top, level, rel)?;
            inode.i_blocks = inode.i_blocks.saturating_sub(freed * unit);
            if empty {
                ext2_alloc::free_block(e2, top)?;
                inode.i_block[slot] = 0;
                inode.i_blocks = inode.i_blocks.saturating_sub(unit);
            }
        }
        base += covers;
        covers *= ppb;
    }
    Ok(())
}

/// Free what indirect block `block` maps from its `first`-th data block on.
/// Level 1 maps data blocks; each level above maps blocks of the one below.
/// Returns how many blocks went, and whether `block` now maps nothing.
fn free_indirect(e2: &mut Ext2State, block: u32, level: u32, first: u64) -> Result<(u32, bool), u64> {
    let ppb = e2.ptrs_per_block();
    let span = (ppb as u64).pow(level - 1);
    let mut freed = 0;
    let mut empty = true;
    for i in 0..ppb {
        let ptr = ext2::read_block_ptr(e2, block, i)?;
        if ptr == 0 {
            continue;
        }
        let start = i as u64 * span;
        if start + span <= first {
            empty = false;
            continue;
        }
        let gone = if level == 1 {
            ext2_alloc::free_block(e2, ptr)?;
            freed += 1;
            true
        } else {
            let (f, child_empty) = free_indirect(e2, ptr, level - 1, first.saturating_sub(start))?;
            freed += f;
            if child_empty {
                ext2_alloc::free_block(e2, ptr)?;
                freed += 1;
            }
            child_empty
        };
        if gone {
            // A block being freed whole need not be tidied first.
            if first > 0 {
                ext2::write_block_ptr(e2, block, i, 0)?;
            }
        } else {
            empty = false;
        }
    }
    Ok((freed, empty))
}
