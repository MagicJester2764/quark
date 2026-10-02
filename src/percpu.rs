//! What each processor has of its own.
//!
//! Most of the kernel's state is the machine's: one task table, one set of
//! ready queues, one of everything. A few things cannot be, because two
//! processors need different answers at the same moment: which task this
//! one is running, where that task's kernel stack is, the stack an
//! interrupt from ring 3 is taken on, and where this processor was when it
//! last had nothing to do.
//!
//! **How a processor finds its own.** GS. In the kernel its base is this
//! processor's [`PerCpu`]; in a program it is the program's (nothing sets
//! one, so it is 0) and the kernel's waits in `IA32_KERNEL_GS_BASE` for the
//! `swapgs` every way in from ring 3 begins with. The system call stub
//! always reached its two words this way; the rest is beside them now.
//!
//! **A task can change processors wherever it can be preempted**, which in
//! a system call is anywhere interrupts are on. So "which processor is
//! this" is a question with an answer only while they are off, and nothing
//! may read the answer and act on it later. What *is* safe with interrupts
//! on is one instruction through GS: it reads or writes the processor the
//! task is on at that instruction. That is all [`current`] is, and it is
//! right however often the task moves, because what it reads is the task's
//! own id — every processor it could be on holds the same one for it.
//! [`set_kernel_stack`] leans on the same thing twice over: each of its two
//! stores puts this task's stack where this task's processor keeps it, and
//! a processor the task has left was given the next task's by the switch
//! that took it away.

use crate::context::CpuContext;
use core::arch::asm;
use core::mem::offset_of;

/// How many processors the kernel will run on. The table of them is this
/// long; a machine with more has the rest left stopped.
pub const MAX_CPUS: usize = 16;

/// The stack a double fault is taken on, per processor: the one fault that
/// may mean the ordinary stack is gone.
const DF_STACK_SIZE: usize = 16384;

/// The 64-bit task state segment: where the processor finds a stack when it
/// changes privilege or takes an interrupt with an IST index.
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    /// The stack for an interrupt or fault taken in ring 3.
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    /// Where the I/O permission bitmap is. Past the end: there is none.
    iomap_base: u16,
}

const TSS_SIZE: usize = core::mem::size_of::<Tss>();
const _: () = assert!(TSS_SIZE == 104);

/// The descriptors, in the order `boot.s` has them and `syscall`/`sysret`
/// need: user data before user code.
const GDT_ENTRIES: usize = 7;
const GDT_KERNEL_CODE: u64 = 0x00AF_9A00_0000_FFFF;
const GDT_KERNEL_DATA: u64 = 0x00CF_9200_0000_FFFF;
const GDT_USER_DATA: u64 = 0x00CF_F200_0000_FFFF;
const GDT_USER_CODE: u64 = 0x00AF_FA00_0000_FFFF;
/// The selector of the TSS descriptor, which is two entries long.
const TSS_SELECTOR: u16 = 0x18;

#[repr(C, align(64))]
pub struct PerCpu {
    /// Where the system call stub keeps the caller's stack pointer while it
    /// finds its own. `%gs:0`, by name in the stub.
    user_rsp: u64,
    /// The top of the running task's kernel stack. `%gs:8`, likewise.
    kernel_rsp: u64,
    /// Which processor this is: its place in the table.
    index: u64,
    /// The task it is running. 0 while it has nothing to run: every
    /// processor's idle loop is task 0.
    current: u64,
    tss: Tss,
    gdt: [u64; GDT_ENTRIES],
    /// Where this processor's idle loop was when it last switched away.
    /// Task 0 is one task with as many of these as there are processors.
    idle_context: CpuContext,
    /// The stack the idle loop runs on, for a fault report to check a stack
    /// pointer against.
    idle_stack: (usize, usize),
}

const USER_RSP: usize = offset_of!(PerCpu, user_rsp);
const KERNEL_RSP: usize = offset_of!(PerCpu, kernel_rsp);
const INDEX: usize = offset_of!(PerCpu, index);
const CURRENT: usize = offset_of!(PerCpu, current);
const TSS_RSP0: usize = offset_of!(PerCpu, tss) + offset_of!(Tss, rsp0);
// The stub in `syscall.rs` says `%gs:0` and `%gs:8`.
const _: () = assert!(USER_RSP == 0 && KERNEL_RSP == 8);

const EMPTY: PerCpu = PerCpu {
    user_rsp: 0,
    kernel_rsp: 0,
    index: 0,
    current: 0,
    tss: Tss {
        reserved0: 0,
        rsp0: 0,
        rsp1: 0,
        rsp2: 0,
        reserved1: 0,
        ist: [0; 7],
        reserved2: 0,
        reserved3: 0,
        iomap_base: TSS_SIZE as u16,
    },
    gdt: [0; GDT_ENTRIES],
    idle_context: CpuContext::empty(),
    idle_stack: (0, 0),
};

static mut CPUS: [PerCpu; MAX_CPUS] = [EMPTY; MAX_CPUS];

#[repr(C, align(16))]
struct DfStack([u8; DF_STACK_SIZE]);
static mut DF_STACKS: [DfStack; MAX_CPUS] = [const { DfStack([0; DF_STACK_SIZE]) }; MAX_CPUS];

const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// The task this processor is running. One instruction, so it is right with
/// interrupts on: see the top of this file.
#[inline(always)]
pub fn current() -> usize {
    let tid: usize;
    unsafe {
        asm!("mov {}, gs:[{at}]", out(reg) tid, at = const CURRENT,
             options(nostack, readonly, preserves_flags));
    }
    tid
}

/// Say which task this processor is running.
///
/// # Safety
/// Interrupts off, and the switch to that task must follow.
#[inline(always)]
pub unsafe fn set_current(tid: usize) {
    unsafe {
        asm!("mov gs:[{at}], {}", in(reg) tid, at = const CURRENT,
             options(nostack, preserves_flags));
    }
}

/// Which processor this is. True for as long as interrupts stay off.
#[inline(always)]
pub fn index() -> usize {
    let index: usize;
    unsafe {
        asm!("mov {}, gs:[{at}]", out(reg) index, at = const INDEX,
             options(nostack, readonly, preserves_flags));
    }
    index
}

/// This processor's own state.
///
/// # Safety
/// Interrupts off for as long as the pointer is used.
unsafe fn this() -> *mut PerCpu {
    unsafe { (&raw mut CPUS[index()]) as *mut PerCpu }
}

/// Where the running task's kernel stack ends: the stack a system call
/// begins on, and the one an interrupt taken in ring 3 is given.
///
/// Called on every switch, and by a task about to enter ring 3 for the
/// first time. Two stores, each of them whole: with interrupts on a task
/// may be moved between them, and each still lands on the processor the
/// task is on, which is the one that must have it.
#[inline(always)]
pub fn set_kernel_stack(top: u64) {
    unsafe {
        asm!(
            "mov gs:[{stub}], {top}",
            "mov gs:[{tss}], {top}",
            top = in(reg) top,
            stub = const KERNEL_RSP,
            tss = const TSS_RSP0,
            options(nostack, preserves_flags),
        );
    }
}

/// Where this processor's idle loop keeps its place.
///
/// # Safety
/// Interrupts off.
pub unsafe fn idle_context() -> *mut CpuContext {
    unsafe { &raw mut (*this()).idle_context }
}

/// The stack this processor's idle loop runs on, as (base, top).
pub fn idle_stack() -> (usize, usize) {
    let flags: u64;
    unsafe {
        asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
        let stack = (*this()).idle_stack;
        if flags & (1 << 9) != 0 {
            asm!("sti", options(nostack, nomem));
        }
        stack
    }
}

/// Make processor `index` this one: from here on GS finds its state.
///
/// The first thing a processor does in Rust, before anything asks which
/// task is running — the answer to that comes through GS, and until this
/// has run GS points at address 0.
///
/// # Safety
/// Once per processor, on that processor, with interrupts off. `stack` is
/// the stack it is running on, which its idle loop keeps.
pub unsafe fn init(index: usize, stack: (usize, usize)) {
    unsafe {
        let cpu = (&raw mut CPUS[index]) as *mut PerCpu;
        (*cpu).index = index as u64;
        (*cpu).current = 0;
        (*cpu).idle_stack = stack;
        // In the kernel, this processor's state; in a program, nothing. The
        // second is what `swapgs` leaves in GS on the way out to ring 3.
        crate::cpu::wrmsr(MSR_GS_BASE, cpu as u64);
        crate::cpu::wrmsr(MSR_KERNEL_GS_BASE, 0);
    }
}

#[repr(C, packed)]
struct TablePtr {
    limit: u16,
    base: u64,
}

/// Give this processor a descriptor table and a task state segment of its
/// own, and load them.
///
/// A TSS cannot be shared: it holds the stack an interrupt from ring 3 is
/// taken on, which is the running task's, and two processors run two
/// tasks. The descriptor table has the TSS's address in it, so that is one
/// each too. The selectors are the ones `boot.s` set up and do not change.
///
/// # Safety
/// After [`init`], on the same processor, interrupts off.
pub unsafe fn load_tables() {
    unsafe {
        let cpu = this();
        let index = (*cpu).index as usize;

        let df_top = (&raw const DF_STACKS[index]) as u64 + DF_STACK_SIZE as u64;
        (*cpu).tss.ist[0] = df_top;
        (*cpu).tss.iomap_base = TSS_SIZE as u16;

        let base = (&raw const (*cpu).tss) as u64;
        let limit = (TSS_SIZE - 1) as u64;
        let tss_low = (limit & 0xFFFF)
            | ((base & 0xFFFF) << 16)
            | (((base >> 16) & 0xFF) << 32)
            | (0x89u64 << 40) // present, an available 64-bit TSS
            | (((base >> 24) & 0xFF) << 56);
        let tss_high = base >> 32;
        (*cpu).gdt = [0, GDT_KERNEL_CODE, GDT_KERNEL_DATA, tss_low, tss_high, GDT_USER_DATA, GDT_USER_CODE];

        let ptr = TablePtr {
            limit: (core::mem::size_of::<[u64; GDT_ENTRIES]>() - 1) as u16,
            base: (&raw const (*cpu).gdt) as u64,
        };
        asm!("lgdt [{}]", in(reg) &ptr, options(nostack, readonly, preserves_flags));
        asm!("ltr {0:x}", in(reg) TSS_SELECTOR, options(nostack, nomem, preserves_flags));
    }
}
