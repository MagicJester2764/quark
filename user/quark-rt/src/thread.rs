//! Threads: tasks that share an address space.
//!
//! Quark has no separate thread object. A thread is an ordinary task started
//! with the *same* `cr3` as its creator, so the two run the same memory with
//! separate registers and stacks. The kernel refcounts address spaces, so the
//! one they share survives until the last of them exits.
//!
//! What this does not give you is thread-local storage. The hosted target still
//! declares `singlethread`, and `thread_local!` will not work until a TLS
//! register is set up per thread. Everything here is shared: use `sync` for
//! anything two threads both touch.
//!
//! Spawning needs `TaskMgmt` to create a task and `PhysAlloc` to give it a
//! stack, so a program that spawns threads must ask for both in its manifest.

use crate::syscall;

/// Default stack for a spawned thread, in pages.
pub const DEFAULT_STACK_PAGES: usize = 4;

/// Where thread stacks are placed, growing *down* from just below the main
/// task's stack, one megabyte apart so an overflow runs into unmapped memory
/// rather than into the next thread's stack.
const THREAD_STACK_BASE: usize = 0x7FFF_0000_0000;
const THREAD_STACK_STRIDE: usize = 0x10_0000;

/// Hands out stack regions. Callers used to pass a slot number, which meant
/// every caller had to know what every other caller had used — unworkable for
/// a runtime that spawns threads on behalf of code it does not control.
static NEXT_SLOT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn take_slot() -> usize {
    NEXT_SLOT.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// A running thread.
pub struct Thread {
    tid: usize,
}

impl Thread {
    /// The thread's task ID.
    pub fn tid(&self) -> usize {
        self.tid
    }

    /// Wait for it to finish, returning its exit status.
    ///
    /// `sys_wait` reaps whichever child exits first, so this loops until the
    /// one we want comes back. A parent waiting on several threads gets them
    /// in completion order, not call order.
    pub fn join(self) -> i32 {
        loop {
            match syscall::sys_wait() {
                Ok((tid, code)) if tid == self.tid => return code,
                Ok(_) => continue,
                Err(()) => return -1,
            }
        }
    }
}

/// Start `entry` on a new thread, handing it `arg`.
///
/// This is what a runtime needs: `entry` is a trampoline and `arg` the boxed
/// closure it should run. [`spawn`] is the same thing with no argument.
pub fn spawn_with_arg(
    entry: extern "C" fn(usize) -> !,
    arg: usize,
    stack_pages: usize,
) -> Result<Thread, ()> {
    start(entry as usize as u64, arg as u64, take_slot(), stack_pages)
}

/// Start `entry` on a new thread in this address space.
///
/// The entry point takes no arguments and must not return: there is nowhere to
/// return to, so it has to exit the task itself.
pub fn spawn(entry: extern "C" fn() -> !) -> Result<Thread, ()> {
    spawn_with_stack(entry, DEFAULT_STACK_PAGES)
}

/// As [`spawn`], with a stack size in pages.
pub fn spawn_with_stack(entry: extern "C" fn() -> !, stack_pages: usize) -> Result<Thread, ()> {
    start(entry as usize as u64, 0, take_slot(), stack_pages)
}

fn start(entry: u64, arg: u64, slot: usize, stack_pages: usize) -> Result<Thread, ()> {
    if stack_pages == 0 {
        return Err(());
    }

    let cr3 = syscall::sys_addrspace_self()?;
    let tid = syscall::sys_task_create()?;

    // Map the stack into the address space we already share, so the thread can
    // use it the moment it starts. Frames we allocate are ours to map: the
    // kernel authorises that by ownership, without any capability.
    let top = THREAD_STACK_BASE - slot * THREAD_STACK_STRIDE;
    let bottom = top - stack_pages * crate::spawn::PAGE_SIZE;
    for p in 0..stack_pages {
        let frame = syscall::sys_phys_alloc(1)?;
        syscall::sys_map_phys(frame, bottom + p * crate::spawn::PAGE_SIZE, 1)?;
    }

    syscall::sys_task_start_arg(tid, entry, top as u64, cr3, arg)?;
    Ok(Thread { tid })
}
