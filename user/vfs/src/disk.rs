//! Requests to the disk driver.
//!
//! Every sector passes through `DISK_IO_BUF`, which each request lends the
//! driver for the length of the call: a read fills it, a write is copied out of
//! it. The driver never learns where it is, and nothing here needs to know
//! where it lives in physical memory — which is what let both the driver and
//! this server give up their authority over physical memory.
//!
//! Both filesystems used to carry their own copy of each request, down to the
//! physical address they named.

use quark_rt::ipc::Message;
use quark_rt::syscall;

use crate::{DISK_IO_BUF, TAG_DISK_OK, TAG_READ_SECTOR, TAG_READ_SECTORS, TAG_WRITE_SECTOR};

/// Sectors one request may carry: as many as fill `DISK_IO_BUF`.
pub const MAX_SECTORS: u32 = 8;

/// Read `count` sectors (at most [`MAX_SECTORS`]) from the absolute `lba` into
/// `DISK_IO_BUF`.
pub fn read(disk_tid: usize, lba: u32, count: u32) -> Result<(), ()> {
    let count = count.clamp(1, MAX_SECTORS);
    let (tag, data) = if count == 1 {
        (TAG_READ_SECTOR, [lba as u64, 0, 0, 0, 0, 0])
    } else {
        (TAG_READ_SECTORS, [lba as u64, 0, count as u64, 0, 0, 0])
    };
    let buf = unsafe {
        core::slice::from_raw_parts_mut(DISK_IO_BUF as *mut u8, count as usize * 512)
    };
    let mut reply = Message::empty();
    match syscall::sys_call_lend_mut(disk_tid, &Message { sender: 0, tag, data }, &mut reply, buf) {
        Ok(()) if reply.tag == TAG_DISK_OK => Ok(()),
        _ => Err(()),
    }
}

/// Write the sector at the start of `DISK_IO_BUF` to the absolute `lba`.
pub fn write(disk_tid: usize, lba: u32) -> Result<(), ()> {
    let buf = unsafe { core::slice::from_raw_parts(DISK_IO_BUF as *const u8, 512) };
    let msg = Message { sender: 0, tag: TAG_WRITE_SECTOR, data: [lba as u64, 0, 0, 0, 0, 0] };
    let mut reply = Message::empty();
    match syscall::sys_call_lend(disk_tid, &msg, &mut reply, buf) {
        Ok(()) if reply.tag == TAG_DISK_OK => Ok(()),
        _ => Err(()),
    }
}
