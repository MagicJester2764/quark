//! Each program's working directory.
//!
//! A directory is held by its inode, as Linux holds it, so renaming anything
//! above it changes what `getcwd` says and nothing else, and one removed while
//! a program is in it stays until the program leaves: a working directory
//! counts as an open reference. FAT32, which renames nothing, keeps the path.
//!
//! A program nobody gave a directory is at `/`, and needs no entry. Programs
//! are named by their address space's id, like the handles they hold.

use crate::protocol::MAX_PATH;
use quark_rt::syscall;

/// As many programs as there can be tasks.
pub const MAX_PROGRAMS: usize = 64;

#[derive(Clone, Copy, PartialEq)]
pub enum Where {
    Root,
    Inode(u32),
    /// A FAT32 path, absolute and without `.` or `..`, `len` bytes of the
    /// entry's `path`.
    Path(usize),
}

struct Entry {
    space: u64,
    at: Where,
    path: [u8; MAX_PATH + 1],
}

static mut TABLE: [Entry; MAX_PROGRAMS] = {
    const EMPTY: Entry = Entry { space: 0, at: Where::Root, path: [0; MAX_PATH + 1] };
    [EMPTY; MAX_PROGRAMS]
};

fn table() -> &'static mut [Entry; MAX_PROGRAMS] {
    unsafe { &mut *core::ptr::addr_of_mut!(TABLE) }
}

/// Where program `space` is, and for a FAT32 path, the path.
pub fn get(space: u64) -> (Where, &'static [u8]) {
    match table().iter().find(|e| e.space != 0 && e.space == space) {
        Some(e) => match e.at {
            Where::Path(len) => (e.at, &e.path[..len]),
            at => (at, b"/"),
        },
        None => (Where::Root, b"/"),
    }
}

/// Put program `space` at `to` (with `path` for a FAT32 directory). Returns
/// what it left, so that a directory nobody holds any more can be freed.
pub fn set(space: u64, to: Where, path: &[u8]) -> Result<Where, u64> {
    if space == 0 {
        return Err(crate::ERR_INVALID_HANDLE);
    }
    let t = table();
    let found = t.iter().position(|e| e.space == space);
    let old = found.map_or(Where::Root, |i| t[i].at);
    if to == Where::Root {
        if let Some(i) = found {
            t[i].space = 0;
            t[i].at = Where::Root;
        }
        return Ok(old);
    }
    let i = match found {
        Some(i) => i,
        None => {
            let i = t.iter().position(|e| e.space == 0).ok_or(crate::ERR_TOO_MANY_OPEN)?;
            // Told when the program goes, so its directory is let go. A
            // program already gone cannot be calling.
            let _ = syscall::sys_space_watch(space);
            i
        }
    };
    let e = &mut t[i];
    e.space = space;
    e.at = to;
    if let Where::Path(len) = to {
        e.path[..len].copy_from_slice(&path[..len]);
    }
    Ok(old)
}

/// Program `space` has gone. Returns the directory it was in.
pub fn forget(space: u64) -> Where {
    let t = table();
    match t.iter_mut().find(|e| e.space != 0 && e.space == space) {
        Some(e) => {
            e.space = 0;
            core::mem::replace(&mut e.at, Where::Root)
        }
        None => Where::Root,
    }
}

/// Whether some program is in directory `ino`.
pub fn holds(ino: u32) -> bool {
    ino != 0 && table().iter().any(|e| e.space != 0 && e.at == Where::Inode(ino))
}

/// Make `path`, relative to FAT32 directory `dir`, absolute, with `.` and `..`
/// taken as written: FAT32 has no links to make that wrong. The result is
/// left in a buffer of its own.
pub fn join(dir: &[u8], path: &[u8]) -> Result<&'static [u8], u64> {
    static mut JOINED: [u8; MAX_PATH + 1] = [0; MAX_PATH + 1];
    let out = unsafe { &mut *core::ptr::addr_of_mut!(JOINED) };
    let mut len = 0usize;
    let from_root = path.first() == Some(&b'/');
    let parts = if from_root { &[][..] } else { dir }
        .split(|&b| b == b'/')
        .chain(path.split(|&b| b == b'/'));
    for part in parts {
        match part {
            b"" | b"." => {}
            b".." => {
                // Back to the slash before the last component.
                while len > 0 && out[len - 1] != b'/' {
                    len -= 1;
                }
                len = len.saturating_sub(1);
            }
            _ => {
                if len + 1 + part.len() > MAX_PATH {
                    return Err(crate::ERR_NAME_TOO_LONG);
                }
                out[len] = b'/';
                out[len + 1..len + 1 + part.len()].copy_from_slice(part);
                len += 1 + part.len();
            }
        }
    }
    if len == 0 {
        out[0] = b'/';
        len = 1;
    }
    // A trailing slash says "a directory"; keep saying it.
    if path.len() > 1 && path.ends_with(b"/") && len > 1 {
        if len + 1 > MAX_PATH {
            return Err(crate::ERR_NAME_TOO_LONG);
        }
        out[len] = b'/';
        len += 1;
    }
    Ok(&out[..len])
}
