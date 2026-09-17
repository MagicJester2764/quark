//! The server as a pager: files mapped into memory.
//!
//! A mapped file is a kernel memory object whose pages this server provides.
//! The kernel asks for a page when a program first touches it (`TAG_PAGE_IN`),
//! lending a frame to fill; the page stays in the object's cache until the
//! object is released, which the kernel says may happen once nothing maps it
//! (`TAG_OBJECT_IDLE`). Writes made through the server while a file is mapped
//! are copied into whatever of it is cached, so that the two agree.
//!
//! The server holds a read-write capability for each object it pages, and
//! derives narrower ones for the programs that map them. Those take CSpace
//! slots, which is what bounds how many files can be mapped at once.

use crate::ext2;
use crate::protocol::*;
use crate::{error_reply, ext2_state, get_handle, reply_opened, CLIENT_BUF, PAGE_SIZE};
use crate::handles::FsFileData;
use quark_rt::ipc::Message;
use quark_rt::syscall;

/// CSpace slots this server keeps object capabilities in, and the one it
/// mints a client's copy in before granting it.
const FIRST_SLOT: usize = 32;
const LAST_SLOT: usize = 61;
const SCRATCH_SLOT: usize = 62;

#[derive(Clone, Copy)]
struct Mapped {
    inode: u32,
    id: u64,
    slot: usize,
}

const MAX_MAPPED: usize = LAST_SLOT - FIRST_SLOT + 1;
static mut MAPPED: [Option<Mapped>; MAX_MAPPED] = [None; MAX_MAPPED];

fn table() -> &'static mut [Option<Mapped>; MAX_MAPPED] {
    unsafe { &mut *core::ptr::addr_of_mut!(MAPPED) }
}

/// A page of this server's own, to copy cached pages through.
static mut PAGE_BUF: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

fn page_buf() -> &'static mut [u8; PAGE_SIZE] {
    unsafe { &mut *core::ptr::addr_of_mut!(PAGE_BUF) }
}

/// Whether a mapping of `inode` is keeping it alive.
pub fn holds(inode: u32) -> bool {
    inode != 0 && table().iter().flatten().any(|m| m.inode == inode)
}

fn find(inode: u32) -> Option<Mapped> {
    table().iter().flatten().find(|m| m.inode == inode).copied()
}

/// Let go of object `id` if nothing maps it. Returns the inode it was for.
fn try_release(id: u64) -> Option<u32> {
    let entry = table().iter_mut().find(|e| e.is_some_and(|m| m.id == id))?;
    let m = entry.unwrap();
    if syscall::sys_object_ctl(m.id, syscall::OBJECT_RELEASE, 0, 0) != 0 {
        return None;
    }
    let _ = syscall::sys_cap_delete(m.slot);
    *entry = None;
    Some(m.inode)
}

/// The object for `inode`, `bytes` long, made on first use.
fn object_for(inode: u32, bytes: u64) -> Result<Mapped, u64> {
    if let Some(m) = find(inode) {
        return Ok(m);
    }
    // Full: whatever nothing maps any more can go now, notice or not.
    if table().iter().all(|e| e.is_some()) {
        let ids: [u64; MAX_MAPPED] = core::array::from_fn(|i| table()[i].map_or(0, |m| m.id));
        for id in ids {
            if let Some(ino) = try_release(id) {
                crate::settle(&[ino]);
            }
        }
    }
    let Some(index) = table().iter().position(|e| e.is_none()) else {
        return Err(ERR_TOO_MANY_OPEN);
    };
    for slot in FIRST_SLOT..=LAST_SLOT {
        if table().iter().flatten().any(|m| m.slot == slot) {
            continue;
        }
        // A slot something else was granted into is refused; try the next.
        if let Ok(id) = syscall::sys_object_create(inode as u64, bytes, slot) {
            let m = Mapped { inode, id, slot };
            table()[index] = Some(m);
            return Ok(m);
        }
    }
    Err(ERR_TOO_MANY_OPEN)
}

/// TAG_MAP: `[handle, flags]`, flags bit 0 asking to write through a shared
/// mapping. Grants the caller a `MemObject` capability and replies
/// `[slot, size]`.
pub fn handle_map(sender: usize, msg: &Message) {
    let Some(file) = get_handle(msg.data[0] as usize, sender) else {
        return error_reply(sender, ERR_INVALID_HANDLE);
    };
    let write_shared = msg.data[1] & 1 != 0;
    let inode_num = match file.fs {
        FsFileData::Ext2 { inode_num } if !file.link && !file.is_dir => inode_num,
        _ => return error_reply(sender, ERR_NOT_SUPPORTED),
    };
    if write_shared && !file.writable {
        return error_reply(sender, ERR_PERMISSION);
    }
    let inode = match ext2::read_inode(ext2_state(), inode_num) {
        Ok(i) => i,
        Err(code) => return error_reply(sender, code),
    };
    if !inode.is_regular() {
        return error_reply(sender, ERR_NOT_SUPPORTED);
    }
    let size = inode.size64();
    let m = match object_for(inode_num, size) {
        Ok(m) => m,
        Err(code) => return error_reply(sender, code),
    };
    let access = syscall::OBJECT_ACCESS_READ
        | if write_shared { syscall::OBJECT_ACCESS_WRITE } else { 0 };
    let _ = syscall::sys_cap_delete(SCRATCH_SLOT);
    if syscall::sys_cap_mint(SCRATCH_SLOT, syscall::CAP_TYPE_MEMOBJECT, m.id, access).is_err() {
        return error_reply(sender, ERR_IO);
    }
    // The caller is in a call to this server, which is its consent.
    let granted = syscall::sys_cap_grant_any(sender, SCRATCH_SLOT);
    let _ = syscall::sys_cap_delete(SCRATCH_SLOT);
    match granted {
        Ok(slot) => reply_opened(sender, [slot as u64, size, 0, 0, 0, 0]),
        Err(()) => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}

/// TAG_PAGE_IN, from the kernel on behalf of `sender`: fill the lent frame
/// with page `data[1]` of inode `data[0]`, zeroes past the end of the file.
pub fn page_in(sender: usize, msg: &Message) {
    let inode_num = msg.data[0] as u32;
    let page = msg.data[1];
    let e2 = ext2_state();
    let inode = match ext2::read_inode(e2, inode_num) {
        Ok(i) => i,
        Err(code) => return error_reply(sender, code),
    };
    let Some(offset) = page.checked_mul(PAGE_SIZE as u64).filter(|&o| o < inode.size64()) else {
        return error_reply(sender, ERR_INVALID_PATH);
    };
    let got = match ext2::read_file_data(e2, &inode, offset as u32, PAGE_SIZE as u32) {
        Ok(n) => n as usize,
        Err(code) => return error_reply(sender, code),
    };
    let data = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, PAGE_SIZE) };
    data[got..].fill(0);
    if syscall::sys_lent_write(sender, 0, data) != Ok(PAGE_SIZE) {
        return error_reply(sender, ERR_IO);
    }
    reply_opened(sender, [0; 6]);
}

/// TAG_OBJECT_IDLE: object `id` is mapped nowhere. Release it, and the inode
/// with it if nothing else holds that.
pub fn idle(id: u64) {
    if let Some(ino) = try_release(id) {
        crate::settle(&[ino]);
    }
}

/// `len` bytes were written at `offset` of `inode` from `CLIENT_BUF`: copy
/// them into any page of it the kernel has cached, so mappings see the file.
pub fn wrote(inode: u32, offset: u64, len: usize) {
    let Some(m) = find(inode) else { return };
    let written = unsafe { core::slice::from_raw_parts(CLIENT_BUF as *const u8, len) };
    let buf = page_buf();
    let mut done = 0usize;
    while done < len {
        let at = offset + done as u64;
        let page = at / PAGE_SIZE as u64;
        let within = (at % PAGE_SIZE as u64) as usize;
        let n = (PAGE_SIZE - within).min(len - done);
        let ptr = buf.as_mut_ptr() as u64;
        if syscall::sys_object_ctl(m.id, syscall::OBJECT_READ_PAGE, ptr, page) == 1 {
            buf[within..within + n].copy_from_slice(&written[done..done + n]);
            let _ = syscall::sys_object_ctl(m.id, syscall::OBJECT_WRITE_PAGE, ptr, page);
        }
        done += n;
    }
}

/// `inode` is now `bytes` long. Pages past the end stop being pageable, and
/// the part of the last page past the end reads as zeroes.
pub fn resized(inode: u32, bytes: u64) {
    let Some(m) = find(inode) else { return };
    let _ = syscall::sys_object_ctl(m.id, syscall::OBJECT_RESIZE, bytes, 0);
    let within = (bytes % PAGE_SIZE as u64) as usize;
    if within != 0 {
        let buf = page_buf();
        let ptr = buf.as_mut_ptr() as u64;
        let page = bytes / PAGE_SIZE as u64;
        if syscall::sys_object_ctl(m.id, syscall::OBJECT_READ_PAGE, ptr, page) == 1 {
            buf[within..].fill(0);
            let _ = syscall::sys_object_ctl(m.id, syscall::OBJECT_WRITE_PAGE, ptr, page);
        }
    }
}
