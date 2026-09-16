//! The file server, as a client sees it.
//!
//! `docs/vfs.md` is the protocol. Paths are lent with the call, so their
//! length is the filesystem's business rather than the message's.

use crate::ipc::Message;
use crate::syscall;

// VFS IPC tags
const TAG_OPEN: u64 = 1;
const TAG_READ: u64 = 2;
const TAG_CLOSE: u64 = 3;
const TAG_STAT: u64 = 5;
const TAG_WRITE: u64 = 6;
const TAG_READDIR_BULK: u64 = 8;
const TAG_MKDIR: u64 = 9;
const TAG_UNLINK: u64 = 10;
const TAG_RMDIR: u64 = 11;
const TAG_RENAME: u64 = 12;
const TAG_TRUNCATE: u64 = 13;
const TAG_STATFS: u64 = 14;
const TAG_ERROR: u64 = u64::MAX;

/// The most one read or write carries.
pub const MAX_IO: usize = 4096;
/// The longest path the server takes. A longer one is refused, not cut.
pub const MAX_PATH: usize = 4095;

/// [`open_with`] makes the file if the name is free.
pub const OPEN_CREATE: u64 = 1;
/// With [`OPEN_CREATE`], the name must be free.
pub const OPEN_EXCLUSIVE: u64 = 2;
/// Empty a regular file the caller may write.
pub const OPEN_TRUNCATE: u64 = 4;
/// The path must name a directory.
pub const OPEN_DIRECTORY: u64 = 8;

// Error codes (match VFS server)
pub const ERR_NOT_FOUND: u64 = 1;
pub const ERR_INVALID_HANDLE: u64 = 2;
pub const ERR_IO: u64 = 3;
pub const ERR_TOO_MANY_OPEN: u64 = 4;
pub const ERR_INVALID_PATH: u64 = 5;
pub const ERR_NOT_DIR: u64 = 6;
pub const ERR_IS_DIR: u64 = 7;
pub const ERR_PERMISSION: u64 = 8;
pub const ERR_READ_ONLY: u64 = 9;
pub const ERR_EXISTS: u64 = 10;
pub const ERR_NOT_EMPTY: u64 = 11;
pub const ERR_NOT_SUPPORTED: u64 = 12;
pub const ERR_NAME_TOO_LONG: u64 = 13;

/// File-type bits of a mode, as [`Stat::mode`] carries them.
pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

/// What [`open_with`] learns about the file it opened.
#[derive(Clone, Copy, Debug)]
pub struct Opened {
    pub handle: usize,
    pub size: u64,
    pub is_dir: bool,
    /// With the file-type bits.
    pub mode: u32,
    /// What this caller may do with it: 4 read, 2 write, 1 execute.
    pub access: u32,
    /// The inode number, stable while the file exists.
    pub id: u64,
}

/// A file's attributes, as `STAT` reports them.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub id: u64,
    pub size: u64,
    pub mode: u32,
    pub links: u32,
    pub uid: u32,
    pub gid: u32,
    /// Seconds since boot; see `docs/vfs.md`.
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    /// In 512-byte units.
    pub blocks: u64,
    pub block_size: u32,
}

/// Call the server with `path` lent, its length in `data[0]`.
fn call_with_path(vfs_tid: usize, tag: u64, path: &[u8], mut data: [u64; 6]) -> Result<Message, u64> {
    if path.is_empty() {
        return Err(ERR_INVALID_PATH);
    }
    if path.len() > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    data[0] = path.len() as u64;
    let msg = Message { sender: 0, tag, data };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(vfs_tid, &msg, &mut reply, path).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(reply)
}

/// One entry of a directory, as a bulk read reports it.
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 255],
    pub name_len: usize,
    pub size: u64,
    pub is_dir: bool,
    /// The inode number (FAT32: the first cluster).
    pub id: u64,
    /// `DT_DIR`, `DT_REG`, `DT_LNK` or `DT_UNKNOWN`.
    pub kind: u8,
}

pub const DT_UNKNOWN: u8 = 0;
pub const DT_DIR: u8 = 4;
pub const DT_REG: u8 = 8;
pub const DT_LNK: u8 = 10;

impl DirEntry {
    pub const fn empty() -> Self {
        Self { name: [0u8; 255], name_len: 0, size: 0, is_dir: false, id: 0, kind: DT_UNKNOWN }
    }

    /// Return the name as a byte slice.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// What one bulk read returned: how many entries, where the next read should
/// start, and whether that is the end.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub count: usize,
    pub next: u64,
    pub end: bool,
}

/// What `STATFS` reports.
#[derive(Clone, Copy, Debug)]
pub struct FsStat {
    /// 0xEF53 for ext2 and ext4, 0x4d44 for FAT.
    pub magic: u64,
    pub block_size: u64,
    pub blocks: u64,
    pub free_blocks: u64,
    /// Free to anybody, not only the superuser.
    pub avail_blocks: u64,
    pub files: u64,
    pub free_files: u64,
    pub name_max: u64,
}

/// Open a file or directory by path, with `OPEN_*` flags.
pub fn open_with(vfs_tid: usize, path: &[u8], flags: u64) -> Result<Opened, u64> {
    let r = call_with_path(vfs_tid, TAG_OPEN, path, [0, flags, 0, 0, 0, 0])?;
    Ok(Opened {
        handle: r.data[0] as usize,
        size: r.data[1],
        is_dir: r.data[2] != 0,
        mode: r.data[3] as u32,
        access: r.data[4] as u32,
        id: r.data[5],
    })
}

/// Open an existing file or directory by path.
/// Returns (handle, file_size, is_dir).
pub fn open(vfs_tid: usize, path: &[u8]) -> Result<(usize, u32, bool), u64> {
    let o = open_with(vfs_tid, path, 0)?;
    Ok((o.handle, o.size as u32, o.is_dir))
}

/// Make a directory.
pub fn mkdir(vfs_tid: usize, path: &[u8]) -> Result<(), u64> {
    call_with_path(vfs_tid, TAG_MKDIR, path, [0; 6]).map(|_| ())
}

/// Read from `offset` into `buf`, at most [`MAX_IO`] bytes of it, which the
/// VFS is lent for the call. Returns bytes actually read.
pub fn read(vfs_tid: usize, handle: usize, buf: &mut [u8], offset: u32) -> Result<u32, u64> {
    let len = buf.len().min(MAX_IO);
    let msg = Message {
        sender: 0,
        tag: TAG_READ,
        data: [handle as u64, 0, offset as u64, len as u64, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(vfs_tid, &msg, &mut reply, &mut buf[..len]).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(reply.data[0] as u32)
}

/// Close an open file/directory handle.
pub fn close(vfs_tid: usize, handle: usize) -> Result<(), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_CLOSE,
        data: [handle as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(())
}

/// The entry of a directory at `index`, or None past its end.
pub fn readdir(vfs_tid: usize, handle: usize, index: u32) -> Result<Option<DirEntry>, u64> {
    let mut one = [DirEntry::empty(); 1];
    let page = readdir_bulk(vfs_tid, handle, index as u64, &mut one)?;
    Ok(if page.count == 1 { Some(one[0]) } else { None })
}

/// Read a directory's entries from `start`, as many as fit `out` and a page.
/// `Page::next` is where the next call should start.
pub fn readdir_bulk(vfs_tid: usize, handle: usize, start: u64, out: &mut [DirEntry]) -> Result<Page, u64> {
    let mut buf = [0u8; 4096];
    let msg = Message {
        sender: 0,
        tag: TAG_READDIR_BULK,
        data: [handle as u64, start, buf.len() as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(vfs_tid, &msg, &mut reply, &mut buf).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    let used = (reply.data[0] as usize).min(buf.len());
    let mut page = Page { count: 0, next: reply.data[1], end: reply.data[2] != 0 };
    if used == 0 && !page.end {
        return Err(ERR_INVALID_PATH); // not even one entry fits a page
    }
    let mut at = 0;
    while at + 28 <= used && page.count < out.len() {
        let r = &buf[at..used];
        let word = |i: usize| u64::from_le_bytes(r[i..i + 8].try_into().unwrap());
        let reclen = u16::from_le_bytes([r[24], r[25]]) as usize;
        let len = r[27] as usize;
        if reclen < 28 + len || reclen > r.len() {
            return Err(ERR_IO);
        }
        let mut e = DirEntry::empty();
        e.name[..len].copy_from_slice(&r[28..28 + len]);
        e.name_len = len;
        e.id = word(0);
        e.size = word(16);
        e.kind = r[26];
        e.is_dir = r[26] == DT_DIR;
        out[page.count] = e;
        page.count += 1;
        page.next = word(8);
        at += reclen;
    }
    // Entries the server sent that `out` had no room for are read again.
    if at < used {
        page.end = false;
    }
    Ok(page)
}

/// What the mounted filesystem is and how full.
pub fn statfs(vfs_tid: usize) -> Result<FsStat, u64> {
    let mut rec = [0u8; 64];
    let msg = Message { sender: 0, tag: TAG_STATFS, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(vfs_tid, &msg, &mut reply, &mut rec).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    let w = |i: usize| u64::from_le_bytes(rec[i * 8..i * 8 + 8].try_into().unwrap());
    Ok(FsStat {
        magic: w(0),
        block_size: w(1),
        blocks: w(2),
        free_blocks: w(3),
        avail_blocks: w(4),
        files: w(5),
        free_files: w(6),
        name_max: w(7),
    })
}

/// Write at most [`MAX_IO`] bytes of `buf` at `offset`, lending them to the
/// VFS for the call. Returns bytes actually written.
pub fn write(vfs_tid: usize, handle: usize, buf: &[u8], offset: u32) -> Result<u32, u64> {
    let len = buf.len().min(MAX_IO);
    let msg = Message {
        sender: 0,
        tag: TAG_WRITE,
        data: [handle as u64, 0, offset as u64, len as u64, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(vfs_tid, &msg, &mut reply, &buf[..len]).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(reply.data[0] as u32)
}

/// Remove a file's name; the file goes with its last one.
pub fn unlink(vfs_tid: usize, path: &[u8]) -> Result<(), u64> {
    call_with_path(vfs_tid, TAG_UNLINK, path, [0; 6]).map(|_| ())
}

/// Remove an empty directory.
pub fn rmdir(vfs_tid: usize, path: &[u8]) -> Result<(), u64> {
    call_with_path(vfs_tid, TAG_RMDIR, path, [0; 6]).map(|_| ())
}

/// Give the file at `from` the name `to`, replacing whatever had it.
pub fn rename(vfs_tid: usize, from: &[u8], to: &[u8]) -> Result<(), u64> {
    if from.is_empty() || to.is_empty() {
        return Err(ERR_INVALID_PATH);
    }
    if from.len() > MAX_PATH || to.len() > MAX_PATH {
        return Err(ERR_NAME_TOO_LONG);
    }
    // Both lent in one buffer, one after the other.
    let mut both = [0u8; 2 * MAX_PATH];
    both[..from.len()].copy_from_slice(from);
    both[from.len()..from.len() + to.len()].copy_from_slice(to);
    let msg = Message {
        sender: 0,
        tag: TAG_RENAME,
        data: [from.len() as u64, to.len() as u64, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(vfs_tid, &msg, &mut reply, &both[..from.len() + to.len()]).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(())
}

/// Make an open file `size` bytes long.
pub fn truncate(vfs_tid: usize, handle: usize, size: u64) -> Result<(), u64> {
    let msg = Message { sender: 0, tag: TAG_TRUNCATE, data: [handle as u64, size, 0, 0, 0, 0] };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(())
}

/// Create a new file or directory, which must not exist, and open it.
/// Returns (handle, size=0, is_dir).
pub fn create(vfs_tid: usize, path: &[u8], is_dir: bool) -> Result<(usize, u32, bool), u64> {
    if is_dir {
        mkdir(vfs_tid, path)?;
        return open(vfs_tid, path);
    }
    let o = open_with(vfs_tid, path, OPEN_CREATE | OPEN_EXCLUSIVE)?;
    Ok((o.handle, 0, false))
}

/// Everything the server knows about an open file.
pub fn stat_full(vfs_tid: usize, handle: usize) -> Result<Stat, u64> {
    let mut rec = [0u8; 88];
    let msg = Message { sender: 0, tag: TAG_STAT, data: [handle as u64, 0, 0, 0, 0, 0] };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(vfs_tid, &msg, &mut reply, &mut rec).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    let w = |i: usize| u64::from_le_bytes(rec[i * 8..i * 8 + 8].try_into().unwrap());
    Ok(Stat {
        id: w(0),
        size: w(1),
        mode: w(2) as u32,
        links: w(3) as u32,
        uid: w(4) as u32,
        gid: w(5) as u32,
        atime: w(6),
        mtime: w(7),
        ctime: w(8),
        blocks: w(9),
        block_size: w(10) as u32,
    })
}

/// An open file's size and whether it is a directory.
pub fn stat(vfs_tid: usize, handle: usize) -> Result<(u32, bool), u64> {
    let s = stat_full(vfs_tid, handle)?;
    Ok((s.size as u32, s.mode & S_IFMT == S_IFDIR))
}
