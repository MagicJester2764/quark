/// Program arguments and environment, passed by a spawner on a mapped page.
///
/// Layout at ARGS_PAGE_ADDR:
///   [argc: u64]
///   [arg0_len: u64] [arg0 bytes (no null terminator)]
///   [arg1_len: u64] [arg1 bytes]
///   ...
///   [envc: u64]
///   [env0_len: u64] [env0 bytes, "NAME=value"]
///   ...
///
/// The environment comes *after* the arguments rather than before, so a
/// program built before it existed reads the same argv it always did and
/// simply never looks further.

/// Virtual address where the argument page is mapped.
pub const ARGS_PAGE_ADDR: usize = 0x80_8000_0000;

/// Return the number of arguments, or 0 if no args page was set up.
pub fn argc() -> usize {
    let base = ARGS_PAGE_ADDR as *const u64;
    unsafe { *base as usize }
}

/// Return the Nth argument as a byte slice, or None if out of range.
pub fn argv(index: usize) -> Option<&'static [u8]> {
    let count = argc();
    if index >= count {
        return None;
    }
    let base = ARGS_PAGE_ADDR as *const u8;
    let mut offset = 8usize; // skip argc
    for i in 0..count {
        let len_ptr = unsafe { base.add(offset) as *const u64 };
        let len = unsafe { *len_ptr } as usize;
        offset += 8;
        if i == index {
            let slice = unsafe { core::slice::from_raw_parts(base.add(offset), len) };
            return Some(slice);
        }
        offset += len;
    }
    None
}

/// Byte offset just past the last argument.
fn env_offset() -> usize {
    let base = ARGS_PAGE_ADDR as *const u8;
    let count = argc();
    let mut offset = 8usize; // skip argc
    for _ in 0..count {
        let len = unsafe { *(base.add(offset) as *const u64) } as usize;
        offset += 8 + len;
        if offset > PAGE_BYTES {
            return PAGE_BYTES;
        }
    }
    offset
}

/// The page the spawner maps is one page, and a length running past it is a
/// malformed list rather than a long value.
const PAGE_BYTES: usize = 4096;

/// How many environment entries there are.
pub fn envc() -> usize {
    let off = env_offset();
    if off + 8 > PAGE_BYTES {
        return 0;
    }
    unsafe { *((ARGS_PAGE_ADDR + off) as *const u64) as usize }
}

/// The Nth environment entry, as `NAME=value`.
pub fn envp(index: usize) -> Option<&'static [u8]> {
    let count = envc();
    if index >= count {
        return None;
    }
    let base = ARGS_PAGE_ADDR as *const u8;
    let mut offset = env_offset() + 8;
    for i in 0..count {
        if offset + 8 > PAGE_BYTES {
            return None;
        }
        let len = unsafe { *(base.add(offset) as *const u64) } as usize;
        offset += 8;
        if offset + len > PAGE_BYTES {
            return None;
        }
        if i == index {
            return Some(unsafe { core::slice::from_raw_parts(base.add(offset), len) });
        }
        offset += len;
    }
    None
}

/// The value of `name`, or `None`.
///
/// The `=` is checked as well as the prefix. Without it, `HOM` matches
/// `HOME=/home/root` and returns `E=/home/root`.
pub fn getenv(name: &[u8]) -> Option<&'static [u8]> {
    if name.is_empty() {
        return None;
    }
    for i in 0..envc() {
        let entry = envp(i)?;
        if entry.len() > name.len() && &entry[..name.len()] == name && entry[name.len()] == b'=' {
            return Some(&entry[name.len() + 1..]);
        }
    }
    None
}
