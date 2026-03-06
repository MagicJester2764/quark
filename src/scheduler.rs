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
            fds: [crate::task::FdEntry::empty(); crate::task::MAX_FDS],
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

/// Reap dead tasks (clean up IPC, IRQs, address space, and free stacks).
pub fn reap_dead() {
    unsafe {
        for i in 1..MAX_TASKS {
            if let Some(ref mut task) = TASKS[i] {
                if task.state == TaskState::Dead {
                    // Clean up IPC state and unblock tasks waiting on this one
                    crate::ipc::cleanup_task_ipc(i);
                    // Unregister any IRQ handlers
                    crate::irq_dispatch::unregister_task_irqs(i);
                    // Destroy user address space
                    let cr3 = task.cr3;
                    if cr3 != 0 && cr3 != crate::paging::kernel_cr3() {
                        crate::paging::destroy_address_space(cr3);
                    }
                    task.free_stack();
                    TASKS[i] = None;
                }
            }
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
            fds: [crate::task::FdEntry::empty(); crate::task::MAX_FDS],
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
pub fn set_fd(tid: usize, fd: usize, entry: crate::task::FdEntry) -> Result<(), ()> {
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

/// Get a file descriptor entry for the current task.
pub fn current_fd(fd: usize) -> Option<crate::task::FdEntry> {
    if fd >= crate::task::MAX_FDS {
        return None;
    }
    unsafe {
        let tid = CURRENT_TID.load(Ordering::SeqCst);
        match TASKS[tid].as_ref() {
            Some(task) => {
                let entry = task.fds[fd];
                if entry.target_tid == 0 {
                    None
                } else {
                    Some(entry)
                }
            }
            None => None,
        }
    }
}
