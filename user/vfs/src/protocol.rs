//! The VFS protocol as this server speaks it: request tags, flags, error
//! codes, and the records a request's lent buffer carries.
//!
//! `docs/vfs.md` is the contract. The clients — quark-rt, the C library, the
//! Linux translation layer — keep their own copies of these numbers.

use quark_rt::syscall;

pub const TAG_OPEN: u64 = 1;
pub const TAG_READ: u64 = 2;
pub const TAG_CLOSE: u64 = 3;
/// One entry at a time, with its name cut to 32 bytes.
pub const TAG_READDIR: u64 = 4;
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
