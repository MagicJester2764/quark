//! The open-file table.
//!
//! A handle names a file, not a copy of it: an ext2 handle holds the inode
//! number and the inode is read when the handle is used. Two handles on one
//! file therefore never disagree about its size or where its blocks are, and
//! a file shortened through one cannot be written past its end through a
//! stale block map in the other.
//!
//! A handle belongs to the program that opened it — every thread of it may use
//! it — and goes when the program does. Programs are named by their address
//! space's id, which the kernel never reuses; a TID would have made a file one
//! thread opened useless to its siblings, and, being recycled, would have
//! handed a dead task's files to whatever took its slot. The server watches
//! every program it gives a handle to.

use quark_rt::syscall;

/// Handles for the whole system.
pub const MAX_OPEN_FILES: usize = 128;

pub enum FsFileData {
    Fat32 {
        first_cluster: u32,
        cur_cluster: u32,
        cur_cluster_offset: u32,
        dir_cluster: u32,
        fat_name: [u8; 11],
    },
    Ext2 {
        inode_num: u32,
    },
    None,
}

pub struct OpenFile {
    pub in_use: bool,
    /// The owning program's space id.
    pub owner: u64,
    /// FAT32's size, which lives in its directory entry. An ext2 handle reads
    /// the inode instead.
    pub file_size: u32,
    pub is_dir: bool,
    pub writable: bool,
    pub read_offset: u32,
    pub fs: FsFileData,
}

impl OpenFile {
    pub const fn empty() -> Self {
        OpenFile {
            in_use: false,
            owner: 0,
            file_size: 0,
            is_dir: false,
            writable: false,
            read_offset: 0,
            fs: FsFileData::None,
        }
    }

    /// The inode an ext2 handle names, or 0.
    pub fn inode_num(&self) -> u32 {
        match self.fs {
            FsFileData::Ext2 { inode_num } => inode_num,
            _ => 0,
        }
    }
}

static mut TABLE: [OpenFile; MAX_OPEN_FILES] = {
    const EMPTY: OpenFile = OpenFile::empty();
    [EMPTY; MAX_OPEN_FILES]
};

fn table() -> &'static mut [OpenFile; MAX_OPEN_FILES] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Put `file` in the table and return its handle, or None if it is full.
pub fn alloc(file: OpenFile) -> Option<usize> {
    let owner = file.owner;
    if owner == 0 {
        return None;
    }
    let t = table();
    let i = t.iter().position(|f| !f.in_use)?;
    t[i] = file;
    t[i].in_use = true;
    // Told when the program is gone, so its handles go with it. Watching a
    // program twice is the same as watching it once, and a program already
    // gone cannot be calling.
    let _ = syscall::sys_space_watch(owner);
    Some(i)
}

/// Program `space`'s handle `handle`, if it is one.
pub fn get(handle: usize, space: u64) -> Option<&'static mut OpenFile> {
    let f = table().get_mut(handle)?;
    if f.in_use && space != 0 && f.owner == space { Some(f) } else { None }
}

/// Close program `space`'s handle `handle`. Returns the inode it named (0 for
/// FAT32), or None if it was not one of that program's.
pub fn close(handle: usize, space: u64) -> Option<u32> {
    let f = get(handle, space)?;
    let ino = f.inode_num();
    *f = OpenFile::empty();
    Some(ino)
}

/// Close every handle program `space` held. The inodes they named are written
/// to `closed`, and their number returned.
pub fn close_all(space: u64, closed: &mut [u32; MAX_OPEN_FILES]) -> usize {
    let mut n = 0;
    for f in table().iter_mut() {
        if f.in_use && f.owner == space {
            closed[n] = f.inode_num();
            n += 1;
            *f = OpenFile::empty();
        }
    }
    n
}

/// Whether any handle still names inode `ino`.
pub fn inode_is_open(ino: u32) -> bool {
    ino != 0 && table().iter().any(|f| f.in_use && f.inode_num() == ino)
}

/// Inodes whose last name went while a handle still named them. Each has a
/// handle, so there can never be more than the table holds.
static mut ORPHANS: [u32; MAX_OPEN_FILES] = [0; MAX_OPEN_FILES];

fn orphans() -> &'static mut [u32; MAX_OPEN_FILES] {
    unsafe { &mut *core::ptr::addr_of_mut!(ORPHANS) }
}

pub fn add_orphan(ino: u32) {
    if is_orphan(ino) {
        return;
    }
    if let Some(slot) = orphans().iter_mut().find(|o| **o == 0) {
        *slot = ino;
    }
}

pub fn is_orphan(ino: u32) -> bool {
    ino != 0 && orphans().contains(&ino)
}

pub fn forget_orphan(ino: u32) {
    for o in orphans().iter_mut().filter(|o| **o == ino) {
        *o = 0;
    }
}
