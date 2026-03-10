/// VFS client helpers — wraps VFS IPC protocol for user-space callers.

use crate::ipc::Message;
use crate::syscall;

// VFS IPC tags
const TAG_OPEN: u64 = 1;
const TAG_READ: u64 = 2;
const TAG_CLOSE: u64 = 3;
const TAG_READDIR: u64 = 4;
const TAG_STAT: u64 = 5;
const TAG_WRITE: u64 = 6;
const TAG_CREATE: u64 = 7;
const TAG_ERROR: u64 = u64::MAX;

// Error codes (match VFS server)
pub const ERR_NOT_FOUND: u64 = 1;
pub const ERR_INVALID_HANDLE: u64 = 2;
pub const ERR_IO: u64 = 3;
pub const ERR_TOO_MANY_OPEN: u64 = 4;
pub const ERR_INVALID_PATH: u64 = 5;
pub const ERR_NOT_DIR: u64 = 6;
pub const ERR_IS_DIR: u64 = 7;

pub struct DirEntry {
    pub name: [u8; 11],
    pub size: u32,
    pub is_dir: bool,
    pub cluster: u32,
    pub attr: u8,
}

/// Open a file or directory by path (up to 47 bytes, null-terminated).
/// Returns (handle, file_size, is_dir).
pub fn open(vfs_tid: usize, path: &[u8]) -> Result<(usize, u32, bool), u64> {
    let mut data = [0u64; 6];
    let bytes = unsafe { core::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, 48) };
    let len = path.len().min(47);
    bytes[..len].copy_from_slice(&path[..len]);

    let msg = Message { sender: 0, tag: TAG_OPEN, data };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok((reply.data[0] as usize, reply.data[1] as u32, reply.data[2] != 0))
}

/// Read file data into a client-owned physical page.
/// `phys_addr` must be a physical address the VFS can map.
/// Returns bytes actually read.
pub fn read(
    vfs_tid: usize,
    handle: usize,
    phys_addr: usize,
    offset: u32,
    max_bytes: u32,
) -> Result<u32, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_READ,
        data: [handle as u64, phys_addr as u64, offset as u64, max_bytes as u64, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
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

    // Unpack name from 2 u64 words
    let mut name_bytes = [0u8; 16];
    name_bytes[0..8].copy_from_slice(&reply.data[0].to_le_bytes());
    name_bytes[8..16].copy_from_slice(&reply.data[1].to_le_bytes());
    let mut name = [0u8; 11];
    name.copy_from_slice(&name_bytes[..11]);

    let size = reply.data[2] as u32;
    let flags_cluster = reply.data[3];
    let is_dir = (flags_cluster >> 32) != 0;
    let cluster = (flags_cluster & 0xFFFF_FFFF) as u32;
    let attr = reply.data[4] as u8;

    Ok(Some(DirEntry { name, size, is_dir, cluster, attr }))
}

/// Write data from a client-owned physical page into a file.
/// `phys_addr` must be a physical address the VFS can map.
/// Returns bytes actually written.
pub fn write(
    vfs_tid: usize,
    handle: usize,
    phys_addr: usize,
    offset: u32,
    len: u32,
) -> Result<u32, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_WRITE,
        data: [handle as u64, phys_addr as u64, offset as u64, len as u64, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok(reply.data[0] as u32)
}

/// Create a new file or directory.
/// Returns (handle, size=0, is_dir).
/// If `is_dir` is true, creates a directory; otherwise creates a file.
pub fn create(vfs_tid: usize, path: &[u8], is_dir: bool) -> Result<(usize, u32, bool), u64> {
    let mut data = [0u64; 6];
    let bytes = unsafe { core::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, 48) };
    let len = path.len().min(40); // leave room for flags in data[5]
    bytes[..len].copy_from_slice(&path[..len]);
    data[5] = if is_dir { 1 } else { 0 };

    let msg = Message { sender: 0, tag: TAG_CREATE, data };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok((reply.data[0] as usize, reply.data[1] as u32, reply.data[2] != 0))
}

/// Get file/directory info for an open handle.
/// Returns (size, is_dir).
pub fn stat(vfs_tid: usize, handle: usize) -> Result<(u32, bool), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_STAT,
        data: [handle as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(vfs_tid, &msg, &mut reply).is_err() {
        return Err(ERR_IO);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok((reply.data[0] as u32, reply.data[1] != 0))
}
