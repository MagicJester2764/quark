/// Preemptive round-robin scheduler for the Quark microkernel.
///
/// Uses a ready queue of task IDs. The PIT timer IRQ calls `schedule()`
/// to preempt the running task and switch to the next ready one.

use crate::context;
use crate::task::{Task, TaskState, KERNEL_STACK_SIZE, MAX_TASKS};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Task table: fixed-size array of Option<Task>
static mut TASKS: [Option<Task>; MAX_TASKS] = {
    const NONE: Option<Task> = None;
    [NONE; MAX_TASKS]
};

// Simple circular ready queue (array of TIDs)
static mut READY_QUEUE: [usize; MAX_TASKS] = [0; MAX_TASKS];
static mut READY_HEAD: usize = 0;
static mut READY_TAIL: usize = 0;
static mut READY_COUNT: usize = 0;

static CURRENT_TID: AtomicUsize = AtomicUsize::new(0);
static NEXT_TID: AtomicUsize = AtomicUsize::new(1); // TID 0 is idle
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Per-task wait state. If true, the task is blocked in sys_wait.
static mut WAIT_BLOCKED: [bool; MAX_TASKS] = [false; MAX_TASKS];
/// TID of the dead child collected for a waiting parent. 0 = none yet.
static mut WAIT_RESULT: [usize; MAX_TASKS] = [0; MAX_TASKS];
/// Per-task "reaped" flag. If true, parent has collected the exit via sys_wait (or has no parent).
static mut REAPED: [bool; MAX_TASKS] = [false; MAX_TASKS];

/// Initialize the scheduler. Creates the idle task (TID 0) which represents
/// the current execution context (kernel_main's continuation).
pub fn init() {
    unsafe {
        // TID 0 = idle task (current context, its stack/context will be saved on switch)
        TASKS[0] = Some(Task {
            tid: 0,
            state: TaskState::Running,
            context: context::CpuContext::empty(),
            kernel_stack_base: core::ptr::null_mut(), // uses boot stack
            kernel_stack_size: 0,
            priority: 255, // lowest priority
            cr3: crate::paging::read_cr3(),
            caps: crate::task::CAP_ALL,
            fds: [crate::task::FdKind::empty(); crate::task::MAX_FDS],
            pager_tid: 0,
            parent_tid: 0,
            mem_pages: 0,
            mem_limit: 0,
            uid: 0,
            gid: 0,
        });
    }
    CURRENT_TID.store(0, Ordering::SeqCst);
    INITIALIZED.store(true, Ordering::SeqCst);
}

/// Spawn a new kernel task that begins at `entry_fn`.
/// Returns the new task's TID.
pub fn spawn(entry_fn: fn()) -> usize {
    let tid = NEXT_TID.fetch_add(1, Ordering::SeqCst);
    if tid >= MAX_TASKS {
        panic!("scheduler: too many tasks");
    }

    let task = Task::new(tid, entry_fn);

    unsafe {
        TASKS[tid] = Some(task);
        enqueue(tid);
    }

    tid
}

/// Voluntary yield — put current task at back of ready queue and reschedule.
pub fn yield_now() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { schedule_inner(false) };
}

/// Mark current task as Dead and reschedule. Never returns.
pub fn exit() -> ! {
    unsafe {
        let current = CURRENT_TID.load(Ordering::SeqCst);
        if let Some(ref mut task) = TASKS[current] {
            task.state = TaskState::Dead;
            crate::ipc::clear_signal_deadline(current);
            let parent = task.parent_tid;
            // If parent is blocked in sys_wait, wake it with our TID
            if parent != 0 && WAIT_BLOCKED[parent] {
                WAIT_BLOCKED[parent] = false;
                WAIT_RESULT[parent] = current;
                REAPED[current] = true;
                unblock_task(parent);
            }
        }
        schedule_inner(false);
    }
    // Should never reach here
    loop {
        core::hint::spin_loop();
    }
}

/// Called from the PIT IRQ handler to preempt the current task.
pub fn timer_tick() {
    if !INITIALIZED.load(Ordering::SeqCst) {
        return;
    }
    unsafe { schedule_inner(true) };
}

/// Core scheduling logic.
///
/// # Safety
/// Must be called with interrupts disabled (from IRQ handler) or willing
/// to be preempted (yield_now).
unsafe fn schedule_inner(from_irq: bool) {
    let _ = from_irq;

    // Disable interrupts during scheduling
    let flags: u64;
    core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));

    let current_tid = CURRENT_TID.load(Ordering::SeqCst);

    // Put current task back in ready queue if it's still runnable
    if let Some(ref mut task) = TASKS[current_tid] {
        if task.state == TaskState::Running {
            task.state = TaskState::Ready;
            enqueue(current_tid);
        }
    }

    // Find next ready task
    let next_tid = match dequeue_ready() {
        Some(tid) => tid,
        None => {
            // No tasks ready — run idle task (TID 0)
            if current_tid == 0 {
                // Already idle, restore flags and return
                if let Some(ref mut task) = TASKS[0] {
                    task.state = TaskState::Running;
                }
                restore_flags(flags);
                return;
            }
            0
        }
    };

    if next_tid == current_tid {
        // Same task, just mark running again
        if let Some(ref mut task) = TASKS[current_tid] {
            task.state = TaskState::Running;
        }
        restore_flags(flags);
        return;
    }

    // Mark next task as running
    if let Some(ref mut task) = TASKS[next_tid] {
        task.state = TaskState::Running;
    }
    CURRENT_TID.store(next_tid, Ordering::SeqCst);

    // Switch CR3 if address spaces differ
    let old_cr3 = TASKS[current_tid].as_ref().unwrap().cr3;
    let new_cr3 = TASKS[next_tid].as_ref().unwrap().cr3;
    if new_cr3 != 0 && new_cr3 != old_cr3 {
        crate::paging::write_cr3(new_cr3);
    }

    // Update kernel RSP for syscall re-entry and TSS RSP0 for exceptions
    let new_task = TASKS[next_tid].as_ref().unwrap();
    if !new_task.kernel_stack_base.is_null() {
        let kernel_stack_top =
            new_task.kernel_stack_base as u64 + new_task.kernel_stack_size as u64;
        crate::syscall::update_kernel_rsp(kernel_stack_top);
        unsafe { crate::idt::update_tss_rsp0(kernel_stack_top); }
    }

    // Get raw pointers to contexts
    let old_ctx = &raw mut TASKS[current_tid].as_mut().unwrap().context;
    let new_ctx = &raw const TASKS[next_tid].as_ref().unwrap().context;

    // Restore interrupt flag before switching (new task may need interrupts)
    restore_flags(flags);

    // Perform the context switch
    context::context_switch(old_ctx, new_ctx);
}

/// Dequeue the next ready task from the ready queue.
unsafe fn dequeue_ready() -> Option<usize> {
    while READY_COUNT > 0 {
        let tid = READY_QUEUE[READY_HEAD];
        READY_HEAD = (READY_HEAD + 1) % MAX_TASKS;
        READY_COUNT -= 1;

        // Skip dead/blocked tasks that may still be in the queue
        if let Some(ref task) = TASKS[tid] {
            if task.state == TaskState::Ready {
                return Some(tid);
            }
        }
    }
    None
}

/// Add a task TID to the back of the ready queue.
unsafe fn enqueue(tid: usize) {
    if READY_COUNT >= MAX_TASKS {
        crate::console::puts(b"scheduler: ready queue full, dropping task\n");
        return;
    }
    READY_QUEUE[READY_TAIL] = tid;
    READY_TAIL = (READY_TAIL + 1) % MAX_TASKS;
    READY_COUNT += 1;
}

/// Restore interrupt flag from saved RFLAGS.
unsafe fn restore_flags(flags: u64) {
    if flags & (1 << 9) != 0 {
        core::arch::asm!("sti", options(nostack, nomem));
    }
}

/// Get the current task's TID.
pub fn current_tid() -> usize {
    CURRENT_TID.load(Ordering::SeqCst)
}

/// Mark a task as blocked. Used by IPC.
pub fn block_task(tid: usize) {
    unsafe {
        if let Some(ref mut task) = TASKS[tid] {
            task.state = TaskState::Blocked;
        }
    }
}

/// Unblock a task and put it back in the ready queue. Used by IPC.
pub fn unblock_task(tid: usize) {
    unsafe {
        if let Some(ref mut task) = TASKS[tid] {
            if task.state == TaskState::Blocked {
                task.state = TaskState::Ready;
                enqueue(tid);
            }
        }
    }
}

/// Block the current task until a child exits. Returns the dead child's TID.
/// Returns u64::MAX if the caller has no children.
pub fn sys_wait() -> u64 {
    let parent = current_tid();

    unsafe {
        // Check if any child is already dead (zombie) and not yet reaped
        for i in 1..MAX_TASKS {
            if let Some(ref task) = TASKS[i] {
                if task.parent_tid == parent && task.state == TaskState::Dead && !REAPED[i] {
                    REAPED[i] = true;
                    return i as u64;
                }
            }
        }

        // Check if we have any living children at all
        let has_children = (1..MAX_TASKS).any(|i| {
            TASKS[i].as_ref().is_some_and(|t| t.parent_tid == parent)
        });
        if !has_children {
            return u64::MAX;
        }

        // Block until a child exits
        WAIT_BLOCKED[parent] = true;
        WAIT_RESULT[parent] = 0;
        block_task(parent);
        yield_now();

        // Woken up — WAIT_RESULT has the dead child's TID
        let child_tid = WAIT_RESULT[parent];
        WAIT_RESULT[parent] = 0;
        if child_tid != 0 {
            child_tid as u64
        } else {
            u64::MAX
        }
    }
}

/// Get a mutable reference to a task by TID.
///
/// # Safety
/// Caller must ensure no aliasing.
pub unsafe fn get_task_mut(tid: usize) -> Option<&'static mut Task> {
    if tid < MAX_TASKS {
        TASKS[tid].as_mut()
    } else {
        None
    }
}

/// Kill a task by TID. Marks it Dead and wakes its parent if waiting.
/// Cannot kill TID 0 (idle) or TID 1 (init).
pub fn kill_task(tid: usize) -> Result<(), ()> {
    if tid <= 1 || tid >= MAX_TASKS {
        return Err(());
    }
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) if task.state != TaskState::Dead => {
                task.state = TaskState::Dead;
                crate::ipc::clear_signal_deadline(tid);
                let parent = task.parent_tid;
                if parent != 0 && WAIT_BLOCKED[parent] {
                    WAIT_BLOCKED[parent] = false;
                    WAIT_RESULT[parent] = tid;
                    REAPED[tid] = true;
                    unblock_task(parent);
                }
                Ok(())
            }
            _ => Err(()),
        }
    }
}

/// Get task info for enumeration. Returns (state, uid, gid, parent_tid) or None.
pub fn task_info(tid: usize) -> Option<(TaskState, u32, u32, usize)> {
    if tid >= MAX_TASKS { return None; }
    unsafe {
        TASKS[tid].as_ref().map(|t| (t.state, t.uid, t.gid, t.parent_tid))
    }
}

/// Reap dead tasks (clean up IPC, IRQs, address space, and free stacks).
/// Only reaps tasks that have been collected by sys_wait, have no parent,
/// or whose parent is already gone.
pub fn reap_dead() {
    unsafe {
        for i in 1..MAX_TASKS {
            if let Some(ref mut task) = TASKS[i] {
                if task.state == TaskState::Dead {
                    let parent = task.parent_tid;
                    // Only reap if: already collected, no parent, or parent is gone
                    let can_reap = REAPED[i]
                        || parent == 0
                        || TASKS[parent].is_none();
                    if !can_reap {
                        continue;
                    }
                    // Clean up pipe refcounts and wake blocked tasks
                    crate::pipe::cleanup_task_fds(&task.fds);
                    // Clean up IPC state and unblock tasks waiting on this one
                    crate::ipc::cleanup_task_ipc(i);
                    // Unregister any IRQ handlers
                    crate::irq_dispatch::unregister_task_irqs(i);
                    // Clean up futex waiters
                    crate::futex::cleanup_task(i);
                    // Clean up shared memory regions created by this task
                    crate::shmem::cleanup_task(i);
                    // Destroy user address space
                    let cr3 = task.cr3;
                    if cr3 != 0 && cr3 != crate::paging::kernel_cr3() {
                        crate::paging::destroy_address_space(cr3);
                    }
                    task.free_stack();
                    REAPED[i] = false;
                    // Clean up wait state if this task was a parent
                    WAIT_BLOCKED[i] = false;
                    WAIT_RESULT[i] = 0;
                    TASKS[i] = None;
                }
            }
        }
    }
}

/// Get the current task's CR3 (address space).
pub fn current_task_cr3() -> usize {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.cr3,
            None => 0,
        }
    }
}

/// Get the current task's capability bits.
pub fn current_task_caps() -> u32 {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.caps,
            None => 0,
        }
    }
}

/// Check if the current task has a given capability.
pub fn current_task_has_cap(cap: u32) -> bool {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.caps & cap != 0,
            None => false,
        }
    }
}

/// Get the current task's UID.
pub fn current_task_uid() -> u32 {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.uid,
            None => 0,
        }
    }
}

/// Get the current task's GID.
pub fn current_task_gid() -> u32 {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.gid,
            None => 0,
        }
    }
}

/// Get a task's UID and GID by TID.
pub fn task_uid_gid(tid: usize) -> Result<(u32, u32), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => Ok((task.uid, task.gid)),
            None => Err(()),
        }
    }
}

/// Set a task's UID.
pub fn set_task_uid(tid: usize, uid: u32) -> Result<(), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => { task.uid = uid; Ok(()) }
            None => Err(()),
        }
    }
}

/// Set a task's GID.
pub fn set_task_gid(tid: usize, gid: u32) -> Result<(), ()> {
    if tid >= MAX_TASKS { return Err(()); }
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => { task.gid = gid; Ok(()) }
            None => Err(()),
        }
    }
}

/// Create an empty task slot (Blocked, cr3=0, caps=0). Returns TID.
pub fn create_empty_task() -> Option<usize> {
    let tid = NEXT_TID.fetch_add(1, Ordering::SeqCst);
    if tid >= MAX_TASKS {
        return None;
    }

    let layout = core::alloc::Layout::from_size_align(KERNEL_STACK_SIZE, 16)
        .expect("scheduler: invalid stack layout");
    let stack_base = unsafe { alloc::alloc::alloc(layout) };
    if stack_base.is_null() {
        return None;
    }

    let parent = current_tid();
    let (parent_uid, parent_gid) = unsafe {
        match TASKS[parent].as_ref() {
            Some(t) => (t.uid, t.gid),
            None => (0, 0),
        }
    };
    unsafe {
        TASKS[tid] = Some(Task {
            tid,
            state: TaskState::Blocked,
            context: context::CpuContext::empty(),
            kernel_stack_base: stack_base,
            kernel_stack_size: KERNEL_STACK_SIZE,
            priority: 0,
            cr3: 0,
            caps: 0,
            fds: [crate::task::FdKind::empty(); crate::task::MAX_FDS],
            pager_tid: 0,
            parent_tid: parent,
            mem_pages: 0,
            mem_limit: 0,
            uid: parent_uid,
            gid: parent_gid,
        });
    }

    Some(tid)
}

/// Configure and start a previously created empty task for userspace entry.
pub fn start_task(tid: usize, rip: u64, rsp: u64, cr3: usize) -> Result<(), ()> {
    unsafe {
        let task = match TASKS[tid].as_mut() {
            Some(t) => t,
            None => return Err(()),
        };
        if task.state != TaskState::Blocked {
            return Err(());
        }

        task.cr3 = cr3;

        // Set up context so context_switch enters enter_user_trampoline
        // with r12=rip, r13=rsp, r14=cr3
        let stack_top = task.kernel_stack_base as usize + task.kernel_stack_size;
        let stack_top = stack_top & !0xF;

        let trampoline_addr =
            crate::userspace::enter_user_trampoline as *const () as usize as u64;

        // Write trampoline return address on kernel stack
        let sp = stack_top as *mut u64;
        core::ptr::write(sp.sub(1), crate::task::task_exit_trampoline as *const () as u64);

        task.context.rip = trampoline_addr;
        task.context.rsp = (stack_top - 8) as u64;
        task.context.rbp = 0;
        task.context.r12 = rip;
        task.context.r13 = rsp;
        task.context.r14 = cr3 as u64;

        task.state = TaskState::Ready;
        enqueue(tid);
        Ok(())
    }
}

/// Grant a capability to a task.
pub fn grant_cap(tid: usize, cap: u32) -> Result<(), ()> {
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => {
                task.caps |= cap;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Set a file descriptor entry on a task.
pub fn set_fd(tid: usize, fd: usize, entry: crate::task::FdKind) -> Result<(), ()> {
    if fd >= crate::task::MAX_FDS {
        return Err(());
    }
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => {
                task.fds[fd] = entry;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Set the pager task for a given task.
pub fn set_pager(tid: usize, pager_tid: usize) -> Result<(), ()> {
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => {
                task.pager_tid = pager_tid;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Get the pager TID for the current task. Returns 0 if no pager.
pub fn current_task_pager() -> usize {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => task.pager_tid,
            None => 0,
        }
    }
}

/// Check if the current task can allocate `pages` more pages.
/// Returns false if the allocation would exceed the task's memory limit.
pub fn current_task_check_mem(pages: usize) -> bool {
    let tid = current_tid();
    unsafe {
        match TASKS[tid].as_ref() {
            Some(task) => {
                if task.mem_limit == 0 {
                    true // unlimited
                } else {
                    task.mem_pages + pages <= task.mem_limit
                }
            }
            None => false,
        }
    }
}

/// Add `pages` to the current task's memory usage counter.
pub fn current_task_charge_mem(pages: usize) {
    let tid = current_tid();
    unsafe {
        if let Some(ref mut task) = TASKS[tid] {
            task.mem_pages += pages;
        }
    }
}

/// Set the memory limit (in pages) for a task. 0 = unlimited.
pub fn set_mem_limit(tid: usize, limit: usize) -> Result<(), ()> {
    unsafe {
        match TASKS[tid].as_mut() {
            Some(task) => {
                task.mem_limit = limit;
                Ok(())
            }
            None => Err(()),
        }
    }
}

/// Get a file descriptor entry for the current task.
pub fn current_fd(fd: usize) -> crate::task::FdKind {
    if fd >= crate::task::MAX_FDS {
        return crate::task::FdKind::Empty;
    }
    unsafe {
        let tid = CURRENT_TID.load(Ordering::SeqCst);
        match TASKS[tid].as_ref() {
            Some(task) => task.fds[fd],
            None => crate::task::FdKind::Empty,
        }
    }
}
