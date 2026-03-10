/// Syscall interface for the Quark microkernel.
///
/// Uses `syscall`/`sysret` via STAR/LSTAR/SFMASK MSRs.
/// Convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5.
/// Return value in RAX.

use crate::{console, paging, scheduler};

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_EFER: u32 = 0xC000_0080;

const KERNEL_CS: u64 = 0x08;

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_WRITE: u64 = 2;
pub const SYS_CONSOLE_POS: u64 = 3;
pub const SYS_SEND: u64 = 10;
pub const SYS_RECV: u64 = 11;
pub const SYS_CALL: u64 = 12;
pub const SYS_REPLY: u64 = 13;
pub const SYS_GETPID: u64 = 21;
pub const SYS_IRQ_REGISTER: u64 = 30;
pub const SYS_IRQ_ACK: u64 = 31;
pub const SYS_IOPORT: u64 = 32;
pub const SYS_MAP_PHYS: u64 = 33;
pub const SYS_IOPORT_REP: u64 = 34;

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
pub const SYS_PIPE_CREATE: u64 = 53;
pub const SYS_PIPE_FD_SET: u64 = 54;
pub const SYS_FD_DUP: u64 = 55;

pub const SYS_FUTEX_WAIT: u64 = 60;
pub const SYS_FUTEX_WAKE: u64 = 61;

pub const SYS_MMAP: u64 = 70;

pub const SYS_RECV_TIMEOUT: u64 = 80;
pub const SYS_TICKS: u64 = 81;
pub const SYS_SET_PAGER: u64 = 82;
pub const SYS_WAIT: u64 = 83;
pub const SYS_SET_MEM_LIMIT: u64 = 84;
pub const SYS_NOTIFY: u64 = 85;

pub const SYS_SHMEM_CREATE: u64 = 90;
pub const SYS_SHMEM_MAP: u64 = 91;
pub const SYS_SHMEM_GRANT: u64 = 92;
pub const SYS_CAP_TRANSFER: u64 = 93;

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

const USER_ADDR_LIMIT: u64 = 0x0000_8000_0000_0000;

/// Validate that a user pointer range is entirely in user space.
fn validate_user_ptr(addr: u64, len: u64) -> bool {
    addr.checked_add(len).map_or(false, |end| end <= USER_ADDR_LIMIT)
}

/// Maximum bytes per IPC write message (5 data words × 8 bytes).
const FD_WRITE_MAX_CHUNK: usize = 40;

/// Send a write via IPC to a service, chunking data into 40-byte messages.
/// Returns bytes written.
fn fd_write_ipc(target_tid: usize, tag: u64, ptr: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let buf = unsafe { core::slice::from_raw_parts(ptr, len) };
    let mut offset = 0usize;
    while offset < len {
        let chunk = (len - offset).min(FD_WRITE_MAX_CHUNK);
        // Pack bytes into data[1..6]
        let mut data = [0u64; 6];
        data[0] = chunk as u64;
        for i in 0..5 {
            let base = i * 8;
            let mut w = [0u8; 8];
            for j in 0..8 {
                if base + j < chunk {
                    w[j] = buf[offset + base + j];
                }
            }
            data[i + 1] = u64::from_le_bytes(w);
        }
        let msg = crate::ipc::Message {
            sender: 0,
            tag,
            data,
        };
        match crate::ipc::sys_call(target_tid, &msg) {
            Ok(_) => {}
            Err(_) => return offset as u64,
        }
        offset += chunk;
    }
    len as u64
}

/// Send a read request via IPC to a service, copy response into user buffer.
/// Returns bytes read, or u64::MAX on error.
fn fd_read_ipc(target_tid: usize, tag: u64, ptr: *mut u8, max_len: usize) -> u64 {
    let request_len = max_len.min(FD_WRITE_MAX_CHUNK);
    let msg = crate::ipc::Message {
        sender: 0,
        tag,
        data: [request_len as u64, 0, 0, 0, 0, 0],
    };
    match crate::ipc::sys_call(target_tid, &msg) {
        Ok(reply) => {
            let actual = (reply.data[0] as usize).min(request_len);
            // Unpack bytes from reply.data[1..6]
            let buf = unsafe { core::slice::from_raw_parts_mut(ptr, actual) };
            for i in 0..5 {
                let base = i * 8;
                let bytes = reply.data[i + 1].to_le_bytes();
                for j in 0..8 {
                    if base + j < actual {
                        buf[base + j] = bytes[j];
                    }
                }
            }
            actual as u64
        }
        Err(_) => u64::MAX,
    }
}

/// Called from assembly with 6 args mapped from user registers.
#[no_mangle]
extern "C" fn syscall_dispatch(
    nr: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> u64 {
    match nr {
        SYS_EXIT => scheduler::exit(),
        SYS_YIELD => {
            scheduler::yield_now();
            0
        }
        SYS_WRITE => {
            let ptr = arg0 as *const u8;
            let len = arg1 as usize;
            if len > 0 && !ptr.is_null() && validate_user_ptr(arg0, arg1) {
                let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                console::puts(slice);
            } else if !validate_user_ptr(arg0, arg1) {
                return u64::MAX;
            }
            len as u64
        }
        SYS_CONSOLE_POS => {
            let (row, col) = console::cursor_pos_and_disable();
            ((row as u64) << 32) | (col as u64)
        }
        SYS_GETPID => scheduler::current_tid() as u64,
        SYS_SEND => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if msg_ptr.is_null() || !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_send(dest, msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_RECV => {
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if msg_ptr.is_null() || !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv(from) {
                Ok(msg) => { unsafe { *msg_ptr = msg }; 0 }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if msg_ptr.is_null() || reply_ptr.is_null()
                || !validate_user_ptr(arg1, msg_size)
                || !validate_user_ptr(arg2, msg_size) { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_call(dest, msg) {
                Ok(reply) => { unsafe { *reply_ptr = reply }; 0 }
                Err(_) => u64::MAX,
            }
        }
        SYS_REPLY => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if msg_ptr.is_null() || !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = unsafe { &*msg_ptr };
            match crate::ipc::sys_reply(dest, msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_IRQ_REGISTER => {
            if !scheduler::current_task_has_cap(crate::task::CAP_IRQ) {
                return u64::MAX;
            }
            let irq = arg0 as u8;
            let tid = scheduler::current_tid();
            crate::irq_dispatch::register_irq_handler(irq, tid);
            unsafe { crate::pic::enable_irq(irq) };
            0
        }
        SYS_IRQ_ACK => {
            // arg0 = IRQ number
            let irq = arg0 as u8;
            unsafe { crate::pic::send_eoi(irq) };
            0
        }
        SYS_IOPORT => {
            // arg0=port, arg1=op (0=read8,1=write8,2=read16,3=write16,4=read32,5=write32), arg2=value (for writes)
            if !scheduler::current_task_has_cap(crate::task::CAP_IOPORT) {
                return u64::MAX;
            }
            let port = arg0 as u16;
            match arg1 {
                0 => unsafe { crate::io::inb(port) as u64 },
                1 => { unsafe { crate::io::outb(port, arg2 as u8) }; 0 }
                2 => unsafe { crate::io::inw(port) as u64 },
                3 => { unsafe { crate::io::outw(port, arg2 as u16) }; 0 }
                4 => unsafe { crate::io::inl(port) as u64 },
                5 => { unsafe { crate::io::outl(port, arg2 as u32) }; 0 }
                _ => u64::MAX,
            }
        }
        SYS_IOPORT_REP => {
            // arg0=port, arg1=user_buf_ptr, arg2=count (words), arg3=op (0=insw, 1=outsw)
            if !scheduler::current_task_has_cap(crate::task::CAP_IOPORT) {
                return u64::MAX;
            }
            let port = arg0 as u16;
            let buf = arg1;
            let count = arg2 as usize;
            let op = arg3;
            if count == 0 {
                return 0;
            }
            if !validate_user_ptr(buf, (count as u64).saturating_mul(2)) {
                return u64::MAX;
            }
            match op {
                0 => {
                    unsafe { crate::io::rep_insw(port, buf as *mut u16, count) };
                    0
                }
                1 => {
                    unsafe { crate::io::rep_outsw(port, buf as *const u16, count) };
                    0
                }
                _ => u64::MAX,
            }
        }
        SYS_MAP_PHYS => {
            if !scheduler::current_task_has_cap(crate::task::CAP_MAP_PHYS) {
                return u64::MAX;
            }
            let phys = arg0 as usize;
            let virt = arg1 as usize;
            let pages = arg2 as usize;
            let pml4 = paging::read_cr3();
            for i in 0..pages {
                let p = phys + i * 4096;
                let v = virt + i * 4096;
                let flags = paging::PRESENT | paging::WRITABLE | paging::USER;
                if unsafe { paging::map_page(pml4, v, p, flags) }.is_err() {
                    return u64::MAX;
                }
            }
            0
        }
        SYS_TASK_CREATE => {
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            match scheduler::create_empty_task() {
                Some(tid) => tid as u64,
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_CREATE => {
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            match crate::userspace::create_address_space() {
                Some(cr3) => cr3 as u64,
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_MAP => {
            // arg0=cr3, arg1=virt, arg2=phys, arg3=pages, arg4=flags
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let cr3 = arg0 as usize;
            let virt = arg1 as usize;
            let phys = arg2 as usize;
            let pages = arg3 as usize;
            let flags = arg4;
            let pte_flags = paging::PRESENT | paging::USER
                | if flags & 1 != 0 { paging::WRITABLE } else { 0 };
            for i in 0..pages {
                let v = virt + i * 4096;
                let p = phys + i * 4096;
                if unsafe { paging::map_page(cr3, v, p, pte_flags) }.is_err() {
                    return u64::MAX;
                }
            }
            0
        }
        SYS_TASK_START => {
            // arg0=tid, arg1=rip, arg2=rsp, arg3=cr3
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let rip = arg1;
            let rsp = arg2;
            let cr3 = arg3 as usize;
            match scheduler::start_task(tid, rip, rsp, cr3) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_PHYS_ALLOC => {
            // arg0 = number of contiguous pages to allocate
            if !scheduler::current_task_has_cap(crate::task::CAP_PHYS_ALLOC) {
                return u64::MAX;
            }
            let count = arg0 as usize;
            if count == 0 {
                return u64::MAX;
            }
            // Check memory quota
            if !scheduler::current_task_check_mem(count) {
                return u64::MAX;
            }
            // For simplicity, allocate pages one at a time and return the first
            // (Only single-page alloc is reliable with bitmap allocator)
            if count == 1 {
                match crate::pmm::alloc() {
                    Some(frame) => {
                        scheduler::current_task_charge_mem(1);
                        frame.address() as u64
                    }
                    None => u64::MAX,
                }
            } else {
                // Allocate count pages, return first address
                // Caller must accept they may not be contiguous
                let first = match crate::pmm::alloc() {
                    Some(frame) => frame.address(),
                    None => return u64::MAX,
                };
                for _ in 1..count {
                    if crate::pmm::alloc().is_none() {
                        return u64::MAX;
                    }
                }
                scheduler::current_task_charge_mem(count);
                first as u64
            }
        }
        SYS_PHYS_FREE => {
            // arg0 = phys addr, arg1 = count
            if !scheduler::current_task_has_cap(crate::task::CAP_PHYS_ALLOC) {
                return u64::MAX;
            }
            let addr = arg0 as usize;
            let count = arg1 as usize;
            for i in 0..count {
                crate::pmm::free(crate::pmm::PhysFrame::from_address(addr + i * 4096));
            }
            0
        }
        SYS_GRANT_IOPORT => {
            // arg0 = tid to grant CAP_IOPORT
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            if !scheduler::current_task_has_cap(crate::task::CAP_IOPORT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            match scheduler::grant_cap(tid, crate::task::CAP_IOPORT) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GRANT_IRQ => {
            // arg0 = tid, arg1 = irq
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            if !scheduler::current_task_has_cap(crate::task::CAP_IRQ) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            match scheduler::grant_cap(tid, crate::task::CAP_IRQ) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GRANT_CAP => {
            // arg0 = tid, arg1 = capability bits to grant
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let caps = arg1 as u32;
            let caller_caps = scheduler::current_task_caps();
            if caps & !caller_caps != 0 {
                return u64::MAX;
            }
            match scheduler::grant_cap(tid, caps) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_FD_WRITE => {
            // arg0 = fd, arg1 = buf ptr, arg2 = len
            let fd = arg0 as usize;
            let ptr = arg1 as *const u8;
            let len = arg2 as usize;
            if len > 0 && !ptr.is_null() && !validate_user_ptr(arg1, arg2) {
                return u64::MAX;
            }
            match scheduler::current_fd(fd) {
                crate::task::FdKind::Ipc { target_tid, tag } => {
                    fd_write_ipc(target_tid, tag, ptr, len)
                }
                crate::task::FdKind::PipeWrite(handle) => {
                    crate::pipe::write(handle, ptr, len)
                }
                crate::task::FdKind::PipeRead(_) => u64::MAX,
                crate::task::FdKind::Empty => {
                    // fd not connected — fall back to kernel console for fd 1/2
                    if (fd == 1 || fd == 2) && len > 0 && !ptr.is_null() {
                        let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                        crate::console::puts(slice);
                        len as u64
                    } else {
                        u64::MAX
                    }
                }
            }
        }
        SYS_FD_READ => {
            // arg0 = fd, arg1 = buf ptr, arg2 = max len
            let fd = arg0 as usize;
            let ptr = arg1 as *mut u8;
            let max_len = arg2 as usize;
            if max_len > 0 && !ptr.is_null() && !validate_user_ptr(arg1, arg2) {
                return u64::MAX;
            }
            match scheduler::current_fd(fd) {
                crate::task::FdKind::Ipc { target_tid, tag } => {
                    fd_read_ipc(target_tid, tag, ptr, max_len)
                }
                crate::task::FdKind::PipeRead(handle) => {
                    crate::pipe::read(handle, ptr, max_len)
                }
                crate::task::FdKind::PipeWrite(_) => u64::MAX,
                crate::task::FdKind::Empty => u64::MAX,
            }
        }
        SYS_FD_SET => {
            // arg0 = target task tid, arg1 = fd, arg2 = service tid, arg3 = tag
            // Requires CAP_TASK_MGMT
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let fd = arg1 as usize;
            let service_tid = arg2 as usize;
            let tag = arg3;
            let entry = crate::task::FdKind::Ipc {
                target_tid: service_tid,
                tag,
            };
            match scheduler::set_fd(tid, fd, entry) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_PIPE_CREATE => {
            // Create a kernel pipe, returns handle index
            match crate::pipe::create() {
                Some(handle) => handle as u64,
                None => u64::MAX,
            }
        }
        SYS_PIPE_FD_SET => {
            // arg0 = target tid, arg1 = fd index, arg2 = pipe handle, arg3 = is_write (0=read, 1=write)
            // Requires CAP_TASK_MGMT
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let fd = arg1 as usize;
            let handle = arg2 as usize;
            let is_write = arg3 != 0;
            if crate::pipe::add_ref(handle, is_write).is_err() {
                return u64::MAX;
            }
            let kind = if is_write {
                crate::task::FdKind::PipeWrite(handle)
            } else {
                crate::task::FdKind::PipeRead(handle)
            };
            match scheduler::set_fd(tid, fd, kind) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_FD_DUP => {
            // arg0 = target tid, arg1 = target fd, arg2 = source fd (from current task)
            // Copies the caller's source fd to the target task's target fd.
            // Increments pipe refcount if the fd is a pipe endpoint.
            // Requires CAP_TASK_MGMT.
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let target_tid = arg0 as usize;
            let target_fd = arg1 as usize;
            let source_fd = arg2 as usize;
            let kind = scheduler::current_fd(source_fd);
            if kind.is_empty() {
                return u64::MAX;
            }
            // If it's a pipe, bump the refcount
            match kind {
                crate::task::FdKind::PipeRead(handle) => {
                    if crate::pipe::add_ref(handle, false).is_err() {
                        return u64::MAX;
                    }
                }
                crate::task::FdKind::PipeWrite(handle) => {
                    if crate::pipe::add_ref(handle, true).is_err() {
                        return u64::MAX;
                    }
                }
                _ => {}
            }
            match scheduler::set_fd(target_tid, target_fd, kind) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_FUTEX_WAIT => {
            // arg0 = addr, arg1 = expected value
            crate::futex::futex_wait(arg0, arg1 as u32)
        }
        SYS_FUTEX_WAKE => {
            // arg0 = addr, arg1 = max_wake
            crate::futex::futex_wake(arg0, arg1)
        }
        SYS_MMAP => {
            // arg0 = vaddr, arg1 = pages
            // Allocates physical frames and maps them into the caller's address space.
            // No capability required — every task can grow its own heap.
            let vaddr = arg0 as usize;
            let pages = arg1 as usize;
            if pages == 0 || pages > 256 {
                return u64::MAX;
            }
            // Must be page-aligned
            if vaddr & 0xFFF != 0 {
                return u64::MAX;
            }
            // Must be in user space and NOT in PML4[0] (kernel identity map / heap)
            let end = match (vaddr as u64).checked_add((pages as u64) * 4096) {
                Some(e) => e,
                None => return u64::MAX,
            };
            if end > USER_ADDR_LIMIT {
                return u64::MAX;
            }
            // Reject PML4[0] range (0 .. 0x80_0000_0000) — collides with kernel heap
            if vaddr < 0x80_0000_0000 {
                return u64::MAX;
            }
            // Check memory quota
            if !scheduler::current_task_check_mem(pages) {
                return u64::MAX;
            }
            let cr3 = paging::read_cr3();
            let flags = paging::PRESENT | paging::WRITABLE | paging::USER;
            for i in 0..pages {
                let phys = match crate::pmm::alloc() {
                    Some(frame) => frame.address(),
                    None => return u64::MAX, // TODO: unmap already-mapped pages on failure
                };
                // Zero the frame (identity-mapped)
                unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096) };
                let v = vaddr + i * 4096;
                if unsafe { paging::map_page(cr3, v, phys, flags) }.is_err() {
                    return u64::MAX;
                }
            }
            scheduler::current_task_charge_mem(pages);
            0
        }
        SYS_RECV_TIMEOUT => {
            // arg0 = from, arg1 = msg_ptr, arg2 = timeout_ticks
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let timeout = arg2;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if msg_ptr.is_null() || !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv_timeout(from, timeout) {
                Ok(msg) => { unsafe { *msg_ptr = msg }; 0 }
                Err(crate::ipc::IpcError::Timeout) => 1,
                Err(_) => u64::MAX,
            }
        }
        SYS_TICKS => {
            crate::pit::ticks()
        }
        SYS_WAIT => {
            // Block until a child task exits. Returns child TID or u64::MAX.
            scheduler::sys_wait()
        }
        SYS_SET_MEM_LIMIT => {
            // arg0 = tid, arg1 = limit in pages (0 = unlimited)
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let limit = arg1 as usize;
            match scheduler::set_mem_limit(tid, limit) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_SHMEM_CREATE => {
            // arg0 = pages
            crate::shmem::create(arg0 as usize)
        }
        SYS_SHMEM_MAP => {
            // arg0 = handle, arg1 = vaddr
            crate::shmem::map(arg0 as usize, arg1 as usize)
        }
        SYS_SHMEM_GRANT => {
            // arg0 = handle, arg1 = target tid
            crate::shmem::grant(arg0 as usize, arg1 as usize)
        }
        SYS_CAP_TRANSFER => {
            // arg0 = dest tid, arg1 = capability bits to transfer
            // Any task can transfer caps it holds — no CAP_TASK_MGMT required.
            let dest = arg0 as usize;
            let caps = arg1 as u32;
            let caller_caps = scheduler::current_task_caps();
            // Sender must hold all bits being transferred
            if caps & !caller_caps != 0 {
                return u64::MAX;
            }
            match scheduler::grant_cap(dest, caps) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_NOTIFY => {
            // arg0 = dest tid, arg1 = badge (bits to OR into notification word)
            let dest = arg0 as usize;
            let badge = arg1;
            match crate::ipc::sys_notify(dest, badge) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_SET_PAGER => {
            // arg0 = tid, arg1 = pager_tid
            if !scheduler::current_task_has_cap(crate::task::CAP_TASK_MGMT) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let pager_tid = arg1 as usize;
            match scheduler::set_pager(tid, pager_tid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
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
// After saving user context, we shuffle registers to match the C ABI for
// syscall_dispatch(nr, arg0, arg1, arg2, arg3, arg4), then sysret back.
//
// User convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4
// C ABI:          RDI=nr, RSI=arg0, RDX=arg1, RCX=arg2, R8=arg3,  R9=arg4
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

    // Set up 6-arg C ABI: syscall_dispatch(nr, arg0, arg1, arg2, arg3, arg4)
    // User regs: rax=nr, rdi=arg0, rsi=arg1, rdx=arg2, r10=arg3, r8=arg4
    // Shuffle order matters — move destinations that overlap sources last
    "    movq %r8, %r9",               // arg4 → r9 (6th C arg) — before r8 overwrite
    "    movq %r10, %r8",              // arg3 → r8 (5th C arg)
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

/// Update the kernel RSP in per-CPU data (used by scheduler on context switch).
pub fn update_kernel_rsp(rsp: u64) {
    unsafe {
        PER_CPU.kernel_rsp = rsp;
    }
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
