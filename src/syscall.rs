/// Syscall interface for the Quark microkernel.
///
/// Uses `syscall`/`sysret` via STAR/LSTAR/SFMASK MSRs.
/// Convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5.
/// Return value in RAX.

use crate::{console, scheduler};

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_EFER: u32 = 0xC000_0080;

const KERNEL_CS: u64 = 0x08;

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_SEND: u64 = 10;
pub const SYS_RECV: u64 = 11;
pub const SYS_CALL: u64 = 12;
pub const SYS_REPLY: u64 = 13;
pub const SYS_GETPID: u64 = 21;

const SFMASK_VALUE: u64 = (1 << 9) | (1 << 10); // clear IF | DF

fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem)
        );
    }
    (hi as u64) << 32 | lo as u64
}

pub fn write_msr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nostack, nomem)
        );
    }
}

extern "C" {
    fn syscall_entry();
}

/// Initialize the syscall/sysret mechanism.
///
/// # Safety
/// Must be called after GDT is set up with user segments.
pub unsafe fn init() {
    let efer = read_msr(MSR_EFER);
    write_msr(MSR_EFER, efer | 1); // SCE

    // STAR[47:32] = kernel CS for syscall, STAR[63:48] = base for sysret
    // sysret 64-bit: CS = (base+16)|3, SS = (base+8)|3
    // base=0x20 → CS=0x33 (selector 0x30), SS=0x2B (selector 0x28)
    // GDT: [0x28]=user data, [0x30]=user code ✓
    let star = (0x0020_u64 << 48) | (KERNEL_CS << 32);
    write_msr(MSR_STAR, star);

    write_msr(MSR_LSTAR, syscall_entry as *const () as u64);
    write_msr(MSR_SFMASK, SFMASK_VALUE);

    console::puts(b"Syscall/sysret initialized.\n");
}

/// Called from assembly. Receives the syscall frame on the stack.
/// Layout: rax, rdi, rsi, rdx, r10, r8 (pushed in that order, so rax is at top).
#[no_mangle]
extern "C" fn syscall_dispatch(nr: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    match nr {
        SYS_EXIT => scheduler::exit(),
        SYS_YIELD => {
            scheduler::yield_now();
            0
        }
        SYS_WRITE => {
            let ptr = arg0 as *const u8;
            let len = arg1 as usize;
            if len > 0 && !ptr.is_null() {
                let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                console::puts(slice);
            }
            len as u64
        }
        SYS_GETPID => scheduler::current_tid() as u64,
        SYS_SEND => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            if msg_ptr.is_null() { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_send(dest, msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_RECV => {
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            if msg_ptr.is_null() { return u64::MAX; }
            match crate::ipc::sys_recv(from) {
                Ok(msg) => { unsafe { *msg_ptr = msg }; 0 }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            if msg_ptr.is_null() || reply_ptr.is_null() { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_call(dest, msg) {
                Ok(reply) => { unsafe { *reply_ptr = reply }; 0 }
                Err(_) => u64::MAX,
            }
        }
        SYS_REPLY => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            if msg_ptr.is_null() { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_reply(dest, msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        _ => u64::MAX,
    }
}

// Syscall entry in AT&T syntax.
//
// On `syscall` instruction: RCX = user RIP, R11 = user RFLAGS.
// RSP is unchanged (still user RSP). Interrupts are cleared by SFMASK.
//
// We use swapgs to access per-CPU data at %gs:0 (user RSP scratch)
// and %gs:8 (kernel RSP).
//
// After saving user context, we shuffle registers to match the C ABI
// for syscall_dispatch(nr, arg0, arg1, arg2), then sysret back.
core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    "    swapgs",
    "    movq %rsp, %gs:0",            // save user RSP
    "    movq %gs:8, %rsp",            // load kernel RSP

    // Save user context on kernel stack
    "    pushq %gs:0",                 // user RSP
    "    pushq %r11",                  // user RFLAGS
    "    pushq %rcx",                  // user RIP

    // Save registers we need to preserve across the call
    "    pushq %rbx",
    "    pushq %rbp",
    "    pushq %r12",
    "    pushq %r13",
    "    pushq %r14",
    "    pushq %r15",

    // Save syscall args (we need them after setting up C ABI)
    "    pushq %rdi",                  // arg0
    "    pushq %rsi",                  // arg1

    "    sti",                          // enable interrupts in kernel

    // Set up C ABI: syscall_dispatch(nr=rdi, arg0=rsi, arg1=rdx, arg2=rcx)
    // Current registers: rax=nr, rdi=arg0, rsi=arg1, rdx=arg2
    "    movq %rdx, %rcx",             // arg2 → rcx (4th C arg)
    "    movq %rsi, %rdx",             // arg1 → rdx (3rd C arg)
    "    movq %rdi, %rsi",             // arg0 → rsi (2nd C arg)
    "    movq %rax, %rdi",             // nr → rdi (1st C arg)
    "    call syscall_dispatch",

    // Return value is in %rax

    // Restore saved arg registers (we pushed rdi, rsi)
    "    popq %rsi",
    "    popq %rdi",

    // Restore callee-saved registers
    "    popq %r15",
    "    popq %r14",
    "    popq %r13",
    "    popq %r12",
    "    popq %rbp",
    "    popq %rbx",

    // Restore user context
    "    cli",                          // disable interrupts before sysret
    "    popq %rcx",                   // user RIP
    "    popq %r11",                   // user RFLAGS
    "    popq %rsp",                   // user RSP
    "    swapgs",
    "    sysretq",
    options(att_syntax)
);

/// Per-CPU data for syscall entry (via GS segment).
#[repr(C, align(16))]
pub struct PerCpuData {
    pub user_rsp_scratch: u64,
    pub kernel_rsp: u64,
}

static mut PER_CPU: PerCpuData = PerCpuData {
    user_rsp_scratch: 0,
    kernel_rsp: 0,
};

/// Set up GS base for per-CPU syscall data.
///
/// # Safety
/// Must be called after syscall init.
pub unsafe fn setup_percpu(kernel_stack_top: u64) {
    PER_CPU.kernel_rsp = kernel_stack_top;
    let addr = &raw const PER_CPU as u64;
    write_msr(0xC000_0101, addr); // IA32_KERNEL_GS_BASE (for swapgs)
}

/// Enter user mode via iretq.
///
/// # Safety
/// `rip` must point to valid user code, `rsp` to a valid user stack.
pub unsafe fn enter_usermode(rip: u64, rsp: u64) -> ! {
    core::arch::asm!(
        "pushq {user_ss}",             // SS
        "pushq {user_rsp}",            // RSP
        "pushfq",                       // RFLAGS (will set IF below)
        "popq %rax",
        "orq $0x200, %rax",            // set IF
        "pushq %rax",
        "pushq {user_cs}",             // CS
        "pushq {user_rip}",            // RIP
        "swapgs",                       // set up GS for next syscall
        "iretq",
        user_ss = in(reg) 0x2Bu64,     // 0x28 | 3
        user_rsp = in(reg) rsp,
        user_cs = in(reg) 0x33u64,      // 0x30 | 3
        user_rip = in(reg) rip,
        options(att_syntax, nostack, noreturn)
    );
}
