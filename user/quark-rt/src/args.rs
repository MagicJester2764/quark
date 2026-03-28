/// Program arguments passed by init via a mapped page.
///
/// Layout at ARGS_PAGE_ADDR:
///   [argc: u64]
///   [arg0_len: u64] [arg0 bytes (no null terminator)]
///   [arg1_len: u64] [arg1 bytes]
///   ...

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
