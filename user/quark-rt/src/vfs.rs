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
const TAG_READDIR: u64 = 4;
const TAG_STAT: u64 = 5;
const TAG_WRITE: u64 = 6;
const TAG_READDIR_BULK: u64 = 8;
const TAG_MKDIR: u64 = 9;
const TAG_UNLINK: u64 = 10;
const TAG_RMDIR: u64 = 11;
const TAG_RENAME: u64 = 12;
const TAG_TRUNCATE: u64 = 13;
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

#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 48],
    pub name_len: u8,
    pub size: u32,
    pub is_dir: bool,
    pub cluster: u32,
    pub attr: u8,
}

impl DirEntry {
    pub const fn empty() -> Self {
        Self {
            name: [0u8; 48],
            name_len: 0,
            size: 0,
            is_dir: false,
            cluster: 0,
            attr: 0,
        }
    }

    /// Return the name as a byte slice.
    pub fn name_bytes(&self) -> &[u8] {
        &self.name[..self.name_len as usize]
    }
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

/// Read a directory entry by index.
/// Returns None when no more entries.
pub fn readdir(vfs_tid: usize, handle: usize, index: u32) -> Result<Option<DirEntry>, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_READDIR,
        data: [handle as u64, index as u64, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        if reply.data[0] == ERR_NOT_FOUND {
            return Ok(None);
        }
        return Err(reply.data[0]);
    }

    // Unpack name from first 4 u64 words (32 bytes, name_len in data[4] low byte)
    let mut name = [0u8; 48];
    let name_data = [
        reply.data[0].to_le_bytes(),
        reply.data[1].to_le_bytes(),
        reply.data[2].to_le_bytes(),
        reply.data[3].to_le_bytes(),
    ];
    for (i, chunk) in name_data.iter().enumerate() {
        name[i * 8..(i + 1) * 8].copy_from_slice(chunk);
    }

    let packed = reply.data[4];
    let name_len = (packed & 0xFF) as u8;
    let attr = ((packed >> 8) & 0xFF) as u8;
    let is_dir = attr & 0x10 != 0;

    let size = reply.data[5] as u32;
    let cluster = 0u32; // not used for ext2

    Ok(Some(DirEntry { name, name_len, size, is_dir, cluster, attr }))
}

/// Read a directory's entries — as many as fit a page — in one call.
/// Returns the number of entries written into `out`.
pub fn readdir_bulk(vfs_tid: usize, handle: usize, out: &mut [DirEntry]) -> Result<usize, u64> {
    let mut buf = [0u8; 4096];
    let msg = Message {
        sender: 0,
        tag: TAG_READDIR_BULK,
        data: [handle as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(vfs_tid, &msg, &mut reply, &mut buf).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }

    // Entry layout: 64 bytes each (48 name + 1 name_len + 1 attr + 2 pad + 4 size + 4 cluster + 4 pad)
    let max_per_page = 4096 / 64; // 64
    let count = (reply.data[0] as usize).min(out.len()).min(max_per_page);

    for i in 0..count {
        let base = i * 64;
        let mut name = [0u8; 48];
        name.copy_from_slice(&buf[base..base + 48]);
        let name_len = buf[base + 48];
        let attr = buf[base + 49];
        let size = u32::from_le_bytes(buf[base + 52..base + 56].try_into().unwrap());
        let cluster = u32::from_le_bytes(buf[base + 56..base + 60].try_into().unwrap());
        let is_dir = attr & 0x10 != 0;
        out[i] = DirEntry { name, name_len, size, is_dir, cluster, attr };
    }

    Ok(count)
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
