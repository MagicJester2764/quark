//! IDT (Interrupt Descriptor Table) and exception handling for x86-64.
//!
//! Sets up handlers for all 32 CPU exceptions, the sixteen interrupt lines
//! of the 8259, and the interrupts a processor's local APIC raises: its
//! timer, and what another processor sends it. The stack a double fault is
//! taken on, and the one an interrupt from ring 3 is, are in each
//! processor's task state segment (`percpu.rs`). The table itself is one
//! for the machine; every processor loads it.

use crate::{console, io, ipc, pit, scheduler};

// ---------------------------------------------------------------------------
// Structures
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
#[repr(C, packed)]
struct IdtEntry {
    offset_lo: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_hi: u32,
    reserved: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_lo: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_hi: 0,
        reserved: 0,
    };

    fn set_handler(&mut self, addr: u64, selector: u16, ist: u8) {
        self.offset_lo = addr as u16;
        self.selector = selector;
        self.ist = ist & 0x7;
        self.type_attr = 0x8E; // present, DPL=0, 64-bit interrupt gate
        self.offset_mid = (addr >> 16) as u16;
        self.offset_hi = (addr >> 32) as u32;
        self.reserved = 0;
    }
}

#[repr(C, align(16))]
struct Idt {
    entries: [IdtEntry; 256],
}

#[repr(C, packed)]
struct IdtPtr {
    limit: u16,
    base: u64,
}

#[repr(C)]
#[allow(dead_code)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

// ---------------------------------------------------------------------------
// Static state
// ---------------------------------------------------------------------------

static mut IDT: Idt = Idt {
    entries: [IdtEntry::EMPTY; 256],
};

// ---------------------------------------------------------------------------
// Exception names
// ---------------------------------------------------------------------------

static EXCEPTION_NAMES: [&[u8]; 32] = [
    b"Divide-by-Zero",
    b"Debug",
    b"NMI",
    b"Breakpoint",
    b"Overflow",
    b"Bound Range Exceeded",
    b"Invalid Opcode",
    b"Device Not Available",
    b"Double Fault",
    b"Coprocessor Segment Overrun",
    b"Invalid TSS",
    b"Segment Not Present",
    b"Stack-Segment Fault",
    b"General Protection Fault",
    b"Page Fault",
    b"Reserved",
    b"x87 FPU Error",
    b"Alignment Check",
    b"Machine Check",
    b"SIMD Floating-Point",
    b"Virtualization",
    b"Control Protection",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Reserved",
    b"Hypervisor Injection",
    b"VMM Communication",
    b"Security",
    b"Reserved",
];

// ---------------------------------------------------------------------------
// Exception stubs (assembly)
// ---------------------------------------------------------------------------

macro_rules! exception_stub {
    (no_error, $n:literal) => {
        core::arch::global_asm!(
            concat!(
                ".global exception_stub_", stringify!($n), "\n",
                "exception_stub_", stringify!($n), ":\n",
                "    pushq $0\n",
                "    pushq $", stringify!($n), "\n",
                "    jmp exception_common\n"
            ),
            options(att_syntax)
        );
    };
    (has_error, $n:literal) => {
        core::arch::global_asm!(
            concat!(
                ".global exception_stub_", stringify!($n), "\n",
                "exception_stub_", stringify!($n), ":\n",
                "    pushq $", stringify!($n), "\n",
                "    jmp exception_common\n"
            ),
            options(att_syntax)
        );
    };
}

exception_stub!(no_error, 0);
exception_stub!(no_error, 1);
exception_stub!(no_error, 2);
exception_stub!(no_error, 3);
exception_stub!(no_error, 4);
exception_stub!(no_error, 5);
exception_stub!(no_error, 6);
exception_stub!(no_error, 7);
exception_stub!(has_error, 8);
exception_stub!(no_error, 9);
exception_stub!(has_error, 10);
exception_stub!(has_error, 11);
exception_stub!(has_error, 12);
exception_stub!(has_error, 13);
exception_stub!(has_error, 14);
exception_stub!(no_error, 15);
exception_stub!(no_error, 16);
exception_stub!(has_error, 17);
exception_stub!(no_error, 18);
exception_stub!(no_error, 19);
exception_stub!(no_error, 20);
exception_stub!(has_error, 21);
exception_stub!(no_error, 22);
exception_stub!(no_error, 23);
exception_stub!(no_error, 24);
exception_stub!(no_error, 25);
exception_stub!(no_error, 26);
exception_stub!(no_error, 27);
exception_stub!(no_error, 28);
exception_stub!(has_error, 29);
exception_stub!(has_error, 30);
exception_stub!(no_error, 31);

// ---------------------------------------------------------------------------
// IRQ stubs (assembly) — IRQ 0–15 → vectors 32–47
// ---------------------------------------------------------------------------

macro_rules! irq_stub {
    ($n:literal) => {
        core::arch::global_asm!(
            concat!(
                ".global irq_stub_", stringify!($n), "\n",
                "irq_stub_", stringify!($n), ":\n",
                "    pushq $0\n",           // dummy error code
                "    pushq $", stringify!($n), "\n", // IRQ number (0–15)
                "    jmp irq_common\n"
            ),
            options(att_syntax)
        );
    };
}

irq_stub!(0);
irq_stub!(1);
irq_stub!(2);
irq_stub!(3);
irq_stub!(4);
irq_stub!(5);
irq_stub!(6);
irq_stub!(7);
irq_stub!(8);
irq_stub!(9);
irq_stub!(10);
irq_stub!(11);
irq_stub!(12);
irq_stub!(13);
irq_stub!(14);
irq_stub!(15);
// Interrupts 16 to 47: not lines of any controller but messages a device
// sends for itself (MSI), on the vectors that follow the sixteen.
irq_stub!(16);
irq_stub!(17);
irq_stub!(18);
irq_stub!(19);
irq_stub!(20);
irq_stub!(21);
irq_stub!(22);
irq_stub!(23);
irq_stub!(24);
irq_stub!(25);
irq_stub!(26);
irq_stub!(27);
irq_stub!(28);
irq_stub!(29);
irq_stub!(30);
irq_stub!(31);
irq_stub!(32);
irq_stub!(33);
irq_stub!(34);
irq_stub!(35);
irq_stub!(36);
irq_stub!(37);
irq_stub!(38);
irq_stub!(39);
irq_stub!(40);
irq_stub!(41);
irq_stub!(42);
irq_stub!(43);
irq_stub!(44);
irq_stub!(45);
irq_stub!(46);
irq_stub!(47);

// What comes through a processor's local APIC rather than the 8259. Each
// stub pushes its own vector, which is how `irq_handler` tells these from
// the sixteen lines above and from each other.

/// A processor's own timer: its tick (`lapic::start_timer`). Every
/// processor but the first has one; the first is ticked by the 8254.
pub const VEC_TIMER: u8 = 0xE0;
/// From another processor: look at what you are running, and at what is
/// waiting to run (`smp.rs`).
pub const VEC_RESCHED: u8 = 0xE1;
/// The clock: something is due (`clock.rs`). The first processor's own
/// timer, set for whatever is due before the next tick; or another
/// processor, which has written down something due sooner than that timer
/// is set for.
pub const VEC_CLOCK: u8 = 0xE2;
/// From another processor: forget your translations (`tlb.rs`). Answered
/// without the kernel lock.
pub const VEC_FLUSH: u8 = 0xF0;
/// From another processor: the kernel has faulted, stop.
pub const VEC_HALT: u8 = 0xF1;
/// What a local APIC raises when an interrupt went away before it could be
/// delivered. Nothing is owed for it, not even an acknowledgement.
pub const VEC_SPURIOUS: u8 = 0xFF;

macro_rules! apic_stub {
    ($name:literal, $vector:literal) => {
        core::arch::global_asm!(
            concat!(
                ".global ", $name, "\n",
                $name, ":\n",
                "    pushq $0\n",
                "    pushq $", $vector, "\n",
                "    jmp irq_common\n"
            ),
            options(att_syntax)
        );
    };
}

apic_stub!("apic_stub_timer", "0xE0");
apic_stub!("apic_stub_resched", "0xE1");
apic_stub!("apic_stub_clock", "0xE2");
apic_stub!("apic_stub_flush", "0xF0");
apic_stub!("apic_stub_halt", "0xF1");
apic_stub!("apic_stub_spurious", "0xFF");

// The direction flag, on the way in.
//
// The processor delivers an interrupt or an exception with RFLAGS.DF as the
// interrupted code had it, and code may have it set: a C library sets it for
// as long as a copy that has to run backwards takes (musl's `memmove` is
// `std; rep movsb; cld`), and an interrupt or a page fault arrives where it
// arrives. The kernel is compiled to the ABI every compiler assumes, in
// which the flag is clear on entry to a function, and its `memset` and
// `memcpy` are string instructions. Entered with the flag set they run
// backwards: a tick's first `memset`, of an array on the stack, went down
// the stack over its own return address and the kernel jumped to zero; a
// page fault cleared the frame *below* the one it was handing out — somebody
// else's page — and handed over the new one still holding what its last
// owner left.
//
// So both stubs clear it before anything compiled runs. `iretq` gives the
// interrupted code its own flags back, set or not. A system call needs none
// of this: SFMASK clears the flag on `syscall`.
//
// And AC, for the same reason one flag along. It is what suspends SMAP, and
// ring 3 may set it: `popfq` there changes it like any arithmetic flag. A
// program that had would run every interrupt and fault it took with the
// kernel free to touch user pages — the protection switched off by the code
// it is there to protect against. `clac` exists only where SMAP does, so it
// is skipped where the kernel did not turn SMAP on.
//
// IRQ common handler: save GPRs, call Rust handler, restore, iretq
// Stack at entry: [vector, error_code, RIP, CS, RFLAGS, RSP, SS]
// If from user mode (CS & 3 != 0), swapgs to get kernel GS.
core::arch::global_asm!(
    "irq_common:",
    "    cld",
    "    cmpb $0, {smap}(%rip)",
    "    je 3f",
    "    clac",
    "3:",
    "    testl $3, 0x18(%rsp)",       // check CS RPL (at RSP+0x18)
    "    jz 1f",
    "    swapgs",                      // from user: swap to kernel GS
    "1:",
    "    pushq %rax",
    "    pushq %rbx",
    "    pushq %rcx",
    "    pushq %rdx",
    "    pushq %rsi",
    "    pushq %rdi",
    "    pushq %rbp",
    "    pushq %r8",
    "    pushq %r9",
    "    pushq %r10",
    "    pushq %r11",
    "    pushq %r12",
    "    pushq %r13",
    "    pushq %r14",
    "    pushq %r15",
    "",
    "    movq %rsp, %rdi",
    "    call irq_handler",
    "",
    "    popq %r15",
    "    popq %r14",
    "    popq %r13",
    "    popq %r12",
    "    popq %r11",
    "    popq %r10",
    "    popq %r9",
    "    popq %r8",
    "    popq %rbp",
    "    popq %rdi",
    "    popq %rsi",
    "    popq %rdx",
    "    popq %rcx",
    "    popq %rbx",
    "    popq %rax",
    "    addq $16, %rsp",             // skip vector + error_code
    "    testl $3, 0x08(%rsp)",       // check CS again before iretq
    "    jz 2f",
    "    swapgs",                      // returning to user: restore user GS
    "2:",
    "    iretq",
    smap = sym crate::cpu::SMAP_ENABLED,
    options(att_syntax)
);

// Common exception handler: save GPRs, call Rust handler, restore, iretq
// Stack at entry: [vector, error_code, RIP, CS, RFLAGS, RSP, SS]
// If from user mode (CS & 3 != 0), swapgs to get kernel GS.
core::arch::global_asm!(
    "exception_common:",
    "    cld",                         // both as in irq_common
    "    cmpb $0, {smap}(%rip)",
    "    je 3f",
    "    clac",
    "3:",
    "    testl $3, 0x18(%rsp)",       // check CS RPL (at RSP+0x18)
    "    jz 1f",
    "    swapgs",                      // from user: swap to kernel GS
    "1:",
    "    pushq %rax",
    "    pushq %rbx",
    "    pushq %rcx",
    "    pushq %rdx",
    "    pushq %rsi",
    "    pushq %rdi",
    "    pushq %rbp",
    "    pushq %r8",
    "    pushq %r9",
    "    pushq %r10",
    "    pushq %r11",
    "    pushq %r12",
    "    pushq %r13",
    "    pushq %r14",
    "    pushq %r15",
    "",
    "    movq %rsp, %rdi",
    "    call exception_handler",
    "",
    "    popq %r15",
    "    popq %r14",
    "    popq %r13",
    "    popq %r12",
    "    popq %r11",
    "    popq %r10",
    "    popq %r9",
    "    popq %r8",
    "    popq %rbp",
    "    popq %rdi",
    "    popq %rsi",
    "    popq %rdx",
    "    popq %rcx",
    "    popq %rbx",
    "    popq %rax",
    "    addq $16, %rsp",             // skip vector + error_code
    "    testl $3, 0x08(%rsp)",       // check CS again before iretq
    "    jz 2f",
    "    swapgs",                      // returning to user: restore user GS
    "2:",
    "    iretq",
    smap = sym crate::cpu::SMAP_ENABLED,
    options(att_syntax)
);

// ---------------------------------------------------------------------------
// Rust exception handler
// ---------------------------------------------------------------------------

/// Page fault error code bits.
const PF_PRESENT: u64 = 1 << 0;
const PF_WRITE: u64 = 1 << 1;
const PF_USER: u64 = 1 << 2;
/// A reserved bit was set in a page-table entry: never a fault that goes
/// away by itself.
const PF_RESERVED: u64 = 1 << 3;
const PF_INSN_FETCH: u64 = 1 << 4;

/// IPC tag for page fault messages sent to pager tasks.
pub const TAG_PAGE_FAULT: u64 = 0xFFFF_0001;

/// Linux's signal numbers for the exceptions a task can cause, which is what a
/// C library and a shell know how to report.
const SIGILL: i32 = 4;
const SIGTRAP: i32 = 5;
const SIGBUS: i32 = 7;
const SIGFPE: i32 = 8;
const SIGSEGV: i32 = 11;

/// The signal Linux sends for an exception taken in user mode.
fn signal_for(vec: usize) -> i32 {
    match vec {
        0 | 16 | 19 => SIGFPE,  // divide error, x87 fault, SIMD exception
        1 | 3 => SIGTRAP,       // debug, breakpoint
        6 => SIGILL,            // invalid opcode
        17 => SIGBUS,           // alignment check
        _ => SIGSEGV,           // general protection, stack, page, and the rest
    }
}

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
}

/// The rest of what a kernel fault says to serial: the registers, and the
/// words on the kernel stack that are addresses in the kernel's own code.
///
/// The kernel is built without frame pointers, so there is no chain to walk.
/// But a return address is a word on the stack that points into `.text`, and
/// nearly every such word is one: read from the faulting stack pointer
/// upwards, they are the calls that were under way, innermost first, with the
/// odd stale one from a frame since left. That is the difference between a
/// fault that names the function it happened in and `rip=0x1029`, which names
/// nothing: a jump through something that was not an address leaves only the
/// caller's return address to say where it was made.
///
/// Printed to serial and not the screen, because the screen may be a
/// compositor's by now and serial is what a test keeps.
fn report_kernel_state(frame: &InterruptFrame, kbase: usize, ktop: usize) {
    use crate::serial::{put_hex_usize, puts};
    let regs: [(&[u8], u64); 16] = [
        (b"rax", frame.rax), (b"rbx", frame.rbx), (b"rcx", frame.rcx), (b"rdx", frame.rdx),
        (b"rsi", frame.rsi), (b"rdi", frame.rdi), (b"rbp", frame.rbp), (b"r8", frame.r8),
        (b"r9", frame.r9), (b"r10", frame.r10), (b"r11", frame.r11), (b"r12", frame.r12),
        (b"r13", frame.r13), (b"r14", frame.r14), (b"r15", frame.r15), (b"rflags", frame.rflags),
    ];
    puts(b"[KFAULT regs");
    for (name, value) in regs {
        puts(b" ");
        puts(name);
        puts(b"=0x");
        put_hex_usize(value as usize);
    }
    puts(b"]\n");

    // Only a stack pointer inside the task's own kernel stack is read from:
    // one that is not is part of what went wrong, and reading through it
    // would fault again inside the report.
    let rsp = frame.rsp as usize & !7;
    if kbase == 0 || rsp < kbase || rsp >= ktop {
        puts(b"[KFAULT stack: rsp is not in the task's kernel stack]\n");
        return;
    }
    let text = core::ptr::addr_of!(__text_start) as usize..core::ptr::addr_of!(__text_end) as usize;
    puts(b"[KFAULT stack top:");
    for i in 0..8 {
        let at = rsp + i * 8;
        if at >= ktop {
            break;
        }
        puts(b" 0x");
        put_hex_usize(unsafe { core::ptr::read_volatile(at as *const usize) });
    }
    puts(b"]\n[KFAULT calls:");
    let mut at = rsp;
    let mut shown = 0;
    while at < ktop && shown < 48 {
        let word = unsafe { core::ptr::read_volatile(at as *const usize) };
        if text.contains(&word) {
            puts(b" +0x");
            put_hex_usize(at - rsp);
            puts(b"=0x");
            put_hex_usize(word);
            shown += 1;
        }
        at += 8;
    }
    puts(b"]\n");
}

/// The way into the kernel for a fault, and the way back out: the kernel
/// lock is taken unless what faulted was the kernel holding it, and what
/// was taken is given back (`klock.rs`). A fault that ends the task does
/// not come back, and one in the kernel does not come back at all.
///
/// A fault taken in ring 3 may have waited at the door for the lock, and
/// the task may have been ended or stopped by whoever held it: that is
/// looked at first (`scheduler::arrived`), and a task that was ended has
/// no fault to be told about.
#[unsafe(no_mangle)]
extern "C" fn exception_handler(frame: &InterruptFrame) {
    let took = crate::klock::enter();
    if frame.cs & 3 != 0 {
        scheduler::arrived();
    }
    exception(frame);
    crate::klock::leave(took);
}

/// End the program that touched a page it was promised and cannot be given:
/// there is no memory for it, or it is a page of a file that cannot be had.
fn no_page(cr2: u64, oom: bool) -> ! {
    let tid = scheduler::current_tid();
    crate::serial::puts(if oom { b"[OOM tid=" } else { b"[BUS tid=" });
    crate::serial::put_usize(tid);
    crate::serial::puts(b" cr2=0x");
    crate::serial::put_hex_usize(cr2 as usize);
    crate::serial::puts(b"]\n");
    console::puts(if oom {
        b"\n[kernel] Out of memory in task "
    } else {
        b"\n[kernel] Bus error in task "
    });
    print_dec(tid);
    console::puts(b" - killing task.\n");
    unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
    scheduler::exit_program(-SIGBUS)
}

fn exception(frame: &InterruptFrame) {
    let vec = frame.vector as usize;
    let from_user = frame.cs & 3 != 0;

    // A page fault that the page tables no longer agree with. The fault was
    // taken in ring 3 and then waited for the kernel; meanwhile another
    // thread of the program, on another processor, faulted on the same new
    // page and was given its memory, or the page was made writable while
    // this processor still remembered that it was not. Either way what was
    // asked for is allowed now, and the instruction is run again. Before
    // there was a second processor a fault and its handling were one step,
    // and the next thing below would call this page's reservation gone and
    // end the program.
    if vec == 14 && from_user && frame.error_code & PF_RESERVED == 0 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };
        let write = frame.error_code & PF_WRITE != 0;
        let exec = frame.error_code & PF_INSN_FETCH != 0;
        if unsafe { crate::paging::permits(crate::paging::read_cr3(), cr2 as usize, write, exec) } {
            return;
        }
    }

    // A fault on a page a mapping reserved is served, not fatal: the page is
    // given its memory and the instruction runs again. The kernel's own copies
    // to and from user memory back what they touch first, but one that did
    // not is served the same way rather than halting the machine.
    if vec == 14 && frame.error_code & PF_PRESENT == 0 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };
        let write = frame.error_code & PF_WRITE != 0;
        let cr3 = crate::paging::read_cr3();
        // A page of a mapped file may have to be asked of its pager, which
        // blocks; only a fault from user mode may wait for it. The kernel's
        // own copies page theirs in before they start.
        match unsafe { crate::paging::back(cr3, cr2 as usize, write, from_user) } {
            Ok(()) => return,
            Err(fault @ (crate::paging::Fault::NoMemory | crate::paging::Fault::Bus)) if from_user => {
                // Promised and not there to give: Linux's overcommit bargain,
                // and its answer. A page of a file that cannot be had is
                // SIGBUS too.
                no_page(cr2, matches!(fault, crate::paging::Fault::NoMemory));
            }
            Err(_) => {}
        }
    }

    // A write to a page shared since a fork is served too: the writer is
    // given a copy of its own, or the page back if nobody shares it any
    // more, and the instruction runs again. From ring 0 as from ring 3:
    // the kernel owns a page before it writes it where it can say so first
    // (`paging::back_range`), and CR0.WP is what brings the writes that
    // could not here, rather than letting them through to a frame that is
    // somebody else's as well.
    if vec == 14
        && frame.error_code & (PF_PRESENT | PF_WRITE | PF_RESERVED) == PF_PRESENT | PF_WRITE
    {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };
        let cr3 = crate::paging::read_cr3();
        match unsafe { crate::paging::own(cr3, cr2 as usize) } {
            Ok(true) => return,
            // The same bargain: a fork promised a page it had not got.
            Err(_) if from_user => no_page(cr2, true),
            _ => {}
        }
    }

    // Handle user-mode page faults: forward to pager or kill task
    if vec == 14 && from_user {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };

        let tid = scheduler::current_tid();
        let pager = scheduler::current_task_pager();

        if pager != 0 {
            // Forward fault to pager via IPC call.
            // data: [fault_addr, error_code, rip, rsp, access_flags, 0]
            // access_flags: bit 0=present, bit 1=write, bit 2=user, bit 4=insn_fetch
            let fault_msg = ipc::Message {
                sender: tid,
                tag: TAG_PAGE_FAULT,
                data: [
                    cr2,
                    frame.error_code,
                    frame.rip,
                    frame.rsp,
                    frame.error_code & (PF_PRESENT | PF_WRITE | PF_USER | PF_INSN_FETCH),
                    0,
                ],
            };
            // Enable interrupts so the pager can run (we're in an exception handler
            // with IF cleared). fault_call will block us and yield.
            unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
            ipc::fault_call(tid, pager, fault_msg);
            // Pager replied — page should be mapped. Return to retry instruction.
            return;
        }

        // No pager — kill the faulting task
        crate::serial::puts(b"[UPFAULT tid=");
        crate::serial::put_usize(tid);
        crate::serial::puts(b" cr2=0x");
        crate::serial::put_hex_usize(cr2 as usize);
        crate::serial::puts(b" rip=0x");
        crate::serial::put_hex_usize(frame.rip as usize);
        crate::serial::puts(b"]\n");
        console::puts(b"\n[kernel] Page fault in task ");
        print_dec(tid);
        console::puts(b" at ");
        print_hex(cr2);
        console::puts(b" (RIP=");
        print_hex(frame.rip);
        console::puts(b") - no pager, killing task.\n");

        unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
        // Not exit(), which reports success: a parent waiting on a task that
        // died of a fault must not be told it finished.
        scheduler::exit_program(-SIGSEGV);
    }

    // Any other exception taken in ring 3 is the task's, not the kernel's.
    //
    // This used to fall through to the fatal path below and halt the machine,
    // so one program executing a bad instruction stopped everything else —
    // and musl's `abort()` executes a bad instruction on purpose, a privileged
    // `hlt`, which made every failed assert in every C program a system halt.
    // In a microkernel of all things. The task is killed with the signal Linux
    // would have sent, negated, which is what a waiting parent sees.
    if from_user {
        let tid = scheduler::current_tid();
        let sig = signal_for(vec);
        crate::serial::puts(b"[UFAULT vec=");
        crate::serial::put_usize(vec);
        crate::serial::puts(b" tid=");
        crate::serial::put_usize(tid);
        crate::serial::puts(b" rip=0x");
        crate::serial::put_hex_usize(frame.rip as usize);
        crate::serial::puts(b" sig=");
        crate::serial::put_usize(sig as usize);
        crate::serial::puts(b"]\n");
        console::puts(b"\n[kernel] ");
        if vec < 32 {
            console::puts(EXCEPTION_NAMES[vec]);
        }
        console::puts(b" in task ");
        print_dec(tid);
        console::puts(b" at RIP=");
        print_hex(frame.rip);
        console::puts(b" - killing task.\n");

        unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
        scheduler::exit_program(-sig);
    }

    // Kernel faults: fatal, and for the whole machine. The other processors
    // are stopped first: this one has the kernel lock and is keeping it, so
    // they would get no further than the door anyway, and what follows on
    // the serial line should be one processor's account.
    crate::smp::halt_others();
    crate::serial::puts(b"[KFAULT cpu=");
    crate::serial::put_usize(crate::percpu::index());
    crate::serial::puts(b" vec=");
    crate::serial::put_usize(vec);
    crate::serial::puts(b" rip=0x");
    crate::serial::put_hex_usize(frame.rip as usize);
    crate::serial::puts(b" rsp=0x");
    crate::serial::put_hex_usize(frame.rsp as usize);
    crate::serial::puts(b" cs=0x");
    crate::serial::put_hex_usize(frame.cs as usize);
    crate::serial::puts(b" err=0x");
    crate::serial::put_hex_usize(frame.error_code as usize);
    if vec == 14 {
        let cr2_k: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2_k, options(nostack, nomem)) };
        crate::serial::puts(b" cr2=0x");
        crate::serial::put_hex_usize(cr2_k as usize);
    }
    crate::serial::puts(b" tid=");
    crate::serial::put_usize(crate::scheduler::current_tid());
    // Where the kernel stack actually is, because an rsp on its own does not
    // say whether it had run out.
    let (kbase, ktop) = crate::scheduler::current_kernel_stack();
    crate::serial::puts(b" kstack=0x");
    crate::serial::put_hex_usize(kbase);
    crate::serial::puts(b"..0x");
    crate::serial::put_hex_usize(ktop);
    crate::serial::puts(b"]\n");
    report_kernel_state(frame, kbase, ktop);
    console::puts(b"\n!!! EXCEPTION: ");
    if vec < 32 {
        console::puts(EXCEPTION_NAMES[vec]);
    } else {
        console::puts(b"Unknown");
    }
    console::puts(b" !!!\n");

    console::puts(b"Vector: ");
    print_dec(vec);
    console::puts(b"  Error code: ");
    print_hex(frame.error_code);
    console::puts(b"\n");

    console::puts(b"RIP: ");
    print_hex(frame.rip);
    console::puts(b"  CS: ");
    print_hex(frame.cs);
    console::puts(b"\n");

    console::puts(b"RSP: ");
    print_hex(frame.rsp);
    console::puts(b"  SS: ");
    print_hex(frame.ss);
    console::puts(b"\n");

    console::puts(b"RFLAGS: ");
    print_hex(frame.rflags);
    console::puts(b"\n");

    console::puts(b"RAX: ");
    print_hex(frame.rax);
    console::puts(b"  RBX: ");
    print_hex(frame.rbx);
    console::puts(b"\n");

    console::puts(b"RCX: ");
    print_hex(frame.rcx);
    console::puts(b"  RDX: ");
    print_hex(frame.rdx);
    console::puts(b"\n");

    if vec == 14 {
        let cr2: u64;
        unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nostack, nomem)) };
        console::puts(b"CR2: ");
        print_hex(cr2);
        console::puts(b"\n");
    }

    console::puts(b"\nSystem halted.\n");

    loop {
        unsafe { core::arch::asm!("cli; hlt", options(nostack, nomem)) };
    }
}

// ---------------------------------------------------------------------------
// IRQ handler
// ---------------------------------------------------------------------------

/// The way into the kernel for an interrupt, and the way back out, as
/// `exception_handler` is for a fault. One that arrives in ring 3 takes
/// the lock; one that arrives in the kernel finds it held — unless the
/// kernel was a processor with nothing to do, waiting in `hlt` without it.
///
/// Three interrupts are not ways into the kernel at all and take nothing:
/// what another processor asks that cannot wait for the lock, because the
/// one asking has it.
///
/// On the way back to ring 3 the task that was interrupted is looked at
/// again (`scheduler::arrived`): another processor may have ended it or
/// stopped it, and sent this interrupt to say so.
#[unsafe(no_mangle)]
extern "C" fn irq_handler(frame: &InterruptFrame) {
    match frame.vector as u8 {
        VEC_FLUSH => {
            crate::tlb::answer();
            crate::lapic::eoi();
            return;
        }
        VEC_HALT => crate::smp::halt_here(),
        VEC_SPURIOUS => return,
        _ => {}
    }
    let took = crate::klock::enter();
    irq(frame);
    if frame.cs & 3 != 0 {
        scheduler::arrived();
    }
    crate::klock::leave(took);
}

fn irq(frame: &InterruptFrame) {
    let irq = frame.vector as u8;

    match irq {
        VEC_TIMER => {
            // Acknowledged first, as the 8254's is and for its reason: the
            // tick may switch away, and whatever is switched to must be
            // able to be ticked.
            crate::lapic::eoi();
            scheduler::timer_tick();
            return;
        }
        VEC_RESCHED => {
            crate::lapic::eoi();
            scheduler::kicked();
            return;
        }
        VEC_CLOCK => {
            // Acknowledged first, as a tick is and for more of a reason:
            // seeing to what is due may switch away, and may not come back.
            crate::lapic::eoi();
            crate::clock::expire(true);
            // What it woke runs now if it is better than what was running,
            // and not at the next tick: that is what it was woken on time
            // for.
            scheduler::woken();
            return;
        }
        _ => {}
    }
    // A device, by its ISA number: that is what the sixteen stubs leave in
    // the frame.
    match irq {
        0 => {
            // The controller is told before pit::tick(), because tick() may
            // switch away: told afterwards, a task that then blocked would
            // leave the clock stopped until it ran again.
            crate::intc::done(0);
            pit::tick();
            return;
        }
        // An 8259 with nothing to say.
        7 | 15 if crate::intc::spurious(irq) => return,
        _ => {}
    }
    // A driver's, if one has asked for it. The controller is told the
    // driver has it (`intc::held`), and hears that the device has been
    // dealt with when the driver says so (`SYS_IRQ_ACK`): told sooner, a
    // device that holds its line until it is answered interrupts again at
    // once, and for ever.
    if crate::irq_dispatch::dispatch_irq(irq) {
        return;
    }
    if irq == 1 {
        // Nobody is reading the keyboard: what was typed is thrown away, so
        // that the controller can say when the next key is.
        unsafe { io::inb(0x60) };
    }
    crate::intc::done(irq);
}

// ---------------------------------------------------------------------------
// Print helpers
// ---------------------------------------------------------------------------

fn print_hex(val: u64) {
    console::puts(b"0x");
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 16];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        let digit = (n & 0xF) as u8;
        buf[i] = if digit < 10 {
            b'0' + digit
        } else {
            b'A' + digit - 10
        };
        n >>= 4;
        i += 1;
    }
    let mut out = [0u8; 16];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

fn print_dec(val: usize) {
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

// ---------------------------------------------------------------------------
// Extern symbols from boot.s and exception stubs
// ---------------------------------------------------------------------------

unsafe extern "C" {
    fn exception_stub_0();
    fn exception_stub_1();
    fn exception_stub_2();
    fn exception_stub_3();
    fn exception_stub_4();
    fn exception_stub_5();
    fn exception_stub_6();
    fn exception_stub_7();
    fn exception_stub_8();
    fn exception_stub_9();
    fn exception_stub_10();
    fn exception_stub_11();
    fn exception_stub_12();
    fn exception_stub_13();
    fn exception_stub_14();
    fn exception_stub_15();
    fn exception_stub_16();
    fn exception_stub_17();
    fn exception_stub_18();
    fn exception_stub_19();
    fn exception_stub_20();
    fn exception_stub_21();
    fn exception_stub_22();
    fn exception_stub_23();
    fn exception_stub_24();
    fn exception_stub_25();
    fn exception_stub_26();
    fn exception_stub_27();
    fn exception_stub_28();
    fn exception_stub_29();
    fn exception_stub_30();
    fn exception_stub_31();

    fn irq_stub_0();
    fn irq_stub_1();
    fn irq_stub_2();
    fn irq_stub_3();
    fn irq_stub_4();
    fn irq_stub_5();
    fn irq_stub_6();
    fn irq_stub_7();
    fn irq_stub_8();
    fn irq_stub_9();
    fn irq_stub_10();
    fn irq_stub_11();
    fn irq_stub_12();
    fn irq_stub_13();
    fn irq_stub_14();
    fn irq_stub_15();
    fn irq_stub_16();
    fn irq_stub_17();
    fn irq_stub_18();
    fn irq_stub_19();
    fn irq_stub_20();
    fn irq_stub_21();
    fn irq_stub_22();
    fn irq_stub_23();
    fn irq_stub_24();
    fn irq_stub_25();
    fn irq_stub_26();
    fn irq_stub_27();
    fn irq_stub_28();
    fn irq_stub_29();
    fn irq_stub_30();
    fn irq_stub_31();
    fn irq_stub_32();
    fn irq_stub_33();
    fn irq_stub_34();
    fn irq_stub_35();
    fn irq_stub_36();
    fn irq_stub_37();
    fn irq_stub_38();
    fn irq_stub_39();
    fn irq_stub_40();
    fn irq_stub_41();
    fn irq_stub_42();
    fn irq_stub_43();
    fn irq_stub_44();
    fn irq_stub_45();
    fn irq_stub_46();
    fn irq_stub_47();

    fn apic_stub_timer();
    fn apic_stub_resched();
    fn apic_stub_clock();
    fn apic_stub_flush();
    fn apic_stub_halt();
    fn apic_stub_spurious();
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

pub unsafe fn init() { unsafe {
    // This processor's descriptor table and task state segment, and then
    // the table of handlers, which is one for the machine.
    crate::percpu::load_tables();
    setup_idt();
    load_idt();
    console::puts(b"IDT initialized.\n");
}}

unsafe fn setup_idt() { unsafe {
    let stubs: [unsafe extern "C" fn(); 32] = [
        exception_stub_0,
        exception_stub_1,
        exception_stub_2,
        exception_stub_3,
        exception_stub_4,
        exception_stub_5,
        exception_stub_6,
        exception_stub_7,
        exception_stub_8,
        exception_stub_9,
        exception_stub_10,
        exception_stub_11,
        exception_stub_12,
        exception_stub_13,
        exception_stub_14,
        exception_stub_15,
        exception_stub_16,
        exception_stub_17,
        exception_stub_18,
        exception_stub_19,
        exception_stub_20,
        exception_stub_21,
        exception_stub_22,
        exception_stub_23,
        exception_stub_24,
        exception_stub_25,
        exception_stub_26,
        exception_stub_27,
        exception_stub_28,
        exception_stub_29,
        exception_stub_30,
        exception_stub_31,
    ];

    let idt_ptr = &raw mut IDT;
    for i in 0..32 {
        let ist = if i == 8 { 1 } else { 0 };
        (*idt_ptr).entries[i].set_handler(stubs[i] as u64, 0x08, ist);
    }

    // IRQ stubs at vectors 32–47
    let irq_stubs: [unsafe extern "C" fn(); 48] = [
        irq_stub_0,
        irq_stub_1,
        irq_stub_2,
        irq_stub_3,
        irq_stub_4,
        irq_stub_5,
        irq_stub_6,
        irq_stub_7,
        irq_stub_8,
        irq_stub_9,
        irq_stub_10,
        irq_stub_11,
        irq_stub_12,
        irq_stub_13,
        irq_stub_14,
        irq_stub_15,
        irq_stub_16,
        irq_stub_17,
        irq_stub_18,
        irq_stub_19,
        irq_stub_20,
        irq_stub_21,
        irq_stub_22,
        irq_stub_23,
        irq_stub_24,
        irq_stub_25,
        irq_stub_26,
        irq_stub_27,
        irq_stub_28,
        irq_stub_29,
        irq_stub_30,
        irq_stub_31,
        irq_stub_32,
        irq_stub_33,
        irq_stub_34,
        irq_stub_35,
        irq_stub_36,
        irq_stub_37,
        irq_stub_38,
        irq_stub_39,
        irq_stub_40,
        irq_stub_41,
        irq_stub_42,
        irq_stub_43,
        irq_stub_44,
        irq_stub_45,
        irq_stub_46,
        irq_stub_47,
    ];

    for (i, stub) in irq_stubs.iter().enumerate() {
        (*idt_ptr).entries[32 + i].set_handler(*stub as u64, 0x08, 0);
    }

    let apic_stubs: [(u8, unsafe extern "C" fn()); 6] = [
        (VEC_TIMER, apic_stub_timer),
        (VEC_RESCHED, apic_stub_resched),
        (VEC_CLOCK, apic_stub_clock),
        (VEC_FLUSH, apic_stub_flush),
        (VEC_HALT, apic_stub_halt),
        (VEC_SPURIOUS, apic_stub_spurious),
    ];
    for (vector, stub) in apic_stubs {
        (*idt_ptr).entries[vector as usize].set_handler(stub as u64, 0x08, 0);
    }
}}

/// Load the table of handlers on this processor. It is one table, made by
/// the first processor ([`init`]).
///
/// # Safety
/// After [`init`] has run on the first processor.
pub unsafe fn load() { unsafe {
    load_idt();
}}

unsafe fn load_idt() { unsafe {
    let idt_ptr = IdtPtr {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        base: &raw const IDT as u64,
    };
    core::arch::asm!(
        "lidt ({0})",
        in(reg) &idt_ptr as *const IdtPtr,
        options(att_syntax, nostack)
    );
}}
