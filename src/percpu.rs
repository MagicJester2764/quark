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
//!
//! **What one processor knows about another** is the rest of what is here:
//! how many there are, the id each one's local APIC answers to, which
//! address space each has loaded, whether it is asleep with nothing to do,
//! and whether it has been asked to forget its translations. Those are
//! atomics, read and written from outside; everything above is touched by
//! its own processor and nobody else.

use crate::context::CpuContext;
use core::arch::asm;
use core::mem::offset_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// How many processors the kernel will run on. The table of them is this
/// long; a machine with more has the rest left stopped.
pub const MAX_CPUS: usize = 16;

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
    /// The caller's R9, which the stub puts here before it uses the register
    /// for something else: `%gs:16`. No call of this kernel's takes a sixth
    /// argument, and Linux's do — a call a program's trap turns into a
    /// signal (`signal::trap_call`) is one of Linux's, and gives every
    /// register back as it was. Read before anything can enter the kernel
    /// on this processor again: interrupts are off from the stub until the
    /// dispatcher has it.
    syscall_r9: u64,
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
    /// The top of the stack a double fault is taken on: the one fault that
    /// may mean the ordinary stack is gone (`kstack.rs`).
    df_top: u64,
    /// The address space it has loaded: what is in its CR3. Written by the
    /// processor itself (`paging::write_cr3`), and read by one that has
    /// changed a space's tables and needs to know who may be holding the
    /// old ones (`tlb.rs`).
    cr3: AtomicUsize,
    /// The id its local APIC answers to: where an interrupt for it is sent.
    apic_id: AtomicU32,
    /// It has nothing to run and is waiting for an interrupt, or is about
    /// to. Set under the kernel lock by the processor itself; taken by
    /// whoever wakes it, so that two tasks made ready wake two processors.
    napping: AtomicBool,
    /// Another processor has taken mappings away and asks this one to
    /// forget what it has cached. Cleared by this one when it has.
    flush: AtomicBool,
}

const USER_RSP: usize = offset_of!(PerCpu, user_rsp);
const KERNEL_RSP: usize = offset_of!(PerCpu, kernel_rsp);
const SYSCALL_R9: usize = offset_of!(PerCpu, syscall_r9);
const INDEX: usize = offset_of!(PerCpu, index);
const CURRENT: usize = offset_of!(PerCpu, current);
const TSS_RSP0: usize = offset_of!(PerCpu, tss) + offset_of!(Tss, rsp0);
const CR3: usize = offset_of!(PerCpu, cr3);
// The stub in `syscall.rs` says `%gs:0`, `%gs:8` and `%gs:16`.
const _: () = assert!(USER_RSP == 0 && KERNEL_RSP == 8 && SYSCALL_R9 == 16);

const EMPTY: PerCpu = PerCpu {
    user_rsp: 0,
    kernel_rsp: 0,
    syscall_r9: 0,
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
    df_top: 0,
    cr3: AtomicUsize::new(0),
    apic_id: AtomicU32::new(0),
    napping: AtomicBool::new(false),
    flush: AtomicBool::new(false),
};

static mut CPUS: [PerCpu; MAX_CPUS] = [EMPTY; MAX_CPUS];

/// How many processors are running the kernel: the first, and each of the
/// others once it has arrived (`smp.rs`). They are numbered in the order
/// they came, so the processors are `0..count()` with no gaps.
static ONLINE: AtomicUsize = AtomicUsize::new(1);


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

/// What the caller of the system call this processor is in had in R9.
/// Right until interrupts are next turned on.
#[inline(always)]
pub fn syscall_r9() -> u64 {
    let r9: u64;
    unsafe {
        asm!("mov {}, gs:[{at}]", out(reg) r9, at = const SYSCALL_R9,
             options(nostack, readonly, preserves_flags));
    }
    r9
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

/// How many processors are running the kernel.
#[inline]
pub fn count() -> usize {
    ONLINE.load(Ordering::Relaxed)
}

/// Processor `index` has arrived, and its local APIC answers to `apic_id`.
/// Said by the first processor as it starts each of the others in turn.
pub fn came_online(index: usize, apic_id: u32) {
    unsafe { (*(&raw const CPUS[index].apic_id)).store(apic_id, Ordering::Relaxed) };
    ONLINE.store(index + 1, Ordering::Release);
}

/// Say what this processor's local APIC answers to.
///
/// # Safety
/// Interrupts off.
pub unsafe fn set_apic_id(apic_id: u32) {
    unsafe { (*(&raw const (*this()).apic_id)).store(apic_id, Ordering::Relaxed) };
}

/// Where each processor sits, as it said when it was started.
static mut PLACES: [crate::cpu::Place; MAX_CPUS] = [crate::cpu::Place::NOWHERE; MAX_CPUS];

/// Say where processor `index` — the one this runs on — sits.
///
/// # Safety
/// On that processor, as it is started: before it is said to be online,
/// after which it is read from anywhere.
pub unsafe fn set_place(index: usize, place: crate::cpu::Place) {
    unsafe { (*(&raw mut PLACES))[index] = place };
}

/// Where processor `cpu`, one that is online, sits.
pub fn place(cpu: usize) -> crate::cpu::Place {
    unsafe { (*(&raw const PLACES))[cpu] }
}

/// What processor `cpu`'s local APIC answers to.
pub fn apic_id(cpu: usize) -> u32 {
    unsafe { (*(&raw const CPUS[cpu].apic_id)).load(Ordering::Relaxed) }
}

/// This processor has loaded address space `cr3`.
///
/// # Safety
/// Interrupts off, between this and the load itself: the two are one fact.
#[inline(always)]
pub unsafe fn note_cr3(cr3: usize) {
    unsafe {
        asm!("mov gs:[{at}], {}", in(reg) cr3, at = const CR3,
             options(nostack, preserves_flags));
    }
}

/// The address space processor `cpu` has loaded. It does not change while
/// the caller holds the kernel lock: a processor loads another only in the
/// kernel.
pub fn cr3_of(cpu: usize) -> usize {
    unsafe { (*(&raw const CPUS[cpu].cr3)).load(Ordering::Relaxed) }
}

/// This processor has nothing to run and is about to wait for an interrupt.
/// Said while it still holds the kernel lock, so that whoever next makes a
/// task ready — which takes the lock — finds it said.
///
/// # Safety
/// Interrupts off.
pub unsafe fn nap() {
    unsafe { (*(&raw const (*this()).napping)).store(true, Ordering::Release) };
}

/// This processor is awake again, whatever woke it.
///
/// # Safety
/// Interrupts off.
pub unsafe fn woke() {
    unsafe { (*(&raw const (*this()).napping)).store(false, Ordering::Release) };
}

/// Whether processor `cpu` is waiting with nothing to do.
pub fn napping(cpu: usize) -> bool {
    unsafe { (*(&raw const CPUS[cpu].napping)).load(Ordering::Acquire) }
}

/// The task processor `cpu` is running, 0 for its idle loop. It does not
/// change while the caller holds the kernel lock: a processor switches only
/// in the kernel.
pub fn current_of(cpu: usize) -> usize {
    unsafe { core::ptr::read_volatile(&raw const CPUS[cpu].current) as usize }
}

/// If processor `cpu` is waiting with nothing to do, it is now the caller's
/// to wake: true once, for one caller.
pub fn wake_from_nap(cpu: usize) -> bool {
    // Read first: every task made ready asks this of every processor, and
    // nearly always the answer is no, which should not cost a write to a
    // line of memory the processor asked about is using.
    unsafe {
        let napping = &*(&raw const CPUS[cpu].napping);
        napping.load(Ordering::Acquire) && napping.swap(false, Ordering::AcqRel)
    }
}

/// Ask processor `cpu` to forget the translations it has cached. It is
/// asked again with an interrupt, and has answered when
/// [`flush_pending`] says no.
pub fn ask_flush(cpu: usize) {
    unsafe { (*(&raw const CPUS[cpu].flush)).store(true, Ordering::Release) };
}

/// Whether processor `cpu` has still to answer [`ask_flush`].
pub fn flush_pending(cpu: usize) -> bool {
    unsafe { (*(&raw const CPUS[cpu].flush)).load(Ordering::Acquire) }
}

/// Whether this processor has been asked to forget its translations.
///
/// # Safety
/// Interrupts off.
pub unsafe fn flush_asked() -> bool {
    unsafe { (*(&raw const (*this()).flush)).load(Ordering::Acquire) }
}

/// This processor has forgotten them.
///
/// # Safety
/// Interrupts off, and after the translations have gone.
pub unsafe fn flush_done() {
    unsafe { (*(&raw const (*this()).flush)).store(false, Ordering::Release) };
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
        (*(&raw const (*cpu).cr3)).store(crate::paging::read_cr3(), Ordering::Relaxed);
        // In the kernel, this processor's state; in a program, nothing. The
        // second is what `swapgs` leaves in GS on the way out to ring 3.
        crate::cpu::wrmsr(MSR_GS_BASE, cpu as u64);
        crate::cpu::wrmsr(MSR_KERNEL_GS_BASE, 0);
    }
}

/// Processor `index` takes a double fault on the stack whose top is `top`,
/// from when it loads its tables. Before it is started, or by itself before
/// it has.
pub fn set_df_stack(index: usize, top: usize) {
    unsafe { (*(&raw mut CPUS[index])).df_top = top as u64 };
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
        (*cpu).tss.ist[0] = (*cpu).df_top;
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
