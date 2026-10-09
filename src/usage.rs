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
//! collects it (`PerTask::ended`), and that is what a parent is told its children
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

/// What this module keeps about a task, in its record (`TaskRec::usage`):
/// each field was an array of `MAX_TASKS`, and an id with no task reads as
/// `PerTask::new()` — what an empty slot of those arrays held.
pub struct PerTask {
    raw: Raw,
    /// Whether each task is in the kernel, and since when on this turn. A task
    /// is made in the kernel, and first leaves for its program.
    in_kernel: bool,
    kernel_since: u64,
    /// What a program's last task leaves for whoever collects it: what the
    /// program used, and what the children it collected did.
    ended: Usage,
}

impl PerTask {
    pub const fn new() -> Self {
        PerTask {
            raw: Raw::ZERO,
            in_kernel: true,
            kernel_since: 0,
            ended: Usage::ZERO,
        }
    }
}

/// What this module keeps about task `tid`: its record's, or for an id with
/// no task a copy put back to `PerTask::new()` each time it is asked for, so
/// a write for a task that is not there goes nowhere. Interrupts must be off.
unsafe fn st(tid: usize) -> &'static mut PerTask {
    unsafe {
        match crate::scheduler::rec(tid) {
            Some(r) => &mut r.usage,
            None => {
                let none = &mut *core::ptr::addr_of_mut!(NO_TASK);
                *none = PerTask::new();
                none
            }
        }
    }
}

static mut NO_TASK: PerTask = PerTask::new();

/// When each processor began running what it is running.
static mut SINCE: [u64; MAX_CPUS] = [0; MAX_CPUS];

/// What a processor does with its time, as it is counted: runs a program,
/// runs the kernel, has nothing to do, or takes an interrupt.
pub const IN_PROGRAM: u8 = 0;
pub const IN_KERNEL: u8 = 1;
pub const IDLE: u8 = 2;
pub const IN_INTERRUPT: u8 = 3;

/// What a processor has spent its time on, by the clock, and what it has
/// done: nanoseconds at each of the four, the interrupts it took, and the
/// times it went from one task to another.
#[derive(Clone, Copy)]
struct Spent {
    ns: [u64; 4],
    interrupts: u64,
    switches: u64,
}

impl Spent {
    const ZERO: Spent = Spent { ns: [0; 4], interrupts: 0, switches: 0 };
}

/// Each processor's, counted to its last change — which its next door, its
/// next switch to or from the idle loop and its next interrupt each are: a
/// processor with nothing to do still takes its tick. Idle time was thrown
/// away; a machine could not say how busy it was.
static mut SPENT: [Spent; MAX_CPUS] = [Spent::ZERO; MAX_CPUS];
/// What each processor is doing, of the four, and since when.
static mut DOING: [u8; MAX_CPUS] = [IN_KERNEL; MAX_CPUS];
static mut MARK: [u64; MAX_CPUS] = [0; MAX_CPUS];
/// How many tasks the machine has made since it started: Linux's
/// `processes`, which counts its threads as well.
static MADE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// This processor's time is counted from now, as the kernel's.
///
/// # Safety
/// Interrupts off; once, on the processor, as it is started, with the clock
/// running.
pub unsafe fn processor_up() {
    unsafe {
        let cpu = crate::percpu::index();
        MARK[cpu] = crate::clock::now_here();
        DOING[cpu] = IN_KERNEL;
    }
}

/// This processor goes over to `doing`: the time since it last changed is
/// counted as what it was doing then, which is returned.
///
/// # Safety
/// Interrupts off.
pub unsafe fn now_doing(doing: u8) -> u8 {
    unsafe {
        let cpu = crate::percpu::index();
        let now = crate::clock::now_here();
        let was = DOING[cpu];
        SPENT[cpu].ns[was as usize] += now.saturating_sub(MARK[cpu]);
        MARK[cpu] = now;
        DOING[cpu] = doing;
        was
    }
}

/// How many times this processor has gone from one task to another.
///
/// # Safety
/// Interrupts off.
pub unsafe fn switches_here() -> u64 {
    unsafe { SPENT[crate::percpu::index()].switches }
}

/// This processor has taken an interrupt: any, the ones that take nothing
/// from the kernel included.
///
/// # Safety
/// Interrupts off.
pub unsafe fn interrupt_taken() {
    unsafe { SPENT[crate::percpu::index()].interrupts += 1 };
}

/// A task has been made: it has used nothing.
pub fn task_made(tid: usize) {
    MADE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if tid < MAX_TASKS {
        let flags = irq_save();
        unsafe {
            st(tid).raw = Raw::ZERO;
            st(tid).ended = Usage::ZERO;
            st(tid).in_kernel = true;
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
        st(tid).raw.run_ns += ran;
        if st(tid).in_kernel {
            st(tid).raw.sys_ns += now.saturating_sub(st(tid).kernel_since);
            st(tid).kernel_since = now;
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
        // Going to the idle loop, the processor has nothing to do; leaving
        // it, it is in the kernel again.
        if tid == 0 {
            now_doing(IDLE);
        } else if DOING[crate::percpu::index()] == IDLE {
            now_doing(IN_KERNEL);
        }
        if tid != 0 && tid < MAX_TASKS && st(tid).in_kernel {
            st(tid).kernel_since = SINCE[crate::percpu::index()];
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
    unsafe { now_doing(IN_KERNEL) };
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    unsafe {
        st(tid).in_kernel = true;
        st(tid).kernel_since = crate::clock::now_here();
    }
}

/// Task `tid`, running here, is going back to its program: what it has had
/// in the kernel since it came in is counted.
///
/// # Safety
/// Interrupts off.
pub unsafe fn leaving(tid: usize) {
    unsafe { now_doing(IN_PROGRAM) };
    if tid == 0 || tid >= MAX_TASKS {
        return;
    }
    unsafe {
        if st(tid).in_kernel {
            st(tid).raw.sys_ns += crate::clock::now_here().saturating_sub(st(tid).kernel_since);
            st(tid).in_kernel = false;
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
        SPENT[crate::percpu::index()].switches += 1;
        if from != 0 && from < MAX_TASKS {
            if gave_up {
                st(from).raw.voluntary += 1;
            } else {
                st(from).raw.involuntary += 1;
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
        let raw = st(tid).raw;
        let (mut run, mut sys) = (raw.run_ns, raw.sys_ns);
        if let Some(cpu) = crate::scheduler::running_on(tid) {
            let now = crate::clock::now();
            run += now.saturating_sub(SINCE[cpu]);
            if st(tid).in_kernel {
                sys += now.saturating_sub(st(tid).kernel_since);
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
    crate::fdtable::each_task(tid, |t| used.add(&of_task(t)));
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
    let mut n = 0;
    crate::fdtable::each_task(tid, |_| n += 1);
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
    crate::scheduler::each_task_of_process(tid, |t| unsafe { st(t).ended = all });
}

/// `parent` has collected `child`: what the child's program used, and its
/// children, is what `parent`'s program's children used.
pub fn collected(parent: usize, child: usize) {
    if child >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    let ended = unsafe { core::mem::replace(&mut st(child).ended, Usage::ZERO) };
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

/// `SYS_CPU_INFO`, by `op`:
///
/// - 0: which processors are online, a bit each of 256, written at `a` (`b`
///   bytes, at least 32). Answers how many there are.
/// - 1: how processor `a` — or every one together, for `u64::MAX` — has
///   spent its time, eight words at `b`: nanoseconds in programs, in the
///   kernel, with nothing to do and taking interrupts; the interrupts it
///   took and the switches it made; and the machine's tasks made since it
///   started and those ready or running now.
/// - 2: where processor `a` sits, four words at `b`: its APIC id, package,
///   core and thread, as it said when it started.
/// - 3: the processor task `a` (0, the caller) last ran on.
///
/// No capability: what a machine is made of and how busy it is are every
/// program's to know, as they are on Linux.
pub fn cpu_info(caller: usize, op: u64, a: u64, b: u64) -> u64 {
    let count = crate::percpu::count();
    match op {
        0 => {
            if b < 32 || !crate::syscall::validate_user_ptr_mut(a, 32) {
                return u64::MAX;
            }
            let mut set = [0u64; 4];
            for i in 0..count.min(256) {
                set[i / 64] |= 1 << (i % 64);
            }
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(a as *mut [u64; 4], set) };
            count as u64
        }
        1 => {
            if (a != u64::MAX && a as usize >= count) || !crate::syscall::validate_user_ptr_mut(b, 64) {
                return u64::MAX;
            }
            let flags = irq_save();
            let now = crate::clock::now();
            let mut sum = Spent::ZERO;
            for cpu in (0..count).filter(|&cpu| a == u64::MAX || a as usize == cpu) {
                let (spent, doing, mark) = unsafe { (SPENT[cpu], DOING[cpu], MARK[cpu]) };
                for (total, ns) in sum.ns.iter_mut().zip(spent.ns) {
                    *total += ns;
                }
                // And what it is doing now, since it began: a processor
                // asleep says nothing until something wakes it.
                sum.ns[doing as usize] += now.saturating_sub(mark);
                sum.interrupts += spent.interrupts;
                sum.switches += spent.switches;
            }
            let ready = crate::scheduler::runnable() as u64;
            irq_restore(flags);
            let made = MADE.load(core::sync::atomic::Ordering::Relaxed);
            let words = [sum.ns[0], sum.ns[1], sum.ns[2], sum.ns[3], sum.interrupts, sum.switches, made, ready];
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(b as *mut [u64; 8], words) };
            0
        }
        2 => {
            if a as usize >= count || !crate::syscall::validate_user_ptr_mut(b, 32) {
                return u64::MAX;
            }
            let place = crate::percpu::place(a as usize);
            let words = [place.apic as u64, place.package as u64, place.core as u64, place.thread as u64];
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(b as *mut [u64; 4], words) };
            0
        }
        3 => {
            let tid = if a == 0 { caller } else { a as usize };
            crate::scheduler::last_cpu(tid).map_or(u64::MAX, |cpu| cpu as u64)
        }
        _ => u64::MAX,
    }
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

/// What a task this nice weighs: the share of its band it is owed, against
/// 1024 at nought. A processor's load is the weight of what it has to run.
pub fn weight(nice: i8) -> u64 {
    WEIGHTS[(nice.clamp(-20, 19) + 20) as usize]
}

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

/// `SYS_NICE`: how nice process `pid` (0 for the caller's) is — its first
/// task, as 0 to 39 for -20 to 19 — and with `new` (a signed number, -20 to
/// 19) unless that is `u64::MAX`, every task of its program made that nice.
/// Anybody may ask. A program may make itself, or another of its user's,
/// nicer; to be less nice takes `TaskMgmt` for the program, which is what
/// root's power to was. How nice one task is is `SYS_SCHED`'s ([`sched`]).
pub fn nice(caller: usize, pid: u64, new: u64) -> u64 {
    let target = if pid == 0 {
        caller
    } else {
        match crate::scheduler::task_of_pid(pid) {
            Some(t) if crate::scheduler::task_is_live(t) => t,
            _ => return u64::MAX,
        }
    };
    let old = crate::scheduler::nice_of(target);
    if new != u64::MAX {
        if target != caller && !may(caller, target) {
            return NOT_ALLOWED;
        }
        let wanted = (new as i64).clamp(-20, 19) as i8;
        // Less nice is asked of every task it changes: a thread the program
        // made nicer than its first is made less nice by being set to the
        // first's.
        let space = crate::scheduler::space_of_task(target);
        let mut nicest = old;
        crate::scheduler::each_task_of(space, |t| nicest = nicest.max(crate::scheduler::nice_of(t)));
        if wanted < nicest && !crate::cap::task_has_task_mgmt(caller, target) {
            return NOT_ALLOWED;
        }
        crate::scheduler::each_task_of(space, |t| crate::scheduler::set_nice(t, wanted));
        // A task with no program yet is one task.
        crate::scheduler::set_nice(target, wanted);
    }
    (old as i64 + 20) as u64
}

/// `SYS_SCHED`'s operations.
const SCHED_NICE: u64 = 0;
const SCHED_SET_NICE: u64 = 1;
const SCHED_SET_CLASS: u64 = 2;
const SCHED_CLASS: u64 = 3;

/// `SYS_SCHED`: how task `tid` (0 for the caller) is scheduled, one task at a
/// time — Linux's `setpriority` on a thread, and its `sched_setscheduler`.
/// Operation 0 answers how nice it is, 0 to 39 for -20 to 19; 1 makes it as
/// nice as `a` (a signed number), the rules of `SYS_NICE`; 2 puts it in
/// class `a` — 0 ordinary, 1 FIFO, 2 round-robin — at real-time priority `b`,
/// 1 to 99 for a real-time class and 0 for the ordinary one; 3 answers
/// `(class << 8) | priority`. A task of the caller's own program, or one it
/// may say this of as `SYS_NICE` decides; and entering a real-time class
/// takes the right to, `RealTime`, which leaving it does not.
pub fn sched(caller: usize, op: u64, tid: u64, a: u64, b: u64) -> u64 {
    let target = if tid == 0 { caller } else { tid as usize };
    if !crate::scheduler::task_is_live(target) {
        return u64::MAX;
    }
    let ours = crate::scheduler::space_of_task(target) == crate::scheduler::space_of_task(caller);
    let allowed = ours || may(caller, target);
    match op {
        SCHED_NICE => (crate::scheduler::nice_of(target) as i64 + 20) as u64,
        SCHED_SET_NICE => {
            let wanted = (a as i64).clamp(-20, 19) as i8;
            if !allowed || (wanted < crate::scheduler::nice_of(target) && !crate::cap::task_has_task_mgmt(caller, target)) {
                return NOT_ALLOWED;
            }
            crate::scheduler::set_nice(target, wanted);
            0
        }
        SCHED_SET_CLASS => {
            let (policy, priority) = (a, b);
            let fits = match policy as u8 {
                crate::scheduler::SCHED_OTHER => priority == 0,
                crate::scheduler::SCHED_FIFO | crate::scheduler::SCHED_RR => (1..=99).contains(&priority),
                _ => false,
            };
            if policy > 2 || !fits {
                return u64::MAX;
            }
            if !allowed || (policy != 0 && !crate::cap::task_has_realtime(caller)) {
                return NOT_ALLOWED;
            }
            crate::scheduler::set_sched(target, policy as u8, priority as u8);
            0
        }
        SCHED_CLASS => {
            let (policy, priority) = crate::scheduler::sched_of(target);
            (policy as u64) << 8 | priority as u64
        }
        _ => u64::MAX,
    }
}

/// `SYS_AFFINITY`: which processors task `tid` (0 for the caller) may run
/// on, a set of 256 bits at `buf` (32 bytes): op 0 writes there those of
/// them that are online, op 1 makes it what is there. A task of the caller's own program, or one it
/// may say this of as `SYS_NICE` decides. A set with no processor that is
/// online is refused; one that leaves out where the task is moves it.
pub fn affinity(caller: usize, op: u64, tid: u64, buf: u64) -> u64 {
    let target = if tid == 0 { caller } else { tid as usize };
    if !crate::scheduler::task_is_live(target) {
        return u64::MAX;
    }
    match op {
        0 => {
            if !crate::syscall::validate_user_ptr_mut(buf, 32) {
                return u64::MAX;
            }
            // As Linux answers it: of the processors it may run on, those that
            // are online. As kept, a task told nothing may run on all 256 —
            // and a C library that counts the bits to say how many processors
            // there are said 256.
            let mut set = crate::scheduler::affinity_of(target);
            let count = crate::percpu::count().min(256);
            for (i, word) in set.iter_mut().enumerate() {
                let online = match count.saturating_sub(i * 64) {
                    0 => 0,
                    n if n >= 64 => u64::MAX,
                    n => (1u64 << n) - 1,
                };
                *word &= online;
            }
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(buf as *mut [u64; 4], set) };
            0
        }
        1 => {
            if !crate::syscall::validate_user_ptr(buf, 32) {
                return u64::MAX;
            }
            let ours = crate::scheduler::space_of_task(target) == crate::scheduler::space_of_task(caller);
            if !ours && !may(caller, target) {
                return NOT_ALLOWED;
            }
            let set = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { core::ptr::read_unaligned(buf as *const [u64; 4]) }
            };
            if !(0..crate::percpu::count().min(256)).any(|cpu| set[cpu / 64] >> (cpu % 64) & 1 == 1) {
                return u64::MAX;
            }
            crate::scheduler::set_affinity(target, set);
            0
        }
        _ => u64::MAX,
    }
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
