//! The VFS protocol as this server speaks it: request tags, flags, error
//! codes, and the records a request's lent buffer carries.
//!
//! `docs/vfs.md` is the contract. The clients — quark-rt, the C library, the
//! Linux translation layer — keep their own copies of these numbers.

use quark_rt::syscall;

pub const TAG_OPEN: u64 = 1;
pub const TAG_READ: u64 = 2;
pub const TAG_CLOSE: u64 = 3;
// 4 read one directory entry at a time and cut its name to 32 bytes;
// TAG_READDIR_BULK replaces it and the number stays taken.
pub const TAG_STAT: u64 = 5;
pub const TAG_WRITE: u64 = 6;
// 7 was CREATE, whose path travelled in the message and was cut to 40 bytes.
// OPEN_CREATE and TAG_MKDIR replace it; the number stays taken.
pub const TAG_READDIR_BULK: u64 = 8;
pub const TAG_MKDIR: u64 = 9;
/// Remove a name; the file goes with its last one.
pub const TAG_UNLINK: u64 = 10;
pub const TAG_RMDIR: u64 = 11;
/// Two paths, lent end to end.
pub const TAG_RENAME: u64 = 12;
pub const TAG_TRUNCATE: u64 = 13;
/// What the filesystem is and how full.
pub const TAG_STATFS: u64 = 14;
pub const TAG_OK: u64 = 0;
pub const TAG_ERROR: u64 = u64::MAX;

/// OPEN makes the file if the name is free.
pub const OPEN_CREATE: u64 = 1;
/// With `OPEN_CREATE`, the name must be free.
pub const OPEN_EXCLUSIVE: u64 = 2;
/// Empty a regular file the caller may write.
pub const OPEN_TRUNCATE: u64 = 4;
/// The path must name a directory.
pub const OPEN_DIRECTORY: u64 = 8;

// Error codes, in an error reply's first word.
pub const ERR_NOT_FOUND: u64 = 1;
pub const ERR_INVALID_HANDLE: u64 = 2;
pub const ERR_IO: u64 = 3;
pub const ERR_TOO_MANY_OPEN: u64 = 4;
pub const ERR_INVALID_PATH: u64 = 5;
pub const ERR_NOT_DIR: u64 = 6;
pub const ERR_IS_DIR: u64 = 7;
pub const ERR_PERMISSION: u64 = 8;
/// The filesystem was mounted read-only, because it uses something a writer
/// would have to maintain and this does not.
pub const ERR_READ_ONLY: u64 = 9;
pub const ERR_EXISTS: u64 = 10;
pub const ERR_NOT_EMPTY: u64 = 11;
/// Something this filesystem cannot do, as opposed to something that failed.
pub const ERR_NOT_SUPPORTED: u64 = 12;
/// A path over [`MAX_PATH`] bytes, or a name over [`MAX_NAME`].
pub const ERR_NAME_TOO_LONG: u64 = 13;

pub const MAX_PATH: usize = 4095;
/// What an ext2 directory entry's one-byte length allows.
pub const MAX_NAME: usize = 255;

/// Where a lent path is copied to be read: room for the two a rename names.
pub const PATH_BUF: usize = 0x88_0000_0000;
pub const PATH_BUF_PAGES: usize = 2;

/// Copy the `len` bytes of path `sender` lent, from `offset` in what it lent,
/// to `at` in [`PATH_BUF`], and return them.
///
/// A path that is too long is refused, never shortened: the one thing worse
/// than failing to open a file is opening a different one.
pub fn lent_path(sender: usize, offset: usize, len: usize, at: usize) -> Result<&'static [u8], u64> {
    if len == 0 {
        return Err(ERR_INVALID_PATH);
    }
    if len > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    if at + len > PATH_BUF_PAGES * 4096 {
        return Err(ERR_INVALID_PATH);
    }
    let buf = unsafe { core::slice::from_raw_parts_mut((PATH_BUF + at) as *mut u8, len) };
    match syscall::sys_lent_read(sender, offset, buf) {
        Ok(n) if n == len => {}
        _ => return Err(ERR_INVALID_PATH),
    }
    if buf.contains(&0) {
        return Err(ERR_INVALID_PATH);
    }
    Ok(buf)
}

/// A directory record: `id`, `next`, `size` (8 bytes each), `reclen` (2),
/// `type` (1), `namelen` (1), then the name and a NUL, padded to 8.
pub const DIRENT_HEADER: usize = 28;
pub const DT_UNKNOWN: u8 = 0;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

/// Write one directory record at `at` in `buf`. Returns its length, or None
/// if it does not fit.
pub fn put_dirent(buf: &mut [u8], at: usize, id: u64, next: u64, size: u64, kind: u8, name: &[u8]) -> Option<usize> {
    let reclen = (DIRENT_HEADER + name.len() + 1 + 7) & !7;
    if name.len() > MAX_NAME || at + reclen > buf.len() {
        return None;
    }
    let r = &mut buf[at..at + reclen];
    r.fill(0);
    r[0..8].copy_from_slice(&id.to_le_bytes());
    r[8..16].copy_from_slice(&next.to_le_bytes());
    r[16..24].copy_from_slice(&size.to_le_bytes());
    r[24..26].copy_from_slice(&(reclen as u16).to_le_bytes());
    r[26] = kind;
    r[27] = name.len() as u8;
    r[DIRENT_HEADER..DIRENT_HEADER + name.len()].copy_from_slice(name);
    Some(reclen)
}

/// What STATFS fills a lent buffer with: eight little-endian words — magic,
/// block size, blocks, free, free to anybody, inodes, free inodes, the longest
/// name.
pub const STATFS_LEN: usize = 64;

pub const STAT_LEN: usize = 88;

/// What `STAT` fills a lent buffer with: eleven little-endian words.
pub struct StatRecord {
    pub id: u64,
    pub size: u64,
    /// With the file-type bits.
    pub mode: u64,
    pub links: u64,
    pub uid: u64,
    pub gid: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    /// In 512-byte units.
    pub blocks: u64,
    pub block_size: u64,
}

impl StatRecord {
    pub fn to_bytes(&self) -> [u8; STAT_LEN] {
        let words = [
            self.id,
            self.size,
            self.mode,
            self.links,
            self.uid,
            self.gid,
            self.atime,
            self.mtime,
            self.ctime,
            self.blocks,
            self.block_size,
        ];
        let mut out = [0u8; STAT_LEN];
        for (i, w) in words.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}
