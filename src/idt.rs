//! IDT (Interrupt Descriptor Table) and exception handling for x86-64.
//!
//! Sets up handlers for all 32 CPU exceptions, a TSS for double-fault
//! recovery, and the GDT changes to support the TSS.

use crate::{console, io, pic, pit};

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

// IRQ common handler: save GPRs, call Rust handler, restore, iretq
core::arch::global_asm!(
    "irq_common:",
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
    "    addq $16, %rsp",
    "    iretq",
    options(att_syntax)
);

// Common handler: save GPRs, call Rust handler, restore, iretq
core::arch::global_asm!(
    "exception_common:",
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
    "    addq $16, %rsp",
    "    iretq",
    options(att_syntax)
);

// ---------------------------------------------------------------------------
// Rust exception handler
// ---------------------------------------------------------------------------

#[no_mangle]
extern "C" fn exception_handler(frame: &InterruptFrame) {
    let vec = frame.vector as usize;

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

#[no_mangle]
extern "C" fn irq_handler(frame: &InterruptFrame) {
    let irq = frame.vector as u8;

    match irq {
        0 => pit::tick(),
        1 => {
            // Check if a user-space handler is registered
            if crate::irq_dispatch::dispatch_irq(1) {
                unsafe { pic::send_eoi(irq) };
                return;
            }
            // No user-space handler — consume and discard the scancode
            unsafe { io::inb(0x60) };
        }
        7 => {
            // Spurious IRQ check for master PIC
            let isr = unsafe { pic::read_isr() };
            if isr & (1 << 7) == 0 {
                return; // spurious — no EOI
            }
        }
        15 => {
            // Spurious IRQ check for slave PIC
            let isr = unsafe { pic::read_isr() };
            if isr & (1 << 15) == 0 {
                // Spurious from slave — still send EOI to master
                unsafe { pic::send_eoi(0) };
                return;
            }
        }
        _ => {
            // Try user-space dispatch for all other IRQs
            if crate::irq_dispatch::dispatch_irq(irq) {
                unsafe { pic::send_eoi(irq) };
                return;
            }
        }
    }

    unsafe { pic::send_eoi(irq) };
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

extern "C" {
    static tss: u8;
    static ist1_stack_top: u8;
    static gdt64: u8;
    static gdt64_ptr: u8;
}

extern "C" {
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
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

pub unsafe fn init() {
    setup_tss();
    install_tss_in_gdt();
    reload_gdt();
    load_tss();
    setup_idt();
    load_idt();
    console::puts(b"IDT initialized.\n");
}

unsafe fn setup_tss() {
    let tss_ptr = &tss as *const u8 as *mut u8;
    core::ptr::write_bytes(tss_ptr, 0, 104);
    // IST1 at offset 36 (8 bytes)
    let ist1_addr = &ist1_stack_top as *const u8 as u64;
    core::ptr::write_unaligned(tss_ptr.add(36) as *mut u64, ist1_addr);
    // IOMAP base at offset 102 (2 bytes) — points past TSS end (no IOMAP)
    core::ptr::write_unaligned(tss_ptr.add(102) as *mut u16, 104u16);
}

/// Update TSS RSP0 — the kernel stack used for ring 3→0 transitions on
/// hardware exceptions and interrupts. Must be called whenever we switch
/// to a user-mode task so the CPU can find a valid kernel stack.
///
/// # Safety
/// `rsp0` must point to the top of a valid, mapped kernel stack.
pub unsafe fn update_tss_rsp0(rsp0: u64) {
    let tss_ptr = &tss as *const u8 as *mut u8;
    // RSP0 is at offset 4 in the x86-64 TSS
    core::ptr::write_unaligned(tss_ptr.add(4) as *mut u64, rsp0);
}

unsafe fn install_tss_in_gdt() {
    let base = &tss as *const u8 as u64;
    let limit: u64 = 103;
    let gdt_ptr = &gdt64 as *const u8 as *mut u8;
    let tss_desc = gdt_ptr.add(0x18);

    // Low 8 bytes of 16-byte TSS descriptor
    let low: u64 = (limit & 0xFFFF)
        | ((base & 0xFFFF) << 16)
        | (((base >> 16) & 0xFF) << 32)
        | (0x89u64 << 40)
        | (((base >> 24) & 0xFF) << 56);

    // High 8 bytes: upper 32 bits of base
    let high: u64 = base >> 32;

    core::ptr::write_unaligned(tss_desc as *mut u64, low);
    core::ptr::write_unaligned(tss_desc.add(8) as *mut u64, high);
}

unsafe fn reload_gdt() {
    let ptr = &gdt64_ptr as *const u8;
    core::arch::asm!(
        "lgdt ({0})",
        in(reg) ptr,
        options(att_syntax, nostack)
    );
}

unsafe fn load_tss() {
    core::arch::asm!(
        "ltr %ax",
        in("ax") 0x18u16,
        options(att_syntax, nostack, nomem)
    );
}

unsafe fn setup_idt() {
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
    let irq_stubs: [unsafe extern "C" fn(); 16] = [
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
    ];

    for i in 0..16 {
        (*idt_ptr).entries[32 + i].set_handler(irq_stubs[i] as u64, 0x08, 0);
    }
}

unsafe fn load_idt() {
    let idt_ptr = IdtPtr {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        base: &raw const IDT as u64,
    };
    core::arch::asm!(
        "lidt ({0})",
        in(reg) &idt_ptr as *const IdtPtr,
        options(att_syntax, nostack)
    );
}
