/// Syscall wrappers for the Quark microkernel.
///
/// Convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5.
/// Return value in RAX.

use core::arch::asm;

// Syscall numbers
// --- 0x00  process lifecycle ---
pub const SYS_EXIT: u64 = 0;
pub const SYS_EXIT_CODE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_WAIT: u64 = 4;
pub const SYS_TASK_KILL: u64 = 5;
pub const SYS_SIGNAL: u64 = 6;
pub const SYS_TASK_INFO: u64 = 7;

// --- 0x10  IPC ---
pub const SYS_SEND: u64 = 16;
pub const SYS_RECV: u64 = 17;
pub const SYS_CALL: u64 = 18;
pub const SYS_REPLY: u64 = 19;
pub const SYS_CALL_TIMEOUT: u64 = 20;
pub const SYS_RECV_TIMEOUT: u64 = 21;
pub const SYS_NOTIFY: u64 = 22;

// --- 0x20  memory ---
pub const SYS_MMAP: u64 = 32;
pub const SYS_MUNMAP: u64 = 33;
pub const SYS_PHYS_ALLOC: u64 = 34;
pub const SYS_PHYS_FREE: u64 = 35;
pub const SYS_ADDRSPACE_CREATE: u64 = 36;
pub const SYS_ADDRSPACE_MAP: u64 = 37;
pub const SYS_MAP_PHYS: u64 = 38;
pub const SYS_SET_MEM_LIMIT: u64 = 39;
pub const SYS_SET_PAGER: u64 = 40;
pub const SYS_ADDRSPACE_SELF: u64 = 41;

// --- 0x30  shared memory ---
pub const SYS_SHMEM_CREATE: u64 = 48;
pub const SYS_SHMEM_MAP: u64 = 49;
pub const SYS_SHMEM_UNMAP: u64 = 50;
pub const SYS_SHMEM_GRANT: u64 = 51;
pub const SYS_SHMEM_DESTROY: u64 = 52;

// --- 0x40  file descriptors and pipes ---
pub const SYS_FD_READ: u64 = 64;
pub const SYS_FD_WRITE: u64 = 65;
pub const SYS_FD_READ_NB: u64 = 66;
pub const SYS_FD_SET: u64 = 67;
pub const SYS_FD_DUP: u64 = 68;
pub const SYS_PIPE_CREATE: u64 = 69;
pub const SYS_PIPE_FD_SET: u64 = 70;

// --- 0x50  capabilities ---
pub const SYS_CAP_MINT: u64 = 80;
pub const SYS_CAP_GRANT: u64 = 81;
pub const SYS_CAP_REVOKE: u64 = 82;
pub const SYS_CAP_INSPECT: u64 = 83;
pub const SYS_CAP_DELETE: u64 = 84;
pub const SYS_CAP_TRANSFER: u64 = 85;
pub const SYS_GRANT_CAP: u64 = 86;
pub const SYS_GRANT_IOPORT: u64 = 87;
pub const SYS_GRANT_IRQ: u64 = 88;
pub const SYS_SET_USER_CAPS: u64 = 89;
pub const SYS_GET_USER_CAPS: u64 = 90;

// --- 0x60  task lifecycle and identity ---
pub const SYS_TASK_CREATE: u64 = 96;
pub const SYS_TASK_START: u64 = 97;
pub const SYS_GET_UID: u64 = 98;
pub const SYS_SET_UID: u64 = 99;
pub const SYS_SET_GID: u64 = 100;
pub const SYS_GET_TUID: u64 = 101;
pub const SYS_SET_FS_BASE: u64 = 102;
pub const SYS_TASK_START_ARG: u64 = 103;
pub const SYS_TASK_WATCH: u64 = 104;

// --- 0x70  hardware and drivers ---
pub const SYS_IRQ_REGISTER: u64 = 112;
pub const SYS_IRQ_ACK: u64 = 113;
pub const SYS_IOPORT: u64 = 114;
pub const SYS_IOPORT_REP: u64 = 115;

// --- 0x80  synchronisation ---
pub const SYS_FUTEX_WAIT: u64 = 128;
pub const SYS_FUTEX_WAKE: u64 = 129;
pub const SYS_FUTEX_WAIT_TIMEOUT: u64 = 130;

// --- 0x90  time ---
pub const SYS_TICKS: u64 = 144;

// --- 0xB0  sockets ---
pub const SYS_SOCK_FD: u64 = 176;
pub const SYS_SOCK_INFO: u64 = 177;

// --- 0xA0  kernel debug console ---
pub const SYS_WRITE: u64 = 160;
pub const SYS_CONSOLE_POS: u64 = 161;

// --- 0xF0  ABI introspection ---
pub const SYS_ABI_VERSION: u64 = 240;







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
        inlateout("rdi") arg0 => _,
        out("rcx") _,
        out("rdx") _,
        out("rsi") _,
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
        inlateout("rdi") arg0 => _,
        inlateout("rsi") arg1 => _,
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
        inlateout("rdi") arg0 => _,
        inlateout("rsi") arg1 => _,
        inlateout("rdx") arg2 => _,
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
        inlateout("rdi") arg0 => _,
        inlateout("rsi") arg1 => _,
        inlateout("rdx") arg2 => _,
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
        inlateout("rdi") arg0 => _,
        inlateout("rsi") arg1 => _,
        inlateout("rdx") arg2 => _,
        inlateout("r8") arg4 => _,
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

/// Exit with a status code, reported to a parent waiting in `sys_wait`.
pub fn sys_exit_code(code: i32) -> ! {
    unsafe { syscall1(SYS_EXIT_CODE, code as u32 as u64) };
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

/// Returns (row, col) of the kernel console cursor.
pub fn sys_console_pos() -> (usize, usize) {
    let ret = unsafe { syscall0(SYS_CONSOLE_POS) };
    let row = (ret >> 32) as usize;
    let col = (ret & 0xFFFF_FFFF) as usize;
    (row, col)
}

pub fn sys_getpid() -> u64 {
    unsafe { syscall0(SYS_GETPID) }
}

pub fn sys_get_uid() -> (u32, u32) {
    let ret = unsafe { syscall0(SYS_GET_UID) };
    let uid = (ret >> 32) as u32;
    let gid = (ret & 0xFFFF_FFFF) as u32;
    (uid, gid)
}

pub fn sys_set_uid(tid: usize, uid: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SET_UID, tid as u64, uid as u64) };
    if ret == 0 { Ok(()) } else { Err(()) }
}

pub fn sys_set_gid(tid: usize, gid: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SET_GID, tid as u64, gid as u64) };
    if ret == 0 { Ok(()) } else { Err(()) }
}

pub fn sys_get_tuid(tid: usize) -> Result<(u32, u32), ()> {
    let ret = unsafe { syscall1(SYS_GET_TUID, tid as u64) };
    if ret == u64::MAX { return Err(()); }
    let uid = (ret >> 32) as u32;
    let gid = (ret & 0xFFFF_FFFF) as u32;
    Ok((uid, gid))
}

pub fn sys_task_kill(tid: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_TASK_KILL, tid as u64) };
    if ret == 0 { Ok(()) } else { Err(()) }
}

/// Returns (state, parent_tid, uid) or Err if no task at that TID.
/// state: 0=Ready, 1=Running, 2=Blocked, 3=Dead
/// Ask to be told when `tid` dies.
///
/// The notification arrives at the caller's next receive as a message from the
/// kernel: sender 0, tag [`crate::ipc::TAG_TASK_DIED`], `data[0]` the task
/// that died. It is the answer to "who still holds this?" for anything lent
/// out — a display, a window, the keyboard — without polling for it.
///
/// Fails if `tid` is already dead, which is an answer rather than a problem:
/// there is nothing to wait for and the caller may reclaim at once.
pub fn sys_task_watch(tid: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_TASK_WATCH, tid as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_task_info(tid: usize) -> Result<(u8, usize, u32), ()> {
    let ret = unsafe { syscall1(SYS_TASK_INFO, tid as u64) };
    if ret == u64::MAX { return Err(()); }
    let state = (ret & 0xF) as u8;
    let parent = ((ret >> 4) & 0x0FFF_FFFF) as usize;
    let uid = (ret >> 32) as u32;
    Ok((state, parent, uid))
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

/// Outcome of a call that carries a deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallOutcome {
    /// The target replied; `reply` holds its message.
    Replied,
    /// The deadline passed with no reply.
    TimedOut,
    /// The call could not be made at all (bad target, no capability, dead task).
    Failed,
}

/// Synchronous call that gives up after `timeout_ticks` (100 Hz, so 10ms each).
///
/// Use this rather than [`sys_call`] for any destination that is not known to
/// be a running server: a task that never reaches sys_recv leaves a plain
/// sys_call blocked forever.
pub fn sys_call_timeout(
    dest: usize,
    msg: &crate::ipc::Message,
    reply: &mut crate::ipc::Message,
    timeout_ticks: u64,
) -> CallOutcome {
    let ret = unsafe {
        syscall4(
            SYS_CALL_TIMEOUT,
            dest as u64,
            msg as *const _ as u64,
            reply as *mut _ as u64,
            timeout_ticks,
        )
    };
    match ret {
        0 => CallOutcome::Replied,
        1 => CallOutcome::TimedOut,
        _ => CallOutcome::Failed,
    }
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

pub fn sys_ioport_read16(port: u16) -> u16 {
    unsafe { syscall3(SYS_IOPORT, port as u64, 2, 0) as u16 }
}

pub fn sys_ioport_write16(port: u16, val: u16) {
    unsafe { syscall3(SYS_IOPORT, port as u64, 3, val as u64) };
}

pub fn sys_ioport_read32(port: u16) -> u32 {
    unsafe { syscall3(SYS_IOPORT, port as u64, 4, 0) as u32 }
}

pub fn sys_ioport_write32(port: u16, val: u32) {
    unsafe { syscall3(SYS_IOPORT, port as u64, 5, val as u64) };
}

pub fn sys_ioport_rep_insw(port: u16, buf: &mut [u16]) -> Result<(), ()> {
    let ret = unsafe {
        syscall4(SYS_IOPORT_REP, port as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_ioport_rep_outsw(port: u16, buf: &[u16]) -> Result<(), ()> {
    let ret = unsafe {
        syscall4(SYS_IOPORT_REP, port as u64, buf.as_ptr() as u64, buf.len() as u64, 1)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
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

/// Non-blocking read from a pipe fd.
/// Returns bytes read, 0 for EOF, 0xFFFF_FFFE if would block, u64::MAX on error.
pub fn sys_fd_read_nb(fd: usize, buf: &mut [u8]) -> u64 {
    unsafe { syscall3(SYS_FD_READ_NB, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64) }
}

pub const WOULD_BLOCK: u64 = 0xFFFF_FFFE;

pub fn sys_fd_set(tid: usize, fd: usize, service_tid: usize, tag: u64) -> Result<(), ()> {
    let ret = unsafe { syscall4(SYS_FD_SET, tid as u64, fd as u64, service_tid as u64, tag) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Bind a net-server connection handle to a file descriptor.
///
/// Returns the descriptor, which then reads and writes with the ordinary
/// `sys_fd_read` and `sys_fd_write` — the point of the exercise.
pub fn sys_sock_fd(net_tid: usize, handle: usize) -> Result<usize, ()> {
    let r = unsafe { syscall2(SYS_SOCK_FD, net_tid as u64, handle as u64) };
    if r == u64::MAX { Err(()) } else { Ok(r as usize) }
}

/// The `(net_tid, handle)` behind a socket fd, for closing the connection.
pub fn sys_sock_info(fd: usize) -> Result<(usize, usize), ()> {
    let r = unsafe { syscall1(SYS_SOCK_INFO, fd as u64) };
    if r == u64::MAX {
        Err(())
    } else {
        Ok(((r >> 32) as usize, (r & 0xFFFF_FFFF) as usize))
    }
}

pub fn sys_futex_wait(addr: *const u32, expected: u32) -> u64 {
    unsafe { syscall2(SYS_FUTEX_WAIT, addr as u64, expected as u64) }
}

pub fn sys_futex_wake(addr: *const u32, max_wake: usize) -> u64 {
    unsafe { syscall2(SYS_FUTEX_WAKE, addr as u64, max_wake as u64) }
}

/// Returned by [`sys_futex_wait_timeout`] when the deadline passed.
pub const FUTEX_TIMED_OUT: u64 = 2;

/// Wait, giving up after `timeout_ticks` (100 Hz, so 10 ms each).
///
/// Returns 0 if woken, 1 if the word already differed, [`FUTEX_TIMED_OUT`] if
/// the deadline passed. A timeout of 0 checks the word without blocking.
pub fn sys_futex_wait_timeout(addr: *const u32, expected: u32, timeout_ticks: u64) -> u64 {
    unsafe { syscall3(SYS_FUTEX_WAIT_TIMEOUT, addr as u64, expected as u64, timeout_ticks) }
}

/// Receive with timeout.
/// Returns Ok(()) if a message was received (written to `msg`),
/// Err(1) on timeout, Err(u64::MAX) on error.
pub fn sys_recv_timeout(from: usize, msg: &mut crate::ipc::Message, timeout_ticks: u64) -> Result<(), u64> {
    let ret = unsafe {
        syscall3(SYS_RECV_TIMEOUT, from as u64, msg as *mut _ as u64, timeout_ticks)
    };
    match ret {
        0 => Ok(()),
        other => Err(other),
    }
}

/// Read the kernel PIT tick counter (100 Hz, 10 ms per tick).
pub fn sys_ticks() -> u64 {
    unsafe { syscall0(SYS_TICKS) }
}

/// Sleep for `ticks` PIT ticks (each tick = 10 ms at 100 Hz).
pub fn sleep_ticks(ticks: u64) {
    if ticks == 0 {
        return;
    }
    // Block by doing a recv_timeout from our own TID — nobody will send to us specifically,
    // so it always times out after the deadline.
    let from = sys_getpid() as usize;
    let mut msg = crate::ipc::Message::empty();
    let _ = sys_recv_timeout(from, &mut msg, ticks);
}

/// Sleep for approximately `ms` milliseconds.
pub fn sleep_ms(ms: u64) {
    // PIT runs at 100 Hz → 1 tick = 10 ms. Round up.
    let ticks = (ms + 9) / 10;
    sleep_ticks(ticks);
}

/// Set the memory limit (in pages) for a task. 0 = unlimited.
/// Requires CAP_TASK_MGMT.
pub fn sys_set_mem_limit(tid: usize, limit_pages: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SET_MEM_LIMIT, tid as u64, limit_pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Create a shared memory region. Returns a handle on success.
pub fn sys_shmem_create(pages: usize) -> Result<usize, ()> {
    let ret = unsafe { syscall1(SYS_SHMEM_CREATE, pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Map a shared memory region into the caller's address space.
/// vaddr must be page-aligned and in user space (>= 0x80_0000_0000).
pub fn sys_shmem_map(handle: usize, vaddr: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SHMEM_MAP, handle as u64, vaddr as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Grant another task access to a shared memory region.
/// Must be the region's creator or have CAP_TASK_MGMT.
pub fn sys_shmem_grant(handle: usize, target_tid: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SHMEM_GRANT, handle as u64, target_tid as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Unmap a shared memory region from the caller's address space.
pub fn sys_shmem_unmap(handle: usize, vaddr: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SHMEM_UNMAP, handle as u64, vaddr as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Destroy a shared memory region, freeing physical pages and reclaiming the handle.
/// Must be the region's creator or have CAP_TASK_MGMT.
pub fn sys_shmem_destroy(handle: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_SHMEM_DESTROY, handle as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Wait for a child task to exit. Returns `(tid, exit_code)`.
/// Returns Err(()) if the caller has no children.
pub fn sys_wait() -> Result<(usize, i32), ()> {
    let ret = unsafe { syscall0(SYS_WAIT) };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(((ret & 0xFFFF_FFFF) as usize, (ret >> 32) as u32 as i32))
    }
}

/// Set the pager task for exception forwarding (requires CAP_TASK_MGMT).
/// Page faults in the target task will be forwarded to pager_tid via IPC.
pub fn sys_set_pager(tid: usize, pager_tid: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SET_PAGER, tid as u64, pager_tid as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Send an asynchronous notification to a task.
/// `badge` bits are OR'd into the target's notification word (non-blocking).
/// The target receives a message with tag=TAG_NOTIFICATION and data[0]=accumulated word.
pub fn sys_notify(dest: usize, badge: u64) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_NOTIFY, dest as u64, badge) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// IPC tag for notification messages from the kernel.
/// data[0] = notification word (accumulated OR of all badges since last consume).
pub const TAG_NOTIFICATION: u64 = 0xFFFF_0002;

/// IPC tag for page fault messages from the kernel.
/// data: [fault_addr, error_code, rip, rsp, access_flags, 0]
pub const TAG_PAGE_FAULT: u64 = 0xFFFF_0001;

/// Map anonymous memory into the caller's address space.
/// `vaddr` must be page-aligned and in user space (>= 0x80_0000_0000).
/// Returns 0 on success, u64::MAX on failure.
pub fn sys_mmap(vaddr: usize, pages: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_MMAP, vaddr as u64, pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Unmap pages from the caller's address space and free their physical frames.
/// `vaddr` must be page-aligned and in user space (>= 0x80_0000_0000).
/// Returns the number of pages actually freed, or u64::MAX on invalid arguments.
pub fn sys_munmap(vaddr: usize, pages: usize) -> Result<usize, ()> {
    let ret = unsafe { syscall2(SYS_MUNMAP, vaddr as u64, pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Transfer capabilities to another task. The caller must hold all bits in `caps`.
/// Unlike sys_grant_cap, this does NOT require CAP_TASK_MGMT.
pub fn sys_cap_transfer(dest: usize, caps: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_CAP_TRANSFER, dest as u64, caps as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Create a kernel pipe. Returns the pipe handle on success.
pub fn sys_pipe_create() -> Result<usize, ()> {
    let ret = unsafe { syscall0(SYS_PIPE_CREATE) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Install a pipe endpoint as a file descriptor on a task.
/// is_write: false = read end, true = write end.
/// Requires CAP_TASK_MGMT.
pub fn sys_pipe_fd_set(tid: usize, fd: usize, pipe_handle: usize, is_write: bool) -> Result<(), ()> {
    let ret = unsafe {
        syscall4(SYS_PIPE_FD_SET, tid as u64, fd as u64, pipe_handle as u64, is_write as u64)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Duplicate the caller's source fd onto a target task's target fd.
/// Handles pipe refcounting automatically. Requires CAP_TASK_MGMT.
pub fn sys_fd_dup(target_tid: usize, target_fd: usize, source_fd: usize) -> Result<(), ()> {
    let ret = unsafe {
        syscall3(SYS_FD_DUP, target_tid as u64, target_fd as u64, source_fd as u64)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

// Signal constants (badge bits, high to avoid collision with app notifications)
pub const SIG_INT: u64 = 1 << 16;
pub const SIG_TERM: u64 = 1 << 17;
pub const SIG_KILL: u64 = 1 << 18;
pub const SIG_MASK: u64 = SIG_INT | SIG_TERM | SIG_KILL;

/// Send a signal to a task. SIG_KILL is immediate; SIG_INT/SIG_TERM give the
/// task a 2-second grace period to handle the signal before being force-killed.
/// Same permissions as sys_task_kill (CAP_TASK_MGMT or same UID).
pub fn sys_signal(tid: usize, sig: u64) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SIGNAL, tid as u64, sig) };
    if ret == 0 { Ok(()) } else { Err(()) }
}

// Object capability syscalls

// Object capability types
pub const CAP_TYPE_IOPORT: u64 = 1;
pub const CAP_TYPE_PHYS_RANGE: u64 = 2;
pub const CAP_TYPE_IRQ: u64 = 3;
pub const CAP_TYPE_TASK_MGMT: u64 = 4;
pub const CAP_TYPE_PHYS_ALLOC: u64 = 5;
pub const CAP_TYPE_SET_UID: u64 = 6;
/// Endpoint: param0 is a bitmask of destination TIDs this task may
/// sys_send / sys_call / sys_notify. Bit N = TID N.
pub const CAP_TYPE_ENDPOINT: u64 = 7;

/// CSpace slot conventions shared by init, login and the shell.
///
/// The endpoint capability lives at a fixed slot so a parent can delegate it
/// to a child with sys_cap_grant without having to know the mask it carries;
/// sys_cap_inspect truncates params and cannot report a 64-bit set.
pub const SLOT_ENDPOINT: usize = 15;
/// Second endpoint slot, used by init to top up services that were started
/// before their peers existed.
pub const SLOT_ENDPOINT_EXTRA: usize = 13;
/// Scratch slot used for the mint-grant-delete idiom.
pub const SLOT_SCRATCH: usize = 14;

/// Mint a new capability in the caller's CSpace slot.
/// The caller must already hold a cap of the same type whose params are a superset.
pub fn sys_cap_mint(slot: usize, cap_type: u64, param0: u64, param1: u64) -> Result<(), ()> {
    let ret = unsafe { syscall4(SYS_CAP_MINT, slot as u64, cap_type, param0, param1) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Delegate a capability from src_slot to dest_tid's dest_slot.
pub fn sys_cap_grant(dest_tid: usize, src_slot: usize, dest_slot: usize) -> Result<(), ()> {
    let ret = unsafe { syscall3(SYS_CAP_GRANT, dest_tid as u64, src_slot as u64, dest_slot as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Revoke a capability slot, invalidating all derived caps.
pub fn sys_cap_revoke(slot: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_CAP_REVOKE, slot as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Inspect a capability slot. Returns packed info:
/// bits [7:0] = type, [23:8] = param0 low 16, [39:24] = param1 low 16.
/// Returns u64::MAX on error.
pub fn sys_cap_inspect(slot: usize) -> Result<(u8, u16, u16), ()> {
    let ret = unsafe { syscall1(SYS_CAP_INSPECT, slot as u64) };
    if ret == u64::MAX { return Err(()); }
    let cap_type = (ret & 0xFF) as u8;
    let param0 = ((ret >> 8) & 0xFFFF) as u16;
    let param1 = ((ret >> 24) & 0xFFFF) as u16;
    Ok((cap_type, param0, param1))
}

/// Delete a capability from the caller's CSpace slot.
pub fn sys_cap_delete(slot: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_CAP_DELETE, slot as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Set the per-user default capability bitmask for a UID.
/// Requires CAP_SET_UID (root gets it automatically via UID bypass).
pub fn sys_set_user_caps(uid: u32, caps: u32) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_SET_USER_CAPS, uid as u64, caps as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Query the per-user default capability bitmask for a UID.
pub fn sys_get_user_caps(uid: u32) -> u32 {
    unsafe { syscall1(SYS_GET_USER_CAPS, uid as u64) as u32 }
}

// Capability bit constants
pub const CAP_IOPORT: u32 = 1 << 0;
pub const CAP_MAP_PHYS: u32 = 1 << 1;
pub const CAP_IRQ: u32 = 1 << 2;
pub const CAP_TASK_MGMT: u32 = 1 << 3;
pub const CAP_PHYS_ALLOC: u32 = 1 << 4;
pub const CAP_SET_UID: u32 = 1 << 5;
pub const CAP_ENDPOINT: u32 = 1 << 6;

/// Syscall ABI version the running kernel implements, as (major, minor).
///
/// A program that cares should check the major and refuse to run against one
/// it was not built for; minor only ever grows by addition.
pub fn sys_abi_version() -> (u32, u32) {
    let v = unsafe { syscall0(SYS_ABI_VERSION) };
    ((v >> 16) as u32, (v & 0xFFFF) as u32)
}

/// The address space this task is running in.
///
/// Needed to start a thread, which is a task started with the same cr3. It
/// conveys no authority: the caller is already executing there.
pub fn sys_addrspace_self() -> Result<usize, ()> {
    let ret = unsafe { syscall0(SYS_ADDRSPACE_SELF) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Set this task's FS base, where its thread-locals live.
///
/// Per task and self-directed, so it needs no capability: a task can already
/// write any of its own memory. Takes effect immediately, not at the next
/// context switch.
pub fn sys_set_fs_base(base: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_SET_FS_BASE, base as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// As [`sys_task_start`], but hands `arg` to the entry point in RDI.
pub fn sys_task_start_arg(
    tid: usize,
    rip: u64,
    rsp: u64,
    cr3: usize,
    arg: u64,
) -> Result<(), ()> {
    let ret = unsafe {
        syscall5(SYS_TASK_START_ARG, tid as u64, rip, rsp, cr3 as u64, arg)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}
