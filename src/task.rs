/// Task abstraction for the Quark microkernel.
///
/// Each task has a unique TID, its own kernel stack, and saved CPU context.


use crate::context::CpuContext;

/// How many tasks there can be: as many as a table has slots (`table.rs`).
/// A task's record is made when it is, so this is a ceiling and not a cost.
/// It was sixty-four, a desktop's limit and a fixed array's.
pub const MAX_TASKS: usize = crate::table::MOST;
/// A task's kernel stack (`kstack.rs`): with a page below it that faults.
pub const KERNEL_STACK_SIZE: usize = crate::kstack::KSTACK_SIZE;
/// The most descriptors a program can have: the highest its limit goes.
///
/// Eight was three spoken for and five left; thirty-two was enough until files
/// became descriptors too; sixty-four was a table of a fixed size for every
/// program, spent whether or not it was used and too small for a desktop's.
/// A program's table grows now (`fdtable.rs`), to the limit the program has —
/// [`FD_SOFT`] to start, raised by the program as far as this.
pub const FD_MOST: usize = 65_536;
/// The limit a program starts with: Linux's, and what `RLIMIT_NOFILE` says.
pub const FD_SOFT: usize = 1_024;
/// How many groups a task may be in besides its own.
pub const MAX_GROUPS: usize = 16;

/// What the syscall entry stub pushed, read back as a structure.
///
/// The stub is the definition: it sets RSP to the task's kernel stack top and
/// pushes these eleven words, so a task that is inside a system call has them
/// at `kernel_stack_base + kernel_stack_size - size_of::<UserFrame>()`. That
/// is how `fork` gets the whole of a caller's register state without the stub
/// having to record anything — the position is a consequence of the pushes,
/// and the assertion below is what keeps the two lists equal.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct UserFrame {
    pub rsi: u64,
    pub rdi: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

const _: () = assert!(core::mem::size_of::<UserFrame>() == 88);

/// File descriptor kind — routes I/O to either an IPC service or a kernel pipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdKind {
    Empty,
    /// A server's: a read or a write is a call to it with `tag`. The task is
    /// named by its endpoint number as well as its TID, as an `Endpoint`
    /// capability names it: a TID is the next task's once this one is
    /// reaped, and a descriptor to a server that had died wrote to whatever
    /// was started next.
    Ipc { target_tid: usize, endpoint: u64, tag: u64 },
    PipeRead(usize),   // pipe handle index
    PipeWrite(usize),  // pipe handle index
    /// Shared memory, named by a descriptor so that it can be passed across a
    /// stream, inherited, and closed like anything else a program holds.
    MemFd { handle: usize },
    /// One end of a connected pair. `end` is 0 or 1.
    StreamEnd { stream: usize, end: u8 },
    /// A set of descriptors to wait on. A set is a descriptor itself, so it
    /// can be held, closed and passed like any other.
    PollSet { set: usize },
    /// One end of a pseudo-terminal: `end` is 0 for the master, 1 for the
    /// slave. The master is held by whatever draws the terminal and the slave
    /// is the program in it — its standard input, output and error.
    PtyEnd { pty: usize, end: u8 },
    /// A timer: readable once its deadline has passed, and read as the count
    /// of times it has. A program's event loop waits on it with everything
    /// else it waits on.
    Timer { timer: usize },
    /// A counter one task adds to and another waits on: `eventfd`. The wake-up
    /// every main loop is built out of, in one descriptor rather than a pipe's
    /// two.
    Event { ev: usize },
    /// An object in a server — a file, most often — named by a number the
    /// server chose. See `served.rs`.
    Served { obj: usize },
    /// Signals to be read, by whoever reads: `signalfd`. See `sigfd.rs`.
    Signals { sfd: usize },
    /// A local socket before it is connected: nothing yet, named, or
    /// listening. See `local.rs`; connected, it is a `StreamEnd`.
    Local { l: usize },
    /// A network connection, held by the net server as `handle`.
    ///
    /// Unlike `Ipc`, which is one-directional and carries a fixed tag, a
    /// socket is read and written through the same descriptor, so the tag is
    /// chosen per direction and the handle travels in its upper bits. The
    /// server is named by its endpoint number too, as `Ipc`'s is.
    Socket { net_tid: usize, endpoint: u64, handle: usize },
}

impl FdKind {
    pub const fn empty() -> Self {
        FdKind::Empty
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, FdKind::Empty)
    }
}

// Capability bits
pub const CAP_IOPORT: u32 = 1 << 0;
pub const CAP_MAP_PHYS: u32 = 1 << 1;
pub const CAP_IRQ: u32 = 1 << 2;
pub const CAP_TASK_MGMT: u32 = 1 << 3;
pub const CAP_PHYS_ALLOC: u32 = 1 << 4;
pub const CAP_SET_UID: u32 = 1 << 5;
/// Once permission to originate IPC to anybody. Confers nothing since ABI 2.0:
/// an `Endpoint` names one task, and is minted for it.
pub const CAP_ENDPOINT: u32 = 1 << 6;
pub const CAP_ALL: u32 = CAP_IOPORT
    | CAP_MAP_PHYS
    | CAP_IRQ
    | CAP_TASK_MGMT
    | CAP_PHYS_ALLOC
    | CAP_SET_UID
    | CAP_ENDPOINT;

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
    /// The band the scheduler actually uses. Normally `base_priority`, but
    /// raised while a task in a better band is blocked waiting on this one.
    pub priority: u8,
    /// The band this task was given, and the one it returns to.
    pub base_priority: u8,
    pub cr3: usize,
    /// The program this task belongs to: its address space's id, set when the
    /// task is made to run there (or made for it), and never changed. 0 for a
    /// kernel task and for one not yet given an address space.
    pub space: u64,
    /// Pager task TID for exception forwarding. 0 = no pager (kill on fault).
    pub pager_tid: usize,
    /// Parent task TID. 0 = no parent (init/kernel tasks).
    pub parent_tid: usize,
    /// Number of physical pages allocated by this task (via sys_mmap / sys_phys_alloc).
    pub mem_pages: usize,
    /// Maximum pages this task may allocate. 0 = unlimited.
    pub mem_limit: usize,
    /// Exit status reported to a parent waiting in sys_wait.
    /// Negative values indicate abnormal termination (killed/signalled).
    pub exit_code: i32,
    /// FS segment base, restored on every switch into this task.
    ///
    /// Threads share an address space, so a thread-local needs a per-task
    /// register to distinguish one thread's copy from another's. FS is that
    /// register: `thread_local!` compiles to an offset from it.
    pub fs_base: u64,
    /// A word in this task's address space to clear and wake when it exits.
    ///
    /// Linux calls this `CLONE_CHILD_CLEARTID`, and musl does not treat it as
    /// optional: its `pthread_exit` takes the thread-list lock and never
    /// unlocks it, because the lock *is* this word and the kernel releasing it
    /// is what publishes the thread's removal from the list. Without this, a
    /// thread that exits leaves the list locked by a dead task and the next
    /// `pthread_join` waits for ever.
    pub clear_child_tid: u64,
    /// User ID. 0 = root.
    pub uid: u32,
    /// Group ID. 0 = root.
    pub gid: u32,
    /// The groups it is in besides `gid`, and how many of them there are.
    /// A file's group may be any of these: what a file server checks.
    pub groups: [u32; MAX_GROUPS],
    pub ngroups: u8,
    /// This task's floating-point and SSE registers while it is not running.
    /// See `fpu.rs` for why this exists and what decides how much of it is used.
    pub fpu: crate::fpu::FpuState,
}

unsafe impl Send for Task {}

impl Task {
    /// A task that is nothing yet: what a new record starts as (`TaskRec`'s
    /// template), every field that is the task's made afterwards.
    pub const EMPTY: Task = Task {
        tid: 0,
        state: TaskState::Blocked,
        context: CpuContext::empty(),
        kernel_stack_base: core::ptr::null_mut(),
        kernel_stack_size: 0,
        priority: crate::scheduler::PRIO_NORMAL,
        base_priority: crate::scheduler::PRIO_NORMAL,
        cr3: 0,
        space: 0,
        pager_tid: 0,
        parent_tid: 0,
        mem_pages: 0,
        mem_limit: 0,
        exit_code: 0,
        fs_base: 0,
        clear_child_tid: 0,
        uid: 0,
        gid: 0,
        groups: [0; MAX_GROUPS],
        ngroups: 0,
        fpu: crate::fpu::ZERO,
    };

    /// Create a new task that will start executing at `entry_fn`.
    ///
    /// Takes a kernel stack (`kstack.rs`) and sets up the initial context
    /// so that the first `context_switch` into this task "returns" into `entry_fn`.
    pub fn new(tid: usize, entry_fn: fn()) -> Self {
        let Some((base, _)) = crate::kstack::alloc() else {
            panic!("task: no kernel stack to be had");
        };
        let stack_base = base as *mut u8;

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
            priority: crate::scheduler::PRIO_NORMAL,
            base_priority: crate::scheduler::PRIO_NORMAL,
            cr3: crate::paging::read_cr3(),
            space: 0,
            pager_tid: 0,
            parent_tid: 0,
            mem_pages: 0,
            mem_limit: 0,
            exit_code: 0,
            fs_base: 0,
            clear_child_tid: 0,
            uid: 0,
            gid: 0,
            groups: [0; MAX_GROUPS],
            ngroups: 0,
            fpu: crate::fpu::clean(),
        }
    }

    /// Free this task's kernel stack.
    ///
    /// # Safety
    /// Must not be called while this task is running or its stack is in use.
    pub unsafe fn free_stack(&mut self) {
        if !self.kernel_stack_base.is_null() {
            crate::kstack::free(self.kernel_stack_base as usize);
            self.kernel_stack_base = core::ptr::null_mut();
        }
    }
}

/// Everything kept about one task: the task itself, and each module's own part
/// of it, in one record the task table makes when the task is made and gives
/// back when it is taken apart (`scheduler.rs`, `table.rs`). It was an array of
/// sixty-four in each module, every slot spent whether or not a task was in
/// it; a slot with no task has no record now, and nothing in it to zero.
///
/// A record reads as its task (`Deref`), so code that had the task has it.
pub struct TaskRec {
    pub task: Task,
    pub sched: crate::scheduler::PerTask,
    pub ipc: crate::ipc::PerTask,
    pub sig: crate::signal::PerTask,
    pub job: crate::job::PerTask,
    pub fd: crate::fdtable::PerTask,
    pub usage: crate::usage::PerTask,
    pub threads: crate::threads::PerTask,
    pub served: crate::served::PerTask,
    pub cap: crate::cap::PerTask,
    pub pmm: crate::pmm::PerTask,
    /// What it waits on, and the tasks either side of it there.
    pub wait: crate::waitlist::WaitLink,
    /// The futex word it waits on, if one, and its place on that word's list.
    pub futex: crate::futex::PerTask,
}

impl TaskRec {
    /// A record of nothing yet: what a task's is copied from
    /// (`scheduler::TASK_TEMPLATE`).
    pub const fn empty() -> Self {
        TaskRec {
            task: Task::EMPTY,
            sched: crate::scheduler::PerTask::new(),
            ipc: crate::ipc::PerTask::new(),
            sig: crate::signal::PerTask::new(),
            job: crate::job::PerTask::new(),
            fd: crate::fdtable::PerTask::new(),
            usage: crate::usage::PerTask::new(),
            threads: crate::threads::PerTask::new(),
            served: crate::served::PerTask::new(),
            cap: crate::cap::PerTask::new(),
            pmm: crate::pmm::PerTask::new(),
            wait: crate::waitlist::WaitLink::NONE,
            futex: crate::futex::PerTask::new(),
        }
    }

    pub fn new(task: Task) -> Self {
        TaskRec {
            task,
            sched: crate::scheduler::PerTask::new(),
            ipc: crate::ipc::PerTask::new(),
            sig: crate::signal::PerTask::new(),
            job: crate::job::PerTask::new(),
            fd: crate::fdtable::PerTask::new(),
            usage: crate::usage::PerTask::new(),
            threads: crate::threads::PerTask::new(),
            served: crate::served::PerTask::new(),
            cap: crate::cap::PerTask::new(),
            pmm: crate::pmm::PerTask::new(),
            wait: crate::waitlist::WaitLink::NONE,
            futex: crate::futex::PerTask::new(),
        }
    }
}

impl core::ops::Deref for TaskRec {
    type Target = Task;
    fn deref(&self) -> &Task {
        &self.task
    }
}

impl core::ops::DerefMut for TaskRec {
    fn deref_mut(&mut self) -> &mut Task {
        &mut self.task
    }
}

/// Trampoline that runs when a task function returns.
/// Marks the task as dead and yields to the scheduler.
pub fn task_exit_trampoline() {
    crate::scheduler::exit();
}
