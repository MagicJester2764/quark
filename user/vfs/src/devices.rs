//! `/dev`: the devices every C program expects to find.
//!
//! None of them is on a disk. A path under `/dev` is answered here before any
//! filesystem sees it, and a handle on one of these is served here too. The
//! root filesystem still carries an empty `/dev` directory, so that listing
//! `/` shows it.

use crate::handles::{self, FsFileData, OpenFile};
use crate::protocol::*;
use crate::{error_reply, lend_out, reply_opened, space_of, CLIENT_BUF, PAGE_SIZE};
use quark_rt::ipc::Message;
use quark_rt::syscall;

#[derive(Clone, Copy, PartialEq)]
pub enum Device {
    Null,
    Zero,
    Full,
    Random,
    Urandom,
}

pub const NAMES: [(&[u8], Device); 5] = [
    (b"null", Device::Null),
    (b"zero", Device::Zero),
    (b"full", Device::Full),
    (b"random", Device::Random),
    (b"urandom", Device::Urandom),
];

/// Ids beside anything a filesystem hands out: the devices in the order of
/// [`NAMES`], then the directory.
const FIRST_ID: u64 = 0xFFFF_FF00;
const DIR_ID: u64 = FIRST_ID + NAMES.len() as u64;
/// The root directory's inode, which is what `..` names.
const ROOT_ID: u64 = 2;

const DEVICE_MODE: u64 = 0o020666;
const DIR_MODE: u64 = 0o040755;

/// What a path names, as far as this module is concerned.
#[derive(Clone, Copy, PartialEq)]
pub enum Lookup {
    /// Not under `/dev`: the filesystem's business.
    Elsewhere,
    Dir,
    Device(Device),
    /// Under `/dev`, and nothing is there.
    Missing,
}

/// Where `path` lands, spelled any way: repeated slashes, `.` and `..` are
/// taken as they would be walked from the root.
pub fn lookup(path: &[u8]) -> Lookup {
    // The first two components that survive are all that matter: nothing
    // under /dev is a directory, so anything deeper is missing. `extra`
    // counts the components past the ones kept.
    let mut kept: [&[u8]; 2] = [b""; 2];
    let mut depth = 0usize;
    let mut extra = 0usize;
    for part in path.split(|&b| b == b'/') {
        match part {
            b"" | b"." => {}
            b".." if extra > 0 => extra -= 1,
            b".." => depth = depth.saturating_sub(1),
            _ if extra == 0 && depth < kept.len() => {
                kept[depth] = part;
                depth += 1;
            }
            _ => extra += 1,
        }
    }
    if depth == 0 || kept[0] != b"dev" {
        return Lookup::Elsewhere;
    }
    if extra > 0 {
        return Lookup::Missing;
    }
    if depth == 1 {
        return Lookup::Dir;
    }
    match NAMES.iter().find(|(name, _)| *name == kept[1]) {
        Some((_, dev)) => Lookup::Device(*dev),
        None => Lookup::Missing,
    }
}

/// The device called `name` in `/dev`.
pub fn by_name(name: &[u8]) -> Option<Device> {
    NAMES.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
}

fn index_of(dev: Device) -> usize {
    NAMES.iter().position(|(_, d)| *d == dev).unwrap_or(0)
}

/// OPEN on a path under `/dev`. `found` is what [`lookup`] said, and is not
/// `Elsewhere`.
pub fn open(sender: usize, path: &[u8], found: Lookup, flags: u64) {
    let trailing = path.len() > 1 && path[path.len() - 1] == b'/';
    let (is_dir, fs, id, mode, access) = match found {
        Lookup::Dir => (true, FsFileData::DevDir, DIR_ID, DIR_MODE, 5),
        Lookup::Device(dev) => {
            if trailing || flags & OPEN_DIRECTORY != 0 {
                return error_reply(sender, ERR_NOT_DIR);
            }
            (false, FsFileData::Device(dev), FIRST_ID + index_of(dev) as u64, DEVICE_MODE, 6)
        }
        // Nothing can be made here: the devices are the whole of it.
        Lookup::Missing if flags & OPEN_CREATE != 0 => return error_reply(sender, ERR_PERMISSION),
        _ => return error_reply(sender, ERR_NOT_FOUND),
    };
    if flags & OPEN_CREATE != 0 && flags & OPEN_EXCLUSIVE != 0 {
        return error_reply(sender, ERR_EXISTS);
    }
    let file = OpenFile {
        in_use: true,
        owner: space_of(sender),
        file_size: 0,
        is_dir,
        writable: !is_dir,
        link: false,
        read_offset: 0,
        fs,
    };
    match handles::alloc(file) {
        Some(handle) => reply_opened(sender, [handle as u64, 0, is_dir as u64, mode, access, id]),
        None => error_reply(sender, ERR_TOO_MANY_OPEN),
    }
}

/// Whether `msg`, a request naming a handle, is for one of these.
pub fn is_ours(sender: usize, msg: &Message) -> bool {
    matches!(
        crate::get_handle(msg.data[0] as usize, sender).map(|f| &f.fs),
        Some(FsFileData::Device(_) | FsFileData::DevDir)
    )
}

/// READ, WRITE, STAT, READDIR_BULK and TRUNCATE on a handle [`is_ours`] said
/// is ours.
pub fn serve(sender: usize, msg: &Message) {
    let Some(file) = crate::get_handle(msg.data[0] as usize, sender) else {
        return error_reply(sender, ERR_INVALID_HANDLE);
    };
    let target = match file.fs {
        FsFileData::Device(dev) => Some(dev),
        FsFileData::DevDir => None,
        _ => return error_reply(sender, ERR_INVALID_HANDLE),
    };
    match (msg.tag, target) {
        (TAG_READ, Some(dev)) => read(sender, dev, msg.data[3] as usize),
        (TAG_READ, None) => error_reply(sender, ERR_IS_DIR),
        (TAG_WRITE, Some(Device::Full)) => error_reply(sender, ERR_NO_SPACE),
        // Written and forgotten. Linux would stir the pool with what is
        // written to random; nothing here needs to.
        (TAG_WRITE, Some(_)) => reply_opened(sender, [msg.data[3].min(PAGE_SIZE as u64), 0, 0, 0, 0, 0]),
        (TAG_WRITE, None) => error_reply(sender, ERR_IS_DIR),
        (TAG_STAT, _) => stat(sender, target),
        (TAG_READDIR_BULK, None) => list(sender, msg.data[1], msg.data[2] as usize),
        (TAG_READDIR_BULK, Some(_)) => error_reply(sender, ERR_NOT_DIR),
        (TAG_TRUNCATE, Some(_)) => error_reply(sender, ERR_INVALID_PATH),
        (TAG_TRUNCATE, None) => error_reply(sender, ERR_IS_DIR),
        _ => error_reply(sender, ERR_NOT_SUPPORTED),
    }
}

fn read(sender: usize, dev: Device, want: usize) {
    let n = want.min(PAGE_SIZE);
    let buf = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, n) };
    let n = match dev {
        Device::Null => 0,
        Device::Zero | Device::Full => {
            buf.fill(0);
            n
        }
        Device::Random | Device::Urandom => match quark_rt::random::fill(buf) {
            Ok(()) => n,
            Err(()) => return error_reply(sender, ERR_IO),
        },
    };
    if lend_out(sender, n) {
        reply_opened(sender, [n as u64, 0, 0, 0, 0, 0]);
    } else {
        error_reply(sender, ERR_IO);
    }
    // What was handed out is the caller's alone.
    if matches!(dev, Device::Random | Device::Urandom) {
        buf.fill(0);
    }
}

fn stat(sender: usize, target: Option<Device>) {
    let now = syscall::unix_time();
    let (id, mode, links) = match target {
        Some(dev) => (FIRST_ID + index_of(dev) as u64, DEVICE_MODE, 1),
        None => (DIR_ID, DIR_MODE, 2),
    };
    let record = StatRecord {
        id,
        size: 0,
        mode,
        links,
        uid: 0,
        gid: 0,
        atime: now,
        mtime: now,
        ctime: now,
        blocks: 0,
        block_size: PAGE_SIZE as u64,
    };
    match syscall::sys_lent_write(sender, 0, &record.to_bytes()) {
        Ok(n) if n == STAT_LEN => reply_opened(sender, [STAT_LEN as u64, 0, 0, 0, 0, 0]),
        _ => error_reply(sender, ERR_IO),
    }
}

/// The directory: `.`, `..`, then the devices, as a bulk read lists them.
fn list(sender: usize, start: u64, room: usize) {
    let room = room.min(PAGE_SIZE);
    let buf = unsafe { core::slice::from_raw_parts_mut(CLIENT_BUF as *mut u8, room) };
    let mut entries: [(u64, u8, &[u8]); 2 + NAMES.len()] = [(0, 0, b""); 2 + NAMES.len()];
    entries[0] = (DIR_ID, DT_DIR, b".");
    entries[1] = (ROOT_ID, DT_DIR, b"..");
    for (i, (name, _)) in NAMES.iter().enumerate() {
        entries[2 + i] = (FIRST_ID + i as u64, DT_CHR, name);
    }
    let mut used = 0;
    let mut next = start;
    let mut end = true;
    for (index, (id, kind, name)) in entries.iter().enumerate().skip(start as usize) {
        match put_dirent(buf, used, *id, index as u64 + 1, 0, *kind, name) {
            Some(len) => {
                used += len;
                next = index as u64 + 1;
            }
            None => {
                end = false;
                break;
            }
        }
    }
    crate::reply_dirents(sender, used, next, end);
}

/// Whether a request that changes names touches `/dev`, where nothing may be
/// made, removed or renamed.
pub fn refuses(path: &[u8]) -> bool {
    lookup(path) != Lookup::Elsewhere
}
