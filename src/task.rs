/// Task abstraction for the Quark microkernel.
///
/// Each task has a unique TID, its own kernel stack, and saved CPU context.

use crate::context::CpuContext;
use alloc::alloc::{alloc, dealloc, Layout};

pub const MAX_TASKS: usize = 64;
pub const KERNEL_STACK_SIZE: usize = 16384; // 16 KiB per task
const STACK_ALIGN: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
    Dead,
}

#[repr(C)]
pub struct Task {
    pub tid: usize,
    pub state: TaskState,
    pub context: CpuContext,
    pub kernel_stack_base: *mut u8,
    pub kernel_stack_size: usize,
    pub priority: u8,
}

unsafe impl Send for Task {}

impl Task {
    /// Create a new task that will start executing at `entry_fn`.
    ///
    /// Allocates a kernel stack from the heap and sets up the initial context
    /// so that the first `context_switch` into this task "returns" into `entry_fn`.
    pub fn new(tid: usize, entry_fn: fn()) -> Self {
        let layout = Layout::from_size_align(KERNEL_STACK_SIZE, STACK_ALIGN)
            .expect("task: invalid stack layout");
        let stack_base = unsafe { alloc(layout) };
        if stack_base.is_null() {
            panic!("task: failed to allocate kernel stack");
        }

        // Stack grows downward: top = base + size
        let stack_top = stack_base as usize + KERNEL_STACK_SIZE;
        // Align stack top to 16 bytes (should already be, but be safe)
        let stack_top = stack_top & !0xF;

        // Set up initial context so context_switch "returns" into entry_fn.
        // We push a trampoline address as the return address on the stack.
        // The trampoline will call the entry function and then call task_exit.
        //
        // Stack layout (growing down):
        //   [stack_top - 8]  = task_exit_trampoline (return address for entry_fn)
        //   [stack_top - 16] = entry_fn (return address for context_switch ret)
        let entry_addr = entry_fn as usize as u64;
        let trampoline_addr = task_exit_trampoline as *const () as usize as u64;

        unsafe {
            let sp = stack_top as *mut u64;
            // The entry_fn will "ret" into the trampoline when it returns
            core::ptr::write(sp.sub(1), trampoline_addr);
            // context_switch does "push [new.rip]; ret" which jumps to entry_fn
        }

        let mut ctx = CpuContext::empty();
        ctx.rsp = (stack_top - 8) as u64; // points at trampoline return addr
        ctx.rip = entry_addr;
        ctx.rbp = 0;

        Task {
            tid,
            state: TaskState::Ready,
            context: ctx,
            kernel_stack_base: stack_base,
            kernel_stack_size: KERNEL_STACK_SIZE,
            priority: 0,
        }
    }

    /// Free this task's kernel stack.
    ///
    /// # Safety
    /// Must not be called while this task is running or its stack is in use.
    pub unsafe fn free_stack(&mut self) {
        if !self.kernel_stack_base.is_null() {
            let layout = Layout::from_size_align(self.kernel_stack_size, STACK_ALIGN)
                .expect("task: invalid stack layout");
            dealloc(self.kernel_stack_base, layout);
            self.kernel_stack_base = core::ptr::null_mut();
        }
    }
}

/// Trampoline that runs when a task function returns.
/// Marks the task as dead and yields to the scheduler.
fn task_exit_trampoline() {
    crate::scheduler::exit();
}
