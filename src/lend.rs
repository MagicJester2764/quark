//! Memory lent with a call.
//!
//! A server reads and writes what a client lent it through the kernel, which
//! copies page by page through the identity map. The server never learns a
//! physical address, cannot reach the buffer once it has replied, and needs no
//! capability over physical memory to serve anybody — which is what let the
//! disk, VFS and NET servers give up theirs.

use crate::paging;

/// The task called may read what is lent.
pub const LEND_READ: u64 = 1 << 62;
/// The task called may write into what is lent.
pub const LEND_WRITE: u64 = 1 << 63;
/// The length, below the two access bits.
pub const LEND_LEN_MASK: u64 = LEND_READ - 1;
/// The most one call may lend.
pub const LEND_MAX: usize = 16 << 20;
/// The most one read or write copies, so interrupts are never off for long.
pub const COPY_MAX: usize = 1 << 20;
/// Every frame the allocator hands out is below this, inside the identity map.
const IDENTITY_END: usize = 0x1_0000_0000;

/// Copy `len` bytes between `local` in the current task and `at` in the
/// address space rooted at `cr3`; `into_lent` says which way.
///
/// Each page is looked up again as it is reached. The lender is blocked, but a
/// thread sharing its address space is not, and it may have unmapped the
/// buffer since the call was made.
///
/// # Safety
/// `cr3` is a live user address space; `local..local + len` has been validated
/// for the current task, writable unless `into_lent`; interrupts are off and
/// stay off, so nothing runs between checking a page and copying it.
pub unsafe fn copy(cr3: usize, at: usize, local: usize, len: usize, into_lent: bool) -> bool {
    unsafe {
        let mut done = 0;
        while done < len {
            let va = match at.checked_add(done) {
                Some(v) => v,
                None => return false,
            };
            let n = (4096 - (va & 0xFFF)).min(len - done);
            let Some(flags) = paging::walk_flags(cr3, va) else {
                return false;
            };
            if flags & paging::USER == 0 || (into_lent && flags & paging::WRITABLE == 0) {
                return false;
            }
            let Some(phys) = paging::translate(cr3, va) else {
                return false;
            };
            if phys + n > IDENTITY_END {
                return false;
            }
            let _ua = crate::cpu::UserAccess::begin();
            if into_lent {
                core::ptr::copy_nonoverlapping((local + done) as *const u8, phys as *mut u8, n);
            } else {
                core::ptr::copy_nonoverlapping(phys as *const u8, (local + done) as *mut u8, n);
            }
            done += n;
        }
        true
    }
}
