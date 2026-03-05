/// Syscall wrappers for the Quark microkernel.
///
/// Convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5.
/// Return value in RAX.

use core::arch::asm;

// Syscall numbers
pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_SEND: u64 = 10;
pub const SYS_RECV: u64 = 11;
pub const SYS_CALL: u64 = 12;
pub const SYS_REPLY: u64 = 13;
pub const SYS_GETPID: u64 = 21;
pub const SYS_IRQ_REGISTER: u64 = 30;
pub const SYS_IRQ_ACK: u64 = 31;
pub const SYS_IOPORT: u64 = 32;
pub const SYS_MAP_PHYS: u64 = 33;
pub const SYS_TASK_CREATE: u64 = 40;
pub const SYS_ADDRSPACE_CREATE: u64 = 41;
pub const SYS_ADDRSPACE_MAP: u64 = 42;
pub const SYS_TASK_START: u64 = 43;
pub const SYS_PHYS_ALLOC: u64 = 44;
pub const SYS_PHYS_FREE: u64 = 45;
pub const SYS_GRANT_IOPORT: u64 = 46;
pub const SYS_GRANT_IRQ: u64 = 47;
pub const SYS_GRANT_CAP: u64 = 48;

pub const SYS_FD_WRITE: u64 = 50;
pub const SYS_FD_READ: u64 = 51;
pub const SYS_FD_SET: u64 = 52;

#[inline(always)]
pub unsafe fn syscall0(nr: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        out("rcx") _,
        out("rdx") _,
        out("r8") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall1(nr: u64, arg0: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") arg0,
        out("rcx") _,
        out("rdx") _,
        out("r8") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall2(nr: u64, arg0: u64, arg1: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") arg0,
        in("rsi") arg1,
        out("rcx") _,
        out("rdx") _,
        out("r8") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall3(nr: u64, arg0: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    asm!(
        "syscall",
        inlateout("rax") nr => ret,
        in("rdi") arg0,
        in("rsi") arg1,
        in("rdx") arg2,
        out("rcx") _,
        out("r8") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall4(nr: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    asm!(
        "mov r10, {arg3}",
        "syscall",
        arg3 = in(reg) arg3,
        inlateout("rax") nr => ret,
        in("rdi") arg0,
        in("rsi") arg1,
        in("rdx") arg2,
        out("rcx") _,
        out("r8") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

#[inline(always)]
pub unsafe fn syscall5(nr: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let ret: u64;
    asm!(
        "mov r10, {arg3}",
        "syscall",
        arg3 = in(reg) arg3,
        inlateout("rax") nr => ret,
        in("rdi") arg0,
        in("rsi") arg1,
        in("rdx") arg2,
        in("r8") arg4,
        out("rcx") _,
        out("r9") _,
        out("r10") _,
        out("r11") _,
        options(nostack)
    );
    ret
}

// Typed wrappers

pub fn sys_exit() -> ! {
    unsafe { syscall0(SYS_EXIT) };
    loop {
        core::hint::spin_loop();
    }
}

pub fn sys_yield() {
    unsafe { syscall0(SYS_YIELD) };
}

pub fn sys_write(buf: &[u8]) -> u64 {
    unsafe { syscall2(SYS_WRITE, buf.as_ptr() as u64, buf.len() as u64) }
}

pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

pub fn sys_send(dest: usize, msg: &crate::ipc::Message) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SEND, dest as u64, msg as *const _ as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_recv(from: usize, msg: &mut crate::ipc::Message) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_RECV, from as u64, msg as *mut _ as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_call(dest: usize, msg: &crate::ipc::Message, reply: &mut crate::ipc::Message) -> Result<(), ()> {
    let ret = unsafe {
        syscall3(SYS_CALL, dest as u64, msg as *const _ as u64, reply as *mut _ as u64)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_reply(dest: usize, msg: &crate::ipc::Message) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_REPLY, dest as u64, msg as *const _ as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_task_create() -> Result<usize, ()> {
    let ret = unsafe { syscall0(SYS_TASK_CREATE) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

pub fn sys_addrspace_create() -> Result<usize, ()> {
    let ret = unsafe { syscall0(SYS_ADDRSPACE_CREATE) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

pub fn sys_addrspace_map(cr3: usize, virt: usize, phys: usize, pages: usize, flags: u64) -> Result<(), ()> {
    let ret = unsafe {
        syscall5(
            SYS_ADDRSPACE_MAP,
            cr3 as u64,
            virt as u64,
            phys as u64,
            pages as u64,
            flags,
        )
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_task_start(tid: usize, rip: u64, rsp: u64, cr3: usize) -> Result<(), ()> {
    let ret = unsafe {
        syscall4(SYS_TASK_START, tid as u64, rip, rsp, cr3 as u64)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_phys_alloc(count: usize) -> Result<usize, ()> {
    let ret = unsafe { syscall1(SYS_PHYS_ALLOC, count as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

pub fn sys_phys_free(addr: usize, count: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_PHYS_FREE, addr as u64, count as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_ioport_read(port: u16) -> u64 {
    unsafe { syscall3(SYS_IOPORT, port as u64, 0, 0) }
}

pub fn sys_ioport_write(port: u16, val: u8) {
    unsafe { syscall3(SYS_IOPORT, port as u64, 1, val as u64) };
}

pub fn sys_irq_register(irq: u8) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_IRQ_REGISTER, irq as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_irq_ack(irq: u8) {
    unsafe { syscall1(SYS_IRQ_ACK, irq as u64) };
}

pub fn sys_map_phys(phys: usize, virt: usize, pages: usize) -> Result<(), ()> {
    let ret = unsafe { syscall3(SYS_MAP_PHYS, phys as u64, virt as u64, pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_grant_ioport(tid: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_GRANT_IOPORT, tid as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_grant_irq(tid: usize, irq: u8) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_GRANT_IRQ, tid as u64, irq as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_grant_cap(tid: usize, caps: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_GRANT_CAP, tid as u64, caps as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_fd_write(fd: usize, buf: &[u8]) -> u64 {
    unsafe { syscall3(SYS_FD_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64) }
}

pub fn sys_fd_read(fd: usize, buf: &mut [u8]) -> u64 {
    unsafe { syscall3(SYS_FD_READ, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub fn sys_fd_set(tid: usize, fd: usize, service_tid: usize, tag: u64) -> Result<(), ()> {
    let ret = unsafe { syscall4(SYS_FD_SET, tid as u64, fd as u64, service_tid as u64, tag) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

// Capability bit constants
pub const CAP_IOPORT: u32 = 1 << 0;
pub const CAP_MAP_PHYS: u32 = 1 << 1;
pub const CAP_IRQ: u32 = 1 << 2;
pub const CAP_TASK_MGMT: u32 = 1 << 3;
pub const CAP_PHYS_ALLOC: u32 = 1 << 4;
