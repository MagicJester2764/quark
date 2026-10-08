//! What a thread library keeps with the kernel for each task: its name,
//! Linux's `comm`, and the list of robust mutexes it holds.
//!
//! A name is fifteen bytes at most (`SYS_TASK_NAME`), and none is the
//! program's own name (`SYS_PROGRAM_NAME`), which whoever reads it shows
//! then. A task is called what the task that made it was, as on Linux, and
//! `exec` forgets it.
//!
//! A robust list is where a task's C library keeps the mutexes it holds
//! that must not stay held if it dies: Linux's `set_robust_list`
//! (`SYS_ROBUST_LIST`). The kernel does nothing with it until the task dies
//! or becomes another program. Then it walks the list in the task's own
//! memory, as it was, a word at a time through its tables; each mutex still
//! marked with the task's id is marked as one whose owner died
//! (FUTEX_OWNER_DIED), and a waiter is woken — or, for a priority-inheriting
//! one somebody waits in the kernel to lock, handed to the best of them
//! (`futex::pi_owner_died`). A C library that ends a
//! thread itself walks the list first, and musl does: what is left for this
//! is a program that ends without it — `_exit`, a kill, a fault — holding a
//! mutex another program waits on, in memory they share. A page that is not
//! there, written out or never touched, ends the walk: what it held is
//! nobody's to find.

use crate::task::MAX_TASKS;

/// The longest name: Linux's TASK_COMM_LEN, less its nought.
pub const NAME_MAX: usize = 15;
/// The most entries a walk follows, as Linux's ROBUST_LIST_LIMIT: a list
/// that loops is the program's mistake, and not the kernel's to follow.
const MOST: usize = 2048;
/// In a robust mutex's word: the owner died, and somebody waits; and the
/// owner's id, in the rest of the word. Linux's, as a priority-inheriting
/// word's are (`futex.rs`).
const OWNER_DIED: u32 = crate::futex::PI_OWNER_DIED;
const WAITERS: u32 = crate::futex::PI_WAITERS;
const OWNER: u32 = crate::futex::PI_OWNER;

/// What this module keeps about a task, in its record (`TaskRec::threads`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    /// Each task's name, its length in the first byte.
    name: [u8; NAME_MAX + 1],
    /// Where each task's robust list's head is, in its memory: 0 for none.
    robust: u64,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            name: [0; NAME_MAX + 1],
            robust: 0,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.threads,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();


#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

/// A task was made by `maker`: called what it was, holding no robust list.
pub fn task_made(tid: usize, maker: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        let name = if maker < MAX_TASKS && maker != tid { st(maker).name } else { [0; NAME_MAX + 1] };
        st(tid).name = name;
        st(tid).robust = 0;
    }
    irq_restore(flags);
}

/// `tid`'s name, and how long it is: 0 for none.
pub fn name_of(tid: usize) -> ([u8; NAME_MAX], usize) {
    let mut out = [0u8; NAME_MAX];
    if tid >= MAX_TASKS {
        return (out, 0);
    }
    let flags = irq_save();
    let len = unsafe {
        let n = &st(tid).name;
        let len = (n[0] as usize).min(NAME_MAX);
        out[..len].copy_from_slice(&n[1..1 + len]);
        len
    };
    irq_restore(flags);
    (out, len)
}

/// Call `tid` `name`: its first fifteen bytes, up to a nought.
pub fn set_name(tid: usize, name: &[u8]) {
    if tid >= MAX_TASKS {
        return;
    }
    let len = name.iter().take(NAME_MAX).position(|&b| b == 0).unwrap_or(name.len().min(NAME_MAX));
    let flags = irq_save();
    unsafe {
        let n = &mut st(tid).name;
        n[0] = len as u8;
        n[1..1 + len].copy_from_slice(&name[..len]);
    }
    irq_restore(flags);
}

/// `SYS_ROBUST_LIST`: where `tid`'s robust list's head is, said (`head`) or
/// asked (`u64::MAX`). What it was.
pub fn robust_list(tid: usize, head: u64) -> u64 {
    if tid >= MAX_TASKS {
        return u64::MAX;
    }
    if head != u64::MAX && head != 0 && (head & 7 != 0 || !crate::paging::user_range_ok(head as usize & !0xFFF, 1)) {
        return u64::MAX;
    }
    let flags = irq_save();
    let was = unsafe {
        let r = &mut st(tid).robust;
        let was = *r;
        if head != u64::MAX {
            *r = head;
        }
        was
    };
    irq_restore(flags);
    was
}

/// The word at `at` in `cr3`, if the page is there.
fn read<const N: usize>(cr3: usize, at: u64) -> Option<[u8; N]> {
    if at & (N as u64 - 1) != 0 || !crate::paging::user_range_ok(at as usize & !0xFFF, 1) {
        return None;
    }
    let phys = unsafe { crate::paging::translate(cr3, at as usize)? };
    if phys + N > crate::paging::identity_end() {
        return None;
    }
    Some(unsafe { core::ptr::read_volatile(phys as *const [u8; N]) })
}

fn read_u64(cr3: usize, at: u64) -> Option<u64> {
    read::<8>(cr3, at).map(u64::from_le_bytes)
}

/// The robust mutex whose word is at `at` in `cr3`: if `tid` held it, its
/// owner died, and one waiter is woken to find so.
fn owner_died(cr3: usize, at: u64, tid: usize) {
    let Some(word) = read::<4>(cr3, at).map(u32::from_le_bytes) else { return };
    if word & OWNER != tid as u32 {
        return;
    }
    if crate::futex::pi_owner_died(cr3, at, tid) {
        return;
    }
    let flags = irq_save();
    let written = unsafe {
        // Reached by its frame, so a page it shares since a fork is made
        // its own first; one it cannot be is not written.
        let _ = crate::paging::own(cr3, at as usize);
        let writable = crate::paging::walk_flags(cr3, at as usize)
            .is_some_and(|f| f & crate::paging::USER != 0 && f & crate::paging::WRITABLE != 0);
        match crate::paging::translate(cr3, at as usize) {
            Some(phys) if writable && phys + 4 <= crate::paging::identity_end() => {
                core::ptr::write_volatile(phys as *mut u32, (word & WAITERS) | OWNER_DIED);
                true
            }
            _ => false,
        }
    };
    irq_restore(flags);
    if written {
        crate::futex::wake_in(cr3, at, 1);
    }
}

/// `tid` has died or is becoming another program: what its robust list says
/// it held, it holds no longer.
pub fn let_go(tid: usize) {
    let head = robust_list(tid, 0);
    if head == 0 || head == u64::MAX {
        return;
    }
    let cr3 = crate::scheduler::task_cr3(tid);
    if cr3 == 0 {
        return;
    }
    // { next, futex_offset, pending }
    let (Some(first), Some(offset), Some(pending)) =
        (read_u64(cr3, head), read_u64(cr3, head + 8), read_u64(cr3, head + 16))
    else {
        return;
    };
    let offset = offset as i64;
    let mut entry = first;
    let mut n = 0;
    while entry != head && entry != 0 && n < MOST {
        // The next before this one is touched: marking it may be what a
        // waiter, woken, unlinks.
        let next = read_u64(cr3, entry);
        if entry != pending {
            owner_died(cr3, entry.wrapping_add(offset as u64), tid);
        }
        let Some(next) = next else { break };
        entry = next;
        n += 1;
    }
    if pending != 0 {
        owner_died(cr3, pending.wrapping_add(offset as u64), tid);
    }
}

/// `tid` is becoming another program: it holds what it held no longer, and
/// is called by its new name.
pub fn exec(tid: usize) {
    let_go(tid);
    // And what it held that was not on the list, nobody lends it any more.
    crate::futex::owner_gone(tid);
    set_name(tid, &[]);
}
