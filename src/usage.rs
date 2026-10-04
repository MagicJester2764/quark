//! What a task, and a program, has used of the machine: how long it has run,
//! how much of that was in the program and how much in the kernel on its
//! behalf, and how often it gave the processor up or had it taken away
//! (`SYS_USAGE`). And two things a program may say about its use: how nice it
//! is to the rest of its band (`SYS_NICE`), and how long it may run at all
//! (`SYS_CPU_LIMIT`).
//!
//! Time is counted whenever the scheduler decides anything, by the clock
//! (`charge`): exactly, whatever the tick. So is the part of it spent in the
//! kernel: each door from ring 3 says when a task came in (`entered`) and
//! each way back when it left (`leaving`), and a switch counts what a task
//! in the kernel had of it so far. The program's part is the rest. It was
//! sampled once — each tick looking at where it found the task, as Linux
//! does when it is not told more — and a call spends most of its time with
//! interrupts off, so a tick that fell in it was taken after `sysretq`, in
//! the program: a loop of calls was said to be four-fifths the program's on
//! one processor model and a seventh on another. Counted, neither part can
//! go back.
//!
//! A program's use is its tasks': what those that have ended used, kept with
//! the program (`fdtable`), and what those still here have. A program that
//! ends leaves its use, and that of the children it collected, for whoever
//! collects it (`ENDED`), and that is what a parent is told its children
//! used. Only time already divided is ever added up.

use crate::percpu::MAX_CPUS;
use crate::task::MAX_TASKS;

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

/// What was used: nanoseconds in the program and in the kernel for it, and
/// the times it gave the processor up — to wait — and had it taken.
#[derive(Clone, Copy)]
pub struct Usage {
    pub user_ns: u64,
    pub sys_ns: u64,
    pub voluntary: u64,
    pub involuntary: u64,
}

impl Usage {
    pub const ZERO: Usage = Usage { user_ns: 0, sys_ns: 0, voluntary: 0, involuntary: 0 };

    pub fn add(&mut self, other: &Usage) {
        self.user_ns += other.user_ns;
        self.sys_ns += other.sys_ns;
        self.voluntary += other.voluntary;
        self.involuntary += other.involuntary;
    }

    pub fn total_ns(&self) -> u64 {
        self.user_ns + self.sys_ns
    }
}

/// What one task has used, as counted.
#[derive(Clone, Copy)]
struct Raw {
    /// Nanoseconds run, to its last switch, and of them, in the kernel.
    run_ns: u64,
    sys_ns: u64,
    voluntary: u64,
    involuntary: u64,
}

impl Raw {
    const ZERO: Raw = Raw { run_ns: 0, sys_ns: 0, voluntary: 0, involuntary: 0 };
}

static mut TASK: [Raw; MAX_TASKS] = [Raw::ZERO; MAX_TASKS];
/// Whether each task is in the kernel, and since when on this turn. A task
/// is made in the kernel, and first leaves for its program.
static mut IN_KERNEL: [bool; MAX_TASKS] = [true; MAX_TASKS];
static mut KERNEL_SINCE: [u64; MAX_TASKS] = [0; MAX_TASKS];
/// What a program's last task leaves for whoever collects it: what the
/// program used, and what the children it collected did.
static mut ENDED: [Usage; MAX_TASKS] = [Usage::ZERO; MAX_TASKS];
/// When each processor began running what it is running.
static mut SINCE: [u64; MAX_CPUS] = [0; MAX_CPUS];

/// A task has been made: it has used nothing.
pub fn task_made(tid: usize) {
    if tid < MAX_TASKS {
        let flags = irq_save();
        unsafe {
            TASK[tid] = Raw::ZERO;
            ENDED[tid] = Usage::ZERO;
            IN_KERNEL[tid] = true;
        }
        irq_restore(flags);
    }
}

/// The turn `tid` is having on this processor, counted to now: what it has
/// run since it was last counted, added to what it has used, and returned.
/// The idle loop's is nobody's.
///
/// # Safety
/// Interrupts off, and `tid` is what this processor is running.
pub unsafe fn charge(tid: usize) -> u64 {
    unsafe {
        let now = crate::clock::now();
        let cpu = crate::percpu::index();
        let ran = now.saturating_sub(SINCE[cpu]);
        SINCE[cpu] = now;
        if tid == 0 || tid >= MAX_TASKS {
            return 0;
        }
        TASK[tid].run_ns += ran;
        if IN_KERNEL[tid] {
            TASK[tid].sys_ns += now.saturating_sub(KERNEL_SINCE[tid]);
            KERNEL_SINCE[tid] = now;
        }
        ran
    }
}

/// `tid` is about to run on this processor, its last turn counted: if it
/// is in the kernel, its time there goes on from now.
///
/// # Safety
/// Interrupts off, after [`charge`] for whatever this processor ran.
pub unsafe fn resumed(tid: usize) {
    unsafe {
        if tid != 0 && tid < MAX_TASKS && IN_KERNEL[tid] {
            KERNEL_SINCE[tid] = SINCE[crate::percpu::index()];
        }
    }
}

/// Task `tid`, running here, has come into the kernel from its program:
/// at a system call, an interrupt or a fault taken in ring 3. Every system
/// call pays for this and [`leaving`], so neither saves the flags nor keeps
/// the clock from going back between processors: both are called at the
/// doors, where interrupts are off already.
///
/// # Safety
/// Interrupts off.
pub unsafe fn entered(tid: usize) {
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    unsafe {
        IN_KERNEL[tid] = true;
        KERNEL_SINCE[tid] = crate::clock::now_here();
    }
}

/// Task `tid`, running here, is going back to its program: what it has had
/// in the kernel since it came in is counted.
///
/// # Safety
/// Interrupts off.
pub unsafe fn leaving(tid: usize) {
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    unsafe {
        if IN_KERNEL[tid] {
            TASK[tid].sys_ns += crate::clock::now_here().saturating_sub(KERNEL_SINCE[tid]);
            IN_KERNEL[tid] = false;
        }
    }
}

/// This processor is leaving `from` for another task, its turn counted:
/// given up if it is leaving because it is waiting, taken if not.
///
/// # Safety
/// Interrupts off.
pub unsafe fn switched(from: usize, gave_up: bool) {
    unsafe {
        if from != 0 && from < MAX_TASKS {
            if gave_up {
                TASK[from].voluntary += 1;
            } else {
                TASK[from].involuntary += 1;
            }
        }
    }
}

/// What task `tid` has used, up to now: the turn it is having included.
pub fn of_task(tid: usize) -> Usage {
    if tid >= MAX_TASKS {
        return Usage::ZERO;
    }
    let flags = irq_save();
    let used = unsafe {
        let raw = TASK[tid];
        let (mut run, mut sys) = (raw.run_ns, raw.sys_ns);
        if let Some(cpu) = crate::scheduler::running_on(tid) {
            let now = crate::clock::now();
            run += now.saturating_sub(SINCE[cpu]);
            if IN_KERNEL[tid] {
                sys += now.saturating_sub(KERNEL_SINCE[tid]);
            }
        }
        // The kernel's part is part of what it ran, counted from the same
        // moments; the program's is the rest.
        let sys = sys.min(run);
        Usage { user_ns: run - sys, sys_ns: sys, voluntary: raw.voluntary, involuntary: raw.involuntary }
    };
    irq_restore(flags);
    used
}

/// What the program `tid` is a task of has used: its tasks that have ended,
/// and those still here.
pub fn of_program(tid: usize) -> Usage {
    let mut used = crate::fdtable::usage_gone(tid);
    let mut tasks = [0usize; MAX_TASKS];
    let n = crate::fdtable::tasks_of(tid, &mut tasks);
    for &t in &tasks[..n] {
        used.add(&of_task(t));
    }
    used
}

/// What the children `tid`'s program has collected used.
pub fn of_children(tid: usize) -> Usage {
    crate::fdtable::usage_children(tid)
}

/// Task `tid` has ended: what it used is its program's now. If it was the
/// program's last, what the program used — and its children — waits for
/// whoever collects it. Before it leaves its program's table.
///
/// Whoever that is collects the task the program began as, or the one that
/// took its place, and the last to end may be neither: a thread that called
/// `exit`. So it waits with every task of the process not yet taken apart;
/// only one of them is anybody's child.
pub fn task_ended(tid: usize) {
    let mut tasks = [0usize; MAX_TASKS];
    let n = crate::fdtable::tasks_of(tid, &mut tasks);
    if n == 0 {
        // Not in a program: what it used has been counted already.
        return;
    }
    crate::fdtable::usage_gone_add(tid, &of_task(tid));
    if n > 1 {
        return;
    }
    let mut all = crate::fdtable::usage_gone(tid);
    all.add(&crate::fdtable::usage_children(tid));
    let n = crate::scheduler::tasks_of_process(tid, &mut tasks);
    let flags = irq_save();
    for &t in &tasks[..n] {
        unsafe { ENDED[t] = all };
    }
    irq_restore(flags);
}

/// `parent` has collected `child`: what the child's program used, and its
/// children, is what `parent`'s program's children used.
pub fn collected(parent: usize, child: usize) {
    if child >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    let ended = unsafe { core::mem::replace(&mut ENDED[child], Usage::ZERO) };
    irq_restore(flags);
    crate::fdtable::usage_children_add(parent, &ended);
}

/// `SYS_USAGE`: what was used — 0 by the caller's program, 1 by the children
/// it has collected, 2 by the calling task, 3 by the program task `of` is a
/// task of, 4 by task `of` itself — written to `out` as four words: nanoseconds in the program,
/// nanoseconds in the kernel for it, and how many times it gave the
/// processor up and had it taken. Anybody may ask about any program, as
/// `ps` does.
pub fn usage(tid: usize, whose: u64, out: u64, of: u64) -> u64 {
    if !crate::syscall::validate_user_ptr_mut(out, 32) {
        return u64::MAX;
    }
    let used = match whose {
        0 => of_program(tid),
        1 => of_children(tid),
        2 => of_task(tid),
        3 if (of as usize) < MAX_TASKS && crate::scheduler::task_is_live(of as usize) => {
            of_program(of as usize)
        }
        // A thread's own clock, another thread's of the program asks.
        4 if (of as usize) < MAX_TASKS && crate::scheduler::task_is_live(of as usize) => of_task(of as usize),
        _ => return u64::MAX,
    };
    let words = [used.user_ns, used.sys_ns, used.voluntary, used.involuntary];
    let _ua = crate::cpu::UserAccess::begin();
    unsafe { core::ptr::write_unaligned(out as *mut [u64; 4], words) };
    0
}

/// How much of its band a program is owed, by how nice it is, against 1024
/// at nought: each step about a quarter more or less. Linux's table, so that
/// `nice` means here what it means there.
const WEIGHTS: [u64; 40] = [
    88761, 71755, 56483, 46273, 36291, // -20
    29154, 23254, 18705, 14949, 11916, // -15
    9548, 7620, 6100, 4904, 3906, // -10
    3121, 2501, 1991, 1586, 1277, // -5
    1024, 820, 655, 526, 423, // 0
    335, 272, 215, 172, 137, // 5
    110, 87, 70, 56, 45, // 10
    36, 29, 23, 18, 15, // 15
];

/// `ns` of running, as it counts against a task of a program this nice:
/// for longer the nicer it is.
pub fn weighted(ns: u64, nice: i8) -> u64 {
    ns.saturating_mul(1024) / WEIGHTS[(nice.clamp(-20, 19) + 20) as usize]
}

/// The slice, in ticks, a task of a program this nice is given when the
/// scheduler chooses it: three at nought, one from ten, twelve at -15 and
/// below. How often it is chosen is its weight's to say (`weighted`); this
/// is only how finely its share is cut.
pub fn slice_for(nice: i8) -> u32 {
    match nice {
        i8::MIN..=-15 => 12,
        -14..=-10 => 9,
        -9..=-5 => 6,
        -4..=-1 => 4,
        0..=4 => 3,
        5..=9 => 2,
        _ => 1,
    }
}

/// What a call about another program answers when it may not.
const NOT_ALLOWED: u64 = u64::MAX - 1;

/// Whether `caller` may say how `target`'s program runs: its own program, or
/// one of the same user, or with `TaskMgmt` for it.
fn may(caller: usize, target: usize) -> bool {
    crate::cap::task_has_task_mgmt(caller, target)
        || crate::scheduler::task_uid_gid(target).is_ok_and(|(uid, _)| {
            crate::scheduler::task_uid_gid(caller).is_ok_and(|(mine, _)| mine == uid)
        })
}

/// `SYS_NICE`: how nice the program of process `pid` (0 for the caller's)
/// is, as 0 to 39 for -20 to 19, set to `new` (a signed number, -20 to 19)
/// unless that is `u64::MAX`. Anybody may ask. A program may make itself, or
/// another of its user's, nicer; to be less nice takes `TaskMgmt` for the
/// program, which is what root's power to was.
pub fn nice(caller: usize, pid: u64, new: u64) -> u64 {
    let target = if pid == 0 {
        caller
    } else {
        match crate::scheduler::task_of_pid(pid) {
            Some(t) if crate::scheduler::task_is_live(t) => t,
            _ => return u64::MAX,
        }
    };
    let old = crate::fdtable::nice_of(target);
    if new != u64::MAX {
        if target != caller && !may(caller, target) {
            return NOT_ALLOWED;
        }
        let wanted = (new as i64).clamp(-20, 19) as i8;
        if wanted < old && !crate::cap::task_has_task_mgmt(caller, target) {
            return NOT_ALLOWED;
        }
        crate::fdtable::set_nice(target, wanted);
    }
    (old as i64 + 20) as u64
}

/// `SYS_CPU_LIMIT`: how long the caller's program may run, in seconds of
/// processor time: past `soft` it is sent SIGXCPU, once a second; at `hard`
/// it is ended, as SIGKILL would. `u64::MAX` is no limit. What it was is
/// written to `old`, two words, if that is not nought; with `ask`, nothing
/// changes. Anybody may lower the hard limit; raising it takes `TaskMgmt`.
pub fn cpu_limit(caller: usize, soft: u64, hard: u64, old: u64, ask: bool) -> u64 {
    if old != 0 && !crate::syscall::validate_user_ptr_mut(old, 16) {
        return u64::MAX;
    }
    let (was_soft, was_hard) = crate::fdtable::cpu_limit_of(caller);
    if !ask {
        if soft > hard {
            return u64::MAX;
        }
        if hard > was_hard && !crate::cap::task_has_task_mgmt(caller, 0) {
            return NOT_ALLOWED;
        }
        crate::fdtable::set_cpu_limit(caller, soft, hard);
    }
    if old != 0 {
        let _ua = crate::cpu::UserAccess::begin();
        unsafe { core::ptr::write_unaligned(old as *mut [u64; 2], [was_soft, was_hard]) };
    }
    0
}

const SIGXCPU: u8 = 24;
const SECOND_NS: u64 = 1_000_000_000;

/// After a tick, for the task it found: a program over its soft limit is
/// sent SIGXCPU, once a second, and one at its hard limit is ended. Last on
/// the tick's way out: ending the program may not return.
pub fn limits() {
    let tid = crate::scheduler::current_tid();
    if tid == 0 {
        return;
    }
    let (soft, hard) = crate::fdtable::cpu_limit_of(tid);
    if soft == u64::MAX && hard == u64::MAX {
        return;
    }
    // A limit of nought is one of a second, as on Linux: nought is what a
    // limit nobody set would look like, so it cannot mean "at once".
    let (soft, hard) = (soft.max(1), hard.max(1));
    let used = of_program(tid).total_ns() / SECOND_NS;
    if used >= hard {
        let _ = crate::scheduler::end_program(tid, -9);
    } else if used >= soft && crate::fdtable::xcpu_due(tid, used) {
        let _ = crate::signal::raise(tid, SIGXCPU);
    }
}
