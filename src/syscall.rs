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
/// Exit with a status. Separate from SYS_EXIT because `syscall0` leaves RDI
/// undefined, so the existing zero-argument SYS_EXIT cannot grow an argument.
pub const SYS_EXIT_CODE: u64 = 4;
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
pub const SYS_FD_READ_NB: u64 = 56;

pub const SYS_FUTEX_WAIT: u64 = 60;
pub const SYS_FUTEX_WAKE: u64 = 61;

pub const SYS_GET_UID: u64 = 100;
pub const SYS_SET_UID: u64 = 101;
pub const SYS_SET_GID: u64 = 102;
pub const SYS_GET_TUID: u64 = 103;
pub const SYS_TASK_KILL: u64 = 104;
pub const SYS_TASK_INFO: u64 = 105;
pub const SYS_SIGNAL: u64 = 106;

pub const SYS_MMAP: u64 = 70;
pub const SYS_MUNMAP: u64 = 71;

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

pub const SYS_CAP_MINT: u64 = 110;
pub const SYS_CAP_GRANT: u64 = 111;
pub const SYS_CAP_REVOKE: u64 = 112;
pub const SYS_CAP_INSPECT: u64 = 113;
pub const SYS_CAP_DELETE: u64 = 114;
pub const SYS_SET_USER_CAPS: u64 = 115;
pub const SYS_GET_USER_CAPS: u64 = 116;
pub const SYS_SHMEM_UNMAP: u64 = 94;
pub const SYS_SHMEM_DESTROY: u64 = 95;

const SFMASK_VALUE: u64 = (1 << 9) | (1 << 10) | (1 << 18); // clear IF | DF | AC

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

unsafe extern "C" {
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

const USER_ADDR_LIMIT: u64 = paging::USER_ADDR_LIMIT;

/// Validate that a user pointer range is entirely in user space *and* actually
/// mapped in the calling task's address space.
///
/// The range check alone is not enough. The kernel dereferences user pointers
/// directly (it runs on the caller's CR3), so an in-range but unmapped address
/// faults inside the kernel — frequently with a spin lock held and interrupts
/// disabled, where the fault path cannot safely reschedule.
///
/// `write` additionally requires the pages be writable, so a syscall cannot be
/// tricked into writing through a read-only user mapping.
fn validate_user_range(addr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    if addr == 0 {
        return false;
    }
    match addr.checked_add(len) {
        Some(end) if end <= USER_ADDR_LIMIT => {}
        _ => return false,
    }
    unsafe { paging::user_range_accessible(paging::read_cr3(), addr, len, write) }
}

/// Read-only user buffer check.
fn validate_user_ptr(addr: u64, len: u64) -> bool {
    validate_user_range(addr, len, false)
}

/// Writable user buffer check.
fn validate_user_ptr_mut(addr: u64, len: u64) -> bool {
    validate_user_range(addr, len, true)
}

/// Authority to map a physical range into a page table.
///
/// Owning the frames is sufficient. `sys_phys_alloc` records the caller as
/// their owner, so handing back memory the allocator just gave you conveys no
/// authority you did not already hold. That is what almost every mapper is
/// doing: init, the shell and login allocate a frame, map it to stage an ELF
/// page or a stack, then map it into the child.
///
/// A `PhysRange` capability is therefore only needed for frames the allocator
/// never owned — device MMIO and the framebuffer — and for a page another
/// task allocated and passed over IPC for DMA. Separating the two is what lets
/// those grants be narrow, instead of the blanket 0-4 GiB that every mapper
/// previously had to hold.
fn may_map_phys(tid: usize, phys: usize, pages: usize) -> bool {
    crate::pmm::owns_range(phys, pages, tid) || crate::cap::task_has_phys_range(tid, phys, pages)
}

/// Report an IPC destination the caller lacks an Endpoint capability for.
///
/// Bounded: serial output busy-waits on the UART, so a task looping on a
/// forbidden destination could otherwise stall the machine by spamming this.
/// The first few reports are what matter when diagnosing a policy gap.
fn deny_ipc(caller: usize, dest: usize, what: &[u8]) -> u64 {
    const MAX_REPORTS: u32 = 32;
    static REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if REPORTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return u64::MAX;
    }

    crate::serial::puts(b"[cap] tid ");
    crate::serial::put_usize(caller);
    crate::serial::puts(b" denied ");
    crate::serial::puts(what);
    crate::serial::puts(b" -> tid ");
    crate::serial::put_usize(dest);
    crate::serial::puts(b"\n");
    u64::MAX
}

/// Unmap `pages` pages starting at `vaddr`, returning owned frames to the PMM.
/// Used to roll back a partially completed mapping loop.
fn unmap_range_owned(cr3: usize, vaddr: usize, pages: usize) {
    for i in 0..pages {
        unsafe { paging::unmap_page_owned(cr3, vaddr + i * 4096) };
    }
}

/// Maximum bytes per IPC write message (5 data words × 8 bytes).
const FD_WRITE_MAX_CHUNK: usize = 40;

/// Send a write via IPC to a service, chunking data into 40-byte messages.
/// Returns bytes written.
fn fd_write_ipc(target_tid: usize, tag: u64, ptr: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let mut offset = 0usize;
    while offset < len {
        let chunk = (len - offset).min(FD_WRITE_MAX_CHUNK);

        // Snapshot this chunk while the SMAP window is open, then close it
        // before doing anything that can block.
        let mut staged = [0u8; FD_WRITE_MAX_CHUNK];
        {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe {
                core::ptr::copy_nonoverlapping(ptr.add(offset), staged.as_mut_ptr(), chunk);
            }
        }

        // Pack bytes into data[1..6]
        let mut data = [0u64; 6];
        data[0] = chunk as u64;
        for i in 0..5 {
            let base = i * 8;
            let mut w = [0u8; 8];
            for j in 0..8 {
                if base + j < chunk {
                    w[j] = staged[base + j];
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
            // Unpack bytes from reply.data[1..6] into a staging buffer, then
            // copy out under a short SMAP window.
            let mut staged = [0u8; FD_WRITE_MAX_CHUNK];
            for i in 0..5 {
                let base = i * 8;
                let bytes = reply.data[i + 1].to_le_bytes();
                for j in 0..8 {
                    if base + j < actual {
                        staged[base + j] = bytes[j];
                    }
                }
            }
            {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    core::ptr::copy_nonoverlapping(staged.as_ptr(), ptr, actual);
                }
            }
            actual as u64
        }
        Err(_) => u64::MAX,
    }
}

/// Called from assembly with 6 args mapped from user registers.
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(
    nr: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> u64 {
    match nr {
        SYS_EXIT => {
            scheduler::exit()
        }
        SYS_EXIT_CODE => {
            scheduler::exit_with(arg0 as i32)
        }
        SYS_YIELD => {
            scheduler::yield_now();
            0
        }
        SYS_WRITE => {
            let ptr = arg0 as *const u8;
            let len = arg1 as usize;
            if len == 0 {
                return 0;
            }
            if !validate_user_ptr(arg0, arg1) {
                return u64::MAX;
            }
            let _ua = crate::cpu::UserAccess::begin();
            let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
            console::puts(slice);
            len as u64
        }
        SYS_CONSOLE_POS => {
            let (row, col) = console::cursor_pos_and_disable();
            ((row as u64) << 32) | (col as u64)
        }
        SYS_GETPID => scheduler::current_tid() as u64,
        SYS_SEND => {
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"send");
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_send(dest, &msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_RECV => {
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr_mut(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv(from) {
                Ok(msg) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *msg_ptr = msg };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL => {
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"call");
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size)
                || !validate_user_ptr_mut(arg2, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_call(dest, &msg) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *reply_ptr = reply };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_REPLY => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_reply(dest, &msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_IRQ_REGISTER => {
            let irq = arg0 as u8;
            if !crate::cap::task_has_irq(scheduler::current_tid(), irq) {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            crate::irq_dispatch::register_irq_handler(irq, tid);
            unsafe { crate::pic::enable_irq(irq) };
            0
        }
        SYS_IRQ_ACK => {
            // arg0 = IRQ number
            let irq = arg0 as u8;
            // Acking an IRQ you do not own lets any task interfere with the
            // PIC's in-service state and stall another driver's interrupts.
            if !crate::cap::task_has_irq(scheduler::current_tid(), irq) {
                return u64::MAX;
            }
            unsafe { crate::pic::send_eoi(irq) };
            0
        }
        SYS_IOPORT => {
            // arg0=port, arg1=op (0=read8,1=write8,2=read16,3=write16,4=read32,5=write32), arg2=value (for writes)
            let port = arg0 as u16;
            if !crate::cap::task_has_ioport(scheduler::current_tid(), port) {
                return u64::MAX;
            }
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
            let port = arg0 as u16;
            if !crate::cap::task_has_ioport(scheduler::current_tid(), port) {
                return u64::MAX;
            }
            let buf = arg1;
            let count = arg2 as usize;
            let op = arg3;
            if count == 0 {
                return 0;
            }
            // insw writes into the buffer, outsw only reads it.
            let bytes = (count as u64).saturating_mul(2);
            let ok = match op {
                0 => validate_user_ptr_mut(buf, bytes),
                1 => validate_user_ptr(buf, bytes),
                _ => return u64::MAX,
            };
            if !ok {
                return u64::MAX;
            }
            let _ua = crate::cpu::UserAccess::begin();
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
            let phys = arg0 as usize;
            let virt = arg1 as usize;
            let pages = arg2 as usize;
            if !paging::user_range_ok(virt, pages) {
                return u64::MAX;
            }
            if phys & 0xFFF != 0 || phys.checked_add(pages * 4096).is_none() {
                return u64::MAX;
            }
            if !may_map_phys(scheduler::current_tid(), phys, pages) {
                return u64::MAX;
            }
            let pml4 = paging::read_cr3();
            // No OWNED bit: these frames belong to a device, not to this
            // address space. Freeing them on unmap/teardown would push MMIO
            // addresses into the frame allocator.
            let flags = paging::PRESENT | paging::WRITABLE | paging::USER;
            for i in 0..pages {
                let p = phys + i * 4096;
                let v = virt + i * 4096;
                if unsafe { paging::map_page(pml4, v, p, flags) }.is_err() {
                    // Roll back the pages mapped so far.
                    for j in 0..i {
                        let _ = unsafe { paging::unmap_page(pml4, virt + j * 4096) };
                    }
                    return u64::MAX;
                }
            }
            0
        }
        SYS_TASK_CREATE => {
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            match scheduler::create_empty_task() {
                Some(tid) => tid as u64,
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_CREATE => {
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            match crate::userspace::create_address_space() {
                Some(cr3) => cr3 as u64,
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_MAP => {
            // arg0=cr3, arg1=virt, arg2=phys, arg3=pages, arg4=flags
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let cr3 = arg0 as usize;
            let virt = arg1 as usize;
            let phys = arg2 as usize;
            let pages = arg3 as usize;
            let flags = arg4;
            if !paging::user_range_ok(virt, pages) {
                return u64::MAX;
            }
            if phys & 0xFFF != 0 || phys.checked_add(pages * 4096).is_none() {
                return u64::MAX;
            }
            // The caller supplies both the target address space and the
            // backing frames, so it must actually hold authority over that
            // physical range — otherwise CAP_TASK_MGMT silently implied full
            // physical read/write.
            if !may_map_phys(scheduler::current_tid(), phys, pages) {
                return u64::MAX;
            }
            // cr3 must be an address space this task created, not an arbitrary
            // physical address reinterpreted as a PML4.
            if !crate::userspace::is_owned_address_space(scheduler::current_tid(), cr3) {
                return u64::MAX;
            }
            // No OWNED bit: the frames came from the caller (via sys_phys_alloc),
            // which stays responsible for them.
            let pte_flags = paging::PRESENT | paging::USER
                | if flags & 1 != 0 { paging::WRITABLE } else { 0 };
            for i in 0..pages {
                let v = virt + i * 4096;
                let p = phys + i * 4096;
                if unsafe { paging::map_page(cr3, v, p, pte_flags) }.is_err() {
                    for j in 0..i {
                        let _ = unsafe { paging::unmap_page(cr3, virt + j * 4096) };
                    }
                    return u64::MAX;
                }
            }
            0
        }
        SYS_TASK_START => {
            // arg0=tid, arg1=rip, arg2=rsp, arg3=cr3
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let rip = arg1;
            // Ensure RSP ≡ 8 mod 16 for x86_64 ABI (as if call pushed return addr).
            // Align DOWN to 16, then subtract 8 — never go above the caller's value.
            // `checked_sub` because arg2 < 8 used to wrap to a kernel address.
            let rsp = match (arg2 & !0xF).checked_sub(8) {
                Some(r) => r,
                None => return u64::MAX,
            };
            // Entry point and stack must both live in user space.
            if rip >= USER_ADDR_LIMIT || rsp >= USER_ADDR_LIMIT {
                return u64::MAX;
            }
            let cr3 = arg3 as usize;
            if !crate::userspace::is_owned_address_space(scheduler::current_tid(), cr3) {
                return u64::MAX;
            }
            match scheduler::start_task(tid, rip, rsp, cr3) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_PHYS_ALLOC => {
            // arg0 = number of contiguous pages to allocate
            if !crate::cap::task_has_phys_alloc(scheduler::current_tid()) {
                return u64::MAX;
            }
            let count = arg0 as usize;
            if count == 0 || count > 1024 {
                return u64::MAX;
            }
            // Check memory quota
            if !scheduler::current_task_check_mem(count) {
                return u64::MAX;
            }
            // For simplicity, allocate pages one at a time and return the first
            // (Only single-page alloc is reliable with bitmap allocator)
            let frame = if count == 1 {
                crate::pmm::alloc()
            } else {
                // Allocate count physically contiguous pages
                crate::pmm::alloc_contiguous(count)
            };
            let base = match frame {
                Some(f) => f.address(),
                None => return u64::MAX,
            };
            // Record who owns these frames so sys_phys_free can verify the
            // caller actually holds them, and so reaping can reclaim them.
            crate::pmm::set_owner(base, count, scheduler::current_tid());
            scheduler::current_task_charge_mem(count);
            base as u64
        }
        SYS_PHYS_FREE => {
            // arg0 = phys addr, arg1 = count
            if !crate::cap::task_has_phys_alloc(scheduler::current_tid()) {
                return u64::MAX;
            }
            let addr = arg0 as usize;
            let count = arg1 as usize;
            if count == 0 || addr & 0xFFF != 0 {
                return u64::MAX;
            }
            if count.checked_mul(4096).and_then(|l| addr.checked_add(l)).is_none() {
                return u64::MAX;
            }
            // Every frame in the range must belong to this task. Anything
            // else — a kernel frame, another task's frames, or a partially
            // owned range — is rejected rather than pushed into the allocator.
            if !crate::pmm::owns_range(addr, count, scheduler::current_tid()) {
                return u64::MAX;
            }
            crate::pmm::clear_owner(addr, count);
            for i in 0..count {
                crate::pmm::free(crate::pmm::PhysFrame::from_address(addr + i * 4096));
            }
            // Refund the quota charged by sys_phys_alloc.
            scheduler::current_task_uncharge_mem(count);
            0
        }
        SYS_GRANT_IOPORT => {
            // arg0 = tid to grant CAP_IOPORT
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) {
                return u64::MAX;
            }
            // Must hold both ends of the range being delegated, not just port 0.
            if !crate::cap::task_has_ioport(caller, 0)
                || !crate::cap::task_has_ioport(caller, 0xFFFF)
            {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            if crate::cap::grant_slot(
                tid,
                crate::cap::CapType::IoPort,
                0,
                0xFFFF,
                caller,
            ) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_GRANT_IRQ => {
            // arg0 = tid, arg1 = irq
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let irq = arg1 as u8;
            // Delegate exactly the IRQ named in arg1. This used to hand over
            // the blanket CAP_IRQ bit, which expands to the 0xFF wildcard --
            // so delegating IRQ 1 delegated every IRQ on the machine.
            if !crate::cap::task_has_irq(caller, irq) {
                return u64::MAX;
            }
            if crate::cap::grant_slot(
                tid,
                crate::cap::CapType::Irq,
                irq as u64,
                0,
                caller,
            ) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_GRANT_CAP => {
            // arg0 = tid, arg1 = capability bits to grant
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let caps = arg1 as u32;
            // The granter must hold every bit it delegates. UID 0 used to skip
            // this entirely, which made the check meaningless for the only
            // tasks that call it.
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
            if len > 0 && !validate_user_ptr(arg1, arg2) {
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
                    if (fd == 1 || fd == 2) && len > 0 {
                        let _ua = crate::cpu::UserAccess::begin();
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
            if max_len > 0 && !validate_user_ptr_mut(arg1, arg2) {
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
        SYS_FD_READ_NB => {
            // Non-blocking fd read. Only supports pipe fds.
            let fd = arg0 as usize;
            let ptr = arg1 as *mut u8;
            let max_len = arg2 as usize;
            if max_len > 0 && !validate_user_ptr_mut(arg1, arg2) {
                return u64::MAX;
            }
            match scheduler::current_fd(fd) {
                crate::task::FdKind::PipeRead(handle) => {
                    crate::pipe::read_nonblock(handle, ptr, max_len)
                }
                _ => u64::MAX,
            }
        }
        SYS_FD_SET => {
            // arg0 = target task tid, arg1 = fd, arg2 = service tid, arg3 = tag
            // Requires CAP_TASK_MGMT
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
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
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
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
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
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
            // Page-aligned, no overflow, and clear of PML4[0] (whose page
            // directories are shared with the kernel).
            if !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            // Charge up front so a partial failure can't leave pages mapped
            // but unaccounted; the rollback path refunds.
            if !scheduler::current_task_check_mem(pages) {
                return u64::MAX;
            }
            scheduler::current_task_charge_mem(pages);

            let cr3 = paging::read_cr3();
            // OWNED: anonymous memory this address space must free on teardown.
            let flags =
                paging::PRESENT | paging::WRITABLE | paging::USER | paging::OWNED;
            for i in 0..pages {
                let v = vaddr + i * 4096;
                let phys = match crate::pmm::alloc() {
                    Some(frame) => frame.address(),
                    None => {
                        unmap_range_owned(cr3, vaddr, i);
                        scheduler::current_task_uncharge_mem(pages);
                        return u64::MAX;
                    }
                };
                // Zero the frame (identity-mapped)
                unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096) };
                if unsafe { paging::map_page(cr3, v, phys, flags) }.is_err() {
                    crate::pmm::free(crate::pmm::PhysFrame::from_address(phys));
                    unmap_range_owned(cr3, vaddr, i);
                    scheduler::current_task_uncharge_mem(pages);
                    return u64::MAX;
                }
            }
            0
        }
        SYS_MUNMAP => {
            // arg0 = vaddr, arg1 = pages
            // Unmaps pages and frees their physical frames.
            let vaddr = arg0 as usize;
            let pages = arg1 as usize;
            if pages == 0 || pages > 256 {
                return u64::MAX;
            }
            if !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            let cr3 = paging::read_cr3();
            let mut freed = 0usize;
            for i in 0..pages {
                let v = vaddr + i * 4096;
                // Only frames this address space owns are returned to the PMM.
                // Shared-memory pages and device MMIO are unmapped but never
                // freed — otherwise munmap double-frees a shmem region or
                // hands the allocator a device physical address.
                if unsafe { paging::unmap_page_owned(cr3, v) } {
                    freed += 1;
                }
            }
            if freed > 0 {
                scheduler::current_task_uncharge_mem(freed);
            }
            freed as u64
        }
        SYS_RECV_TIMEOUT => {
            // arg0 = from, arg1 = msg_ptr, arg2 = timeout_ticks
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let timeout = arg2;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr_mut(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv_timeout(from, timeout) {
                Ok(msg) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *msg_ptr = msg };
                    0
                }
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
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
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
        SYS_SHMEM_UNMAP => {
            // arg0 = handle, arg1 = vaddr
            crate::shmem::unmap(arg0 as usize, arg1 as usize)
        }
        SYS_SHMEM_DESTROY => {
            // arg0 = handle
            crate::shmem::destroy(arg0 as usize)
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
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"notify");
            }
            match crate::ipc::sys_notify(dest, badge) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_SET_PAGER => {
            // arg0 = tid, arg1 = pager_tid
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let pager_tid = arg1 as usize;
            match scheduler::set_pager(tid, pager_tid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GET_UID => {
            let uid = scheduler::current_task_uid();
            let gid = scheduler::current_task_gid();
            ((uid as u64) << 32) | (gid as u64)
        }
        SYS_SET_UID => {
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let uid = arg1 as u32;
            match scheduler::set_task_uid(tid, uid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_SET_GID => {
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let gid = arg1 as u32;
            match scheduler::set_task_gid(tid, gid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GET_TUID => {
            let tid = arg0 as usize;
            match scheduler::task_uid_gid(tid) {
                Ok((uid, gid)) => ((uid as u64) << 32) | (gid as u64),
                Err(()) => u64::MAX,
            }
        }
        SYS_TASK_KILL => {
            // arg0 = tid to kill. Requires TaskMgmt cap for target or same UID.
            let tid = arg0 as usize;
            let caller = scheduler::current_tid();
            let caller_uid = scheduler::current_task_uid();
            let has_cap = crate::cap::task_has_task_mgmt(caller, tid);
            let same_uid = scheduler::task_uid_gid(tid)
                .map(|(uid, _)| uid == caller_uid)
                .unwrap_or(false);
            if !has_cap && !same_uid {
                return u64::MAX;
            }
            match scheduler::kill_task(tid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_TASK_INFO => {
            // arg0 = tid. Returns packed info or u64::MAX if no task.
            // bits [3:0] = state (0=Ready,1=Running,2=Blocked,3=Dead)
            // bits [31:4] = parent_tid
            // bits [63:32] = uid
            let tid = arg0 as usize;
            match scheduler::task_info(tid) {
                Some((state, uid, _gid, parent)) => {
                    let state_bits = match state {
                        crate::task::TaskState::Ready => 0u64,
                        crate::task::TaskState::Running => 1,
                        crate::task::TaskState::Blocked => 2,
                        crate::task::TaskState::Dead => 3,
                    };
                    state_bits | ((parent as u64) << 4) | ((uid as u64) << 32)
                }
                None => u64::MAX,
            }
        }
        SYS_SIGNAL => {
            // arg0 = tid, arg1 = signal bits. Same permissions as sys_task_kill.
            let tid = arg0 as usize;
            let sig = arg1;
            let caller = scheduler::current_tid();
            let caller_uid = scheduler::current_task_uid();
            let has_cap = crate::cap::task_has_task_mgmt(caller, tid);
            let same_uid = scheduler::task_uid_gid(tid)
                .map(|(uid, _)| uid == caller_uid)
                .unwrap_or(false);
            if !has_cap && !same_uid {
                return u64::MAX;
            }
            match crate::ipc::sys_signal(tid, sig) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_CAP_MINT => {
            // arg0 = slot, arg1 = type, arg2 = param0, arg3 = param1
            // Create a root cap in caller's slot (requires existing authority)
            let slot = arg0 as usize;
            let cap_type_raw = arg1 as u8;
            let param0 = arg2;
            let param1 = arg3;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let cap_type = match cap_type_raw {
                1 => crate::cap::CapType::IoPort,
                2 => crate::cap::CapType::PhysRange,
                3 => crate::cap::CapType::Irq,
                4 => crate::cap::CapType::TaskMgmt,
                5 => crate::cap::CapType::PhysAlloc,
                6 => crate::cap::CapType::SetUid,
                7 => crate::cap::CapType::Endpoint,
                _ => return u64::MAX,
            };
            let tid = scheduler::current_tid();
            unsafe {
                let task = match scheduler::get_task_mut(tid) {
                    Some(t) => t,
                    None => return u64::MAX,
                };
                // The caller must already hold a capability that covers what
                // it is minting. This used to be skipped for UID 0.
                if !crate::cap::can_mint(&task.cspace, cap_type, param0, param1) {
                    return u64::MAX;
                }
                // Target slot must be empty
                if task.cspace[slot].cap_type as u8 != crate::cap::CapType::Empty as u8 {
                    return u64::MAX;
                }
                // Must adopt the slot's *current* generation. Hardcoding 0
                // meant that after a single sys_cap_revoke on this slot every
                // subsequently minted cap was born already-invalid.
                task.cspace[slot] = crate::cap::CapSlot {
                    cap_type,
                    generation: crate::cap::current_generation(tid, slot),
                    root_slot: slot as u8,
                    root_tid: tid as u8,
                    param0,
                    param1,
                };
            }
            0
        }
        SYS_CAP_GRANT => {
            // arg0 = dest_tid, arg1 = src_slot, arg2 = dest_slot
            // Delegate cap to another task (with attenuation tracking)
            let dest_tid = arg0 as usize;
            let src_slot = arg1 as usize;
            let dest_slot = arg2 as usize;
            if src_slot >= crate::cap::MAX_CAPS || dest_slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let caller_tid = scheduler::current_tid();
            unsafe {
                let src_cap = match scheduler::get_task_mut(caller_tid) {
                    Some(t) => t.cspace[src_slot],
                    None => return u64::MAX,
                };
                if src_cap.cap_type as u8 == crate::cap::CapType::Empty as u8 {
                    return u64::MAX;
                }
                // A revoked cap must not be re-delegatable.
                if !crate::cap::slot_is_valid(&src_cap) {
                    return u64::MAX;
                }
                let dest_task = match scheduler::get_task_mut(dest_tid) {
                    Some(t) => t,
                    None => return u64::MAX,
                };
                if dest_task.cspace[dest_slot].cap_type as u8 != crate::cap::CapType::Empty as u8 {
                    return u64::MAX;
                }
                // Derive: copy cap but track provenance for revocation
                let kernel_minted = src_cap.root_tid == crate::cap::KERNEL_ROOT_TID;
                let root_tid = if kernel_minted {
                    // Kernel-minted cap: the granter becomes the root
                    caller_tid as u8
                } else {
                    src_cap.root_tid
                };
                let root_slot = if kernel_minted {
                    src_slot as u8
                } else {
                    src_cap.root_slot
                };
                let generation =
                    crate::cap::current_generation(root_tid as usize, root_slot as usize);
                dest_task.cspace[dest_slot] = crate::cap::CapSlot {
                    cap_type: src_cap.cap_type,
                    generation,
                    root_slot,
                    root_tid,
                    param0: src_cap.param0,
                    param1: src_cap.param1,
                };
            }
            0
        }
        SYS_CAP_REVOKE => {
            // arg0 = slot
            // Bump generation, invalidate all derived caps
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            crate::cap::revoke(tid, slot);
            0
        }
        SYS_CAP_INSPECT => {
            // arg0 = slot
            // Return packed info: type in bits [7:0], param0 in upper bits
            // For full inspection, use two calls or a buffer.
            // Simple: return type | (param0 << 8) truncated to u64
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            unsafe {
                let task = match scheduler::get_task_mut(tid) {
                    Some(t) => t,
                    None => return u64::MAX,
                };
                let cap = &task.cspace[slot];
                let cap_type = cap.cap_type as u64;
                // Pack: [7:0]=type, [23:8]=param0 low 16, [39:24]=param1 low 16
                cap_type | ((cap.param0 & 0xFFFF) << 8) | ((cap.param1 & 0xFFFF) << 24)
            }
        }
        SYS_CAP_DELETE => {
            // arg0 = slot
            // Delete cap from own CSpace
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            unsafe {
                let task = match scheduler::get_task_mut(tid) {
                    Some(t) => t,
                    None => return u64::MAX,
                };
                task.cspace[slot] = crate::cap::CapSlot::empty();
            }
            0
        }
        SYS_SET_USER_CAPS => {
            // arg0 = uid, arg1 = capability bitmask
            // Requires CAP_SET_UID (root gets it via UID bypass)
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let uid = arg0 as u32;
            let caps = arg1 as u32;
            crate::cap::set_user_caps(uid, caps);
            0
        }
        SYS_GET_USER_CAPS => {
            // arg0 = uid
            let uid = arg0 as u32;
            crate::cap::user_caps(uid) as u64
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

    // Return value is in %rax.
    // Scrub the caller-saved scratch registers the ABI lets us clobber: they
    // still hold kernel values here and sysret would hand them to ring 3.
    // (rcx/r11 are overwritten below with the user's saved RIP/RFLAGS,
    // rsi/rdi are restored from the user's own saved args.)
    "    xorl %edx, %edx",
    "    xorl %r8d, %r8d",
    "    xorl %r9d, %r9d",
    "    xorl %r10d, %r10d",

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
pub unsafe fn setup_percpu(kernel_stack_top: u64) { unsafe {
    PER_CPU.kernel_rsp = kernel_stack_top;
    let addr = &raw const PER_CPU as u64;
    write_msr(0xC000_0101, addr); // IA32_KERNEL_GS_BASE (for swapgs)
}}

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
pub unsafe fn enter_usermode(rip: u64, rsp: u64) -> ! { unsafe {
    core::arch::asm!(
        "pushq {user_ss}",             // SS
        "pushq {user_rsp}",            // RSP
        // RFLAGS built from scratch: bit 1 (reserved, always set) + IF.
        // Deriving it from the kernel's current RFLAGS handed user mode
        // whatever DF/AC/IOPL state the kernel happened to be in.
        "pushq $0x202",                // RFLAGS
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
}}
