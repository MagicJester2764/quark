//! More than one processor: starting the others, and what one processor
//! says to another.
//!
//! The first processor is the one the firmware started. The rest are found
//! in the ACPI tables (`acpi.rs`) and started here one at a time: each is
//! reset through its local APIC, told to begin at a page of code copied into
//! the first megabyte (`ap_boot.s`), and arrives in [`arrive`] on a stack of
//! its own. From there it is a processor like the first — it loads the same
//! tables, turns on what the first turned on, and goes to the idle loop to
//! wait for the kernel lock and something to run.
//!
//! One processor tells another something by interrupting it. There are
//! three things to say, and how each is heard depends on whether hearing it
//! needs the kernel:
//!
//! - **Look at what you are running, and at what is waiting**
//!   (`idt::VEC_RESCHED`): to a processor asleep with nothing to do when a
//!   task has been put to wait there, or waits elsewhere for one that may
//!   run it ([`wake`]); to a processor running worse than a task put to wait
//!   there, and to the processor running a task that has just been ended or
//!   stopped ([`interrupt`]) — a task in
//!   ring 3 does not know, and goes on until something brings its processor
//!   into the kernel; this is the something. An ordinary interrupt: whoever
//!   takes it takes the kernel lock like any other way in.
//! - **Forget your translations** (`idt::VEC_FLUSH`, `tlb.rs`): answered
//!   without the lock, because whoever asks is holding it and waiting.
//! - **Stop** (`idt::VEC_HALT`, [`halt_others`]): when the kernel has
//!   faulted. The machine halts, and that means all of it. Without the lock
//!   too: the processor that faulted has it and is not giving it back.
//!
//! A processor waiting for the kernel lock has interrupts off and hears
//! nothing, so it looks for the last two each time round
//! ([`while_waiting`]).

use crate::multiboot2::{MemoryRegion, MMAP_TYPE_AVAILABLE};
use crate::percpu::{self, MAX_CPUS};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

const PAGE: usize = 4096;

unsafe extern "C" {
    static ap_start: u8;
    static ap_start_end: u8;
    static ap_protected: u8;
    static ap_long: u8;
    static ap_gdt: u8;
    static ap_gdt_base: u8;
    static ap_far_protected: u8;
    static ap_far_long: u8;
    static ap_word_cr0: u8;
    static ap_word_cr3: u8;
    static ap_word_cr4: u8;
    static ap_word_efer: u8;
    static ap_word_stack: u8;
    static ap_word_index: u8;
    static ap_word_entry: u8;
}

core::arch::global_asm!(include_str!("ap_boot.s"), options(att_syntax));

/// How far each processor has got in being started. A processor that
/// arrives after the first has stopped waiting for it must not run: nothing
/// would know it was there.
const WAITED_FOR: u8 = 0;
const ARRIVED: u8 = 1;
const TAKEN_ON: u8 = 2;
const GIVEN_UP_ON: u8 = 3;
static STARTED: [AtomicU8; MAX_CPUS] = [const { AtomicU8::new(WAITED_FOR) }; MAX_CPUS];

/// What the time-stamp counter of the processor being started read, the
/// moment it was told it had been taken on: for the first to compare with
/// its own (`clock.rs`).
static COUNTER_READ: AtomicU64 = AtomicU64::new(0);

/// The stack the processor being started was given, for it to record, and
/// the first processor's control registers, for it to match.
static mut ARRIVING_STACK: (usize, usize) = (0, 0);
static mut CONTROL: (u64, u64) = (0, 0);

/// The kernel has faulted: every processor stops where it is.
static HALTING: AtomicBool = AtomicBool::new(false);

/// CR4.PAE, which is all of CR4 that getting into long mode needs.
const CR4_PAE: u32 = 1 << 5;
const MSR_EFER: u32 = 0xC000_0080;
/// EFER.LMA: the processor's to say, not something to ask for.
const EFER_LMA: u64 = 1 << 10;

/// How far into the code that is copied a label is.
fn offset_of(label: *const u8) -> usize {
    label as usize - (&raw const ap_start) as usize
}

/// A page in the first megabyte to start processors from: memory the
/// firmware says is ordinary, and that nothing the bootloader left is in.
///
/// The frame allocator never gives out the first megabyte, so nothing of
/// the kernel's is there either.
fn startup_page(regions: &[MemoryRegion], mb_info: (usize, usize)) -> Option<usize> {
    let usable = |page: usize| {
        let end = page + PAGE;
        let in_ram = regions.iter().any(|r| {
            r.region_type == MMAP_TYPE_AVAILABLE
                && r.base as usize <= page
                && end as u64 <= r.base.saturating_add(r.length)
        });
        let clear_of = |start: usize, stop: usize| end <= start || stop <= page;
        let mut free = clear_of(mb_info.0, mb_info.0 + mb_info.1);
        for i in 0..crate::modules::count() {
            if let Some(m) = crate::modules::get(i) {
                free &= clear_of(m.start, m.end);
            }
        }
        in_ram && free
    };
    // Not the first pages, which are where a wild pointer lands and where a
    // BIOS keeps its own data, and not the last ones below the video memory,
    // where it keeps more.
    (0x8000..0x9_0000).step_by(PAGE).find(|&page| usable(page))
}

/// Busy for `ticks` of the clock. Interrupts must be on.
fn wait_ticks(ticks: u64) {
    let until = crate::pit::ticks() + ticks;
    while crate::pit::ticks() < until {
        core::hint::spin_loop();
    }
}

/// Start every processor the firmware listed, up to as many as there is
/// room for. Says what it did on the serial line.
///
/// Does nothing, and says why, on a machine with no tables, one processor,
/// no usable local APIC or nowhere to start one from: that machine has one
/// processor, as every machine had.
///
/// # Safety
/// Once, on the first processor, with the kernel lock held, interrupts on
/// and the clock ticking; after the system call MSRs and the protections
/// are set, since the others are given what the first has; and before there
/// is a task to switch to, since a tick that switched away in the middle of
/// this would leave a processor half started.
pub unsafe fn start(regions: &[MemoryRegion], mb_info: (usize, usize)) {
    use crate::serial::{put_usize, puts};
    let info = crate::acpi::info();
    if !info.found || info.ncpus < 2 {
        return;
    }
    let found = unsafe {
        core::arch::asm!("cli", options(nostack, nomem));
        let found = crate::lapic::init();
        if found {
            percpu::set_apic_id(crate::lapic::id());
        }
        core::arch::asm!("sti", options(nostack, nomem));
        found
    };
    if !found {
        puts(b"SMP: no local APIC to start the other processors with; one processor.\n");
        return;
    }
    let me = crate::lapic::id();
    let Some(page) = startup_page(regions, mb_info) else {
        puts(b"SMP: nowhere in the first megabyte to start a processor from; one processor.\n");
        return;
    };
    if !crate::lapic::calibrate() {
        puts(b"SMP: the local APIC's timer does not count; one processor.\n");
        return;
    }

    unsafe {
        // The code, and in it the three addresses it cannot work out.
        let len = offset_of(&raw const ap_start_end);
        core::ptr::copy_nonoverlapping(&raw const ap_start, page as *mut u8, len);
        let word32 = |label: *const u8, value: u32| {
            core::ptr::write_unaligned((page + offset_of(label)) as *mut u32, value);
        };
        let word64 = |label: *const u8, value: u64| {
            core::ptr::write_unaligned((page + offset_of(label)) as *mut u64, value);
        };
        word32(&raw const ap_gdt_base, (page + offset_of(&raw const ap_gdt)) as u32);
        word32(&raw const ap_far_protected, (page + offset_of(&raw const ap_protected)) as u32);
        word32(&raw const ap_far_long, (page + offset_of(&raw const ap_long)) as u32);
        // What the first processor has turned on. Paging and long mode from
        // the start; the rest of CR4 once the processor is in Rust.
        *(&raw mut CONTROL) = crate::cpu::control_registers();
        word32(&raw const ap_word_cr0, (*(&raw const CONTROL)).0 as u32);
        word32(&raw const ap_word_cr4, CR4_PAE);
        word32(&raw const ap_word_efer, (crate::cpu::rdmsr(MSR_EFER) & !EFER_LMA) as u32);
        word32(&raw const ap_word_cr3, crate::paging::kernel_cr3() as u32);
        word64(&raw const ap_word_entry, arrive as extern "C" fn(usize) -> ! as usize as u64);

        let mut index = 1;
        for cpu in &info.cpus[..info.ncpus] {
            if cpu.apic_id == me {
                continue;
            }
            if index >= MAX_CPUS {
                break;
            }
            // Eight bits name a processor in xAPIC mode, and the last of
            // them is everybody.
            if !crate::lapic::x2() && cpu.apic_id >= 0xFF {
                puts(b"SMP: a processor named in more than eight bits, and no x2APIC mode to name it in.\n");
                continue;
            }
            // The stack its idle loop runs on, and an interrupt taken there,
            // and the one it takes a double fault on: each with a page below
            // it that faults (`kstack.rs`).
            let (Some((stack, top)), Some((_, df))) = (crate::kstack::alloc(), crate::kstack::alloc()) else {
                break;
            };
            percpu::set_df_stack(index, df);
            *(&raw mut ARRIVING_STACK) = (stack, top);
            word64(&raw const ap_word_stack, top as u64);
            word64(&raw const ap_word_index, index as u64);

            // Reset it, give it time to be reset, and say where to start —
            // twice if the first is not heard, which is what the processors
            // this was first written down for sometimes needed.
            crate::lapic::send_init(cpu.apic_id);
            wait_ticks(2);
            for _ in 0..2 {
                crate::lapic::send_startup(cpu.apic_id, (page / PAGE) as u8);
                let give_up = crate::pit::ticks() + 20;
                while STARTED[index].load(Ordering::Acquire) != ARRIVED && crate::pit::ticks() < give_up {
                    core::hint::spin_loop();
                }
                if STARTED[index].load(Ordering::Acquire) == ARRIVED {
                    break;
                }
            }
            // One of two things, and the processor is told which: it is
            // here and is counted, or it is not and must never run — it
            // was given this stack and this place in the table, and if it
            // arrives late nothing would know to wake it or to tell it a
            // mapping had gone.
            COUNTER_READ.store(0, Ordering::Release);
            let before = crate::clock::raw();
            if STARTED[index]
                .compare_exchange(ARRIVED, TAKEN_ON, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                percpu::came_online(index, cpu.apic_id);
                index += 1;
                // The clock is one counter read on whichever processor is
                // asked, so every processor's has to read the same. This
                // one's, read between two readings of the first's: if it
                // is not between them, the counter is not a clock here.
                if crate::clock::fine() {
                    let give_up = crate::pit::ticks() + 20;
                    while COUNTER_READ.load(Ordering::Acquire) == 0 && crate::pit::ticks() < give_up {
                        core::hint::spin_loop();
                    }
                    let theirs = COUNTER_READ.load(Ordering::Acquire);
                    let after = crate::clock::raw();
                    if theirs < before || theirs > after {
                        crate::clock::distrust();
                    }
                }
            } else {
                // Not here. It is reset where it is, if it is anywhere,
                // and waits for a STARTUP it is never sent: one that came
                // late would read the next processor's words in the page
                // and run on its stack. Then its place is the next one's.
                // The stacks stay its.
                STARTED[index].store(GIVEN_UP_ON, Ordering::Release);
                crate::lapic::send_init(cpu.apic_id);
                wait_ticks(2);
                STARTED[index].store(WAITED_FOR, Ordering::Release);
                puts(b"SMP: the processor with APIC id ");
                put_usize(cpu.apic_id as usize);
                puts(b" did not start.\n");
            }
        }
    }

    puts(b"SMP: ");
    put_usize(percpu::count());
    puts(if percpu::count() == 1 { b" processor" } else { b" processors" });
    puts(if crate::lapic::x2() { b", x2APIC.\n" } else { b".\n" });
}

/// Where a processor other than the first arrives, from `ap_boot.s`: in
/// long mode, on the first processor's page tables and its own stack, with
/// interrupts off and nothing else.
///
/// It makes itself a processor like the first and goes to the idle loop.
/// Until it has the kernel lock it touches nothing that is not its own.
extern "C" fn arrive(index: usize) -> ! {
    unsafe {
        crate::cpu::set_control_registers(*(&raw const CONTROL));
        percpu::init(index, *(&raw const ARRIVING_STACK));
        // Its number where a program can ask it, and where it sits.
        crate::cpu::say_processor_index(index);
        percpu::set_place(index, crate::cpu::place());
        percpu::load_tables();
        crate::idt::load();
        crate::fpu::init_processor();
        crate::syscall::init_processor();
        crate::lapic::init_other();
        crate::lapic::init_local(false);
    }
    // Here, and waiting to be told it was in time.
    if STARTED[index]
        .compare_exchange(WAITED_FOR, ARRIVED, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        halt_here();
    }
    while STARTED[index].load(Ordering::Acquire) == ARRIVED {
        core::hint::spin_loop();
    }
    if STARTED[index].load(Ordering::Acquire) != TAKEN_ON {
        halt_here();
    }
    COUNTER_READ.store(crate::clock::raw().max(1), Ordering::Release);
    // Its time is counted from here.
    unsafe { crate::usage::processor_up() };
    // The first processor is still starting the system and has the lock;
    // this waits here until it has nothing to do, or goes to ring 3.
    crate::klock::acquire();
    unsafe { crate::lapic::start_timer() };
    crate::scheduler::idle()
}

/// Wake processor `cpu`, if it is asleep: something has been put in its
/// queue for it to run.
pub fn wake(cpu: usize) {
    if cpu < percpu::count() && cpu != percpu::index() && percpu::wake_from_nap(cpu) {
        crate::lapic::send(percpu::apic_id(cpu), crate::idt::VEC_RESCHED);
    }
}

/// Bring processor `cpu` into the kernel to look at what it is running:
/// the task has just been ended or stopped, by this one.
pub fn interrupt(cpu: usize) {
    if cpu < percpu::count() && crate::lapic::present() {
        crate::lapic::send(percpu::apic_id(cpu), crate::idt::VEC_RESCHED);
    }
}

/// The kernel has faulted on this processor: stop the others. They are
/// either in ring 3 or asleep, where the interrupt reaches them, or waiting
/// for the kernel lock with interrupts off, where they see the flag.
pub fn halt_others() {
    if HALTING.swap(true, Ordering::SeqCst) || percpu::count() == 1 || !crate::lapic::present() {
        return;
    }
    let me = percpu::index();
    for cpu in (0..percpu::count()).filter(|&cpu| cpu != me) {
        crate::lapic::send(percpu::apic_id(cpu), crate::idt::VEC_HALT);
    }
}

/// Stop this processor for good.
pub fn halt_here() -> ! {
    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nostack, nomem)) };
    }
}

/// What a processor does each time round while it waits, with interrupts
/// off, for another: the things that cannot wait for it.
#[inline]
pub fn while_waiting() {
    if HALTING.load(Ordering::Relaxed) {
        halt_here();
    }
    crate::tlb::answer();
}
