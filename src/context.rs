/// CPU context for task switching.
///
/// Stores callee-saved registers per the System V AMD64 ABI:
/// RBX, RBP, R12–R15, RSP, RIP, RFLAGS.

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct CpuContext {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rsp: u64,
    pub rip: u64,
    pub rflags: u64,
}

impl CpuContext {
    pub const fn empty() -> Self {
        CpuContext {
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rsp: 0,
            rip: 0,
            rflags: 0x200, // IF=1 (interrupts enabled)
        }
    }
}

/// Switch from `old` context to `new` context.
///
/// Saves callee-saved registers into `old`, loads from `new`, swaps RSP,
/// and "returns" into the new task by pushing RIP and using `ret`.
///
/// # Safety
/// Both pointers must point to valid CpuContext structs.
/// The new context's RSP must point to a valid stack.
#[unsafe(naked)]
pub unsafe extern "C" fn context_switch(old: *mut CpuContext, new: *const CpuContext) {
    core::arch::naked_asm!(
        // Save callee-saved registers into old context
        // old is in rdi, new is in rsi
        "mov [rdi + 0x00], rbx",
        "mov [rdi + 0x08], rbp",
        "mov [rdi + 0x10], r12",
        "mov [rdi + 0x18], r13",
        "mov [rdi + 0x20], r14",
        "mov [rdi + 0x28], r15",
        "mov [rdi + 0x30], rsp",
        // Save return address as RIP
        "lea rax, [rip + 2f]",
        "mov [rdi + 0x38], rax",
        // Save RFLAGS
        "pushfq",
        "pop rax",
        "mov [rdi + 0x40], rax",

        // Load callee-saved registers from new context
        "mov rbx, [rsi + 0x00]",
        "mov rbp, [rsi + 0x08]",
        "mov r12, [rsi + 0x10]",
        "mov r13, [rsi + 0x18]",
        "mov r14, [rsi + 0x20]",
        "mov r15, [rsi + 0x28]",
        "mov rsp, [rsi + 0x30]",

        // Where to go, read while interrupts are still off; then the new
        // context's RFLAGS, which may turn them on; then go.
        //
        // The order matters. Restoring the flags first left one instruction
        // -- the read of the new RIP -- to run with interrupts on, and an
        // interrupt taken there that rescheduled saved this task's context
        // over the one being read: when it was resumed it read the RIP of
        // *that* save, the label below, and not where it had been going. For
        // a task switched out from here the two are the same. For one that
        // had never run they are not: it returned through the label into
        // whatever was on top of its new stack, which is the trampoline that
        // ends a task, and a program that had just been started exited with
        // status 0 without running an instruction. With the address already
        // on the stack there is nothing left to read: an interrupt taken
        // before the `ret` pushes its frame below it and comes back to it.
        "push QWORD PTR [rsi + 0x38]",
        "push QWORD PTR [rsi + 0x40]",
        "popfq",

        "ret",

        // This is where we resume when switched back
        "2:",
        "ret",
    );
}
