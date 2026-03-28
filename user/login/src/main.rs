#![no_std]
#![no_main]

use quark_rt::ipc::Message;
use quark_rt::stdio::read_line;
use quark_rt::{passwd, print, println, syscall, vfs};

const PAGE_SIZE: usize = 4096;
const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;

// Login temp address ranges (non-overlapping with init 0x82-0x88, shell 0x90-0x93)
const FILE_BUF_BASE: usize = 0x94_0000_0000;
const ELF_TEMP: usize = 0x95_0000_0000;
const STACK_TEMP: usize = 0x96_0000_0000;
const ARGS_TEMP_PAGE: usize = 0x97_0000_0000;
const ARGS_PAGE_ADDR: usize = 0x80_8000_0000;
const PASSWD_BUF: usize = 0x98_0000_0000;

// ---------------------------------------------------------------------------
// ELF64 structures
// ---------------------------------------------------------------------------

#[repr(C, packed)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
}

#[repr(C, packed)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

const PT_LOAD: u32 = 1;
const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];

// ---------------------------------------------------------------------------
// Service discovery
// ---------------------------------------------------------------------------

fn lookup_service(name: &[u8]) -> Option<usize> {
    let mut buf = [0u8; 24];
    let len = name.len().min(24);
    buf[..len].copy_from_slice(&name[..len]);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        Some(reply.tag as usize)
    } else {
        None
    }
}

fn lookup_service_with_retry(name: &[u8], max_attempts: usize) -> Option<usize> {
    for _ in 0..max_attempts {
        if let Some(tid) = lookup_service(name) {
            return Some(tid);
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// ELF loader
// ---------------------------------------------------------------------------

struct SpawnInfo {
    tid: usize,
    entry: u64,
    stack_top: u64,
    cr3: usize,
}

impl SpawnInfo {
    fn start(&self) -> Result<(), ()> {
        syscall::sys_task_start(self.tid, self.entry, self.stack_top, self.cr3)
    }
}

fn load_elf(elf_data: &[u8]) -> Result<SpawnInfo, ()> {
    if elf_data.len() < 64 || elf_data[0..4] != ELF_MAGIC {
        return Err(());
    }

    let hdr = unsafe { &*(elf_data.as_ptr() as *const Elf64Header) };
    let entry = hdr.e_entry;
    let phoff = hdr.e_phoff as usize;
    let phentsize = hdr.e_phentsize as usize;
    let phnum = hdr.e_phnum as usize;

    let cr3 = syscall::sys_addrspace_create()?;
    let tid = syscall::sys_task_create()?;

    for i in 0..phnum {
        let offset = phoff + i * phentsize;
        if offset + phentsize > elf_data.len() {
            break;
        }
        let phdr = unsafe { &*(elf_data.as_ptr().add(offset) as *const Elf64Phdr) };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;
        let file_offset = phdr.p_offset as usize;
        let writable = phdr.p_flags & 2 != 0;

        let vaddr_page_start = vaddr & !0xFFF;
        let vaddr_end = vaddr + memsz;
        let pages = (vaddr_end - vaddr_page_start + PAGE_SIZE - 1) / PAGE_SIZE;

        let file_start = vaddr;
        let file_end = vaddr + filesz;

        for p in 0..pages {
            let page_vaddr = vaddr_page_start + p * PAGE_SIZE;

            let frame = syscall::sys_phys_alloc(1)?;

            let temp_page = ELF_TEMP + p * PAGE_SIZE;
            syscall::sys_map_phys(frame, temp_page, 1)?;

            unsafe {
                core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE);
            }

            let page_end = page_vaddr + PAGE_SIZE;
            if file_start < page_end && file_end > page_vaddr {
                let copy_vstart = file_start.max(page_vaddr);
                let copy_vend = file_end.min(page_end);
                let copy_len = copy_vend - copy_vstart;
                let dst_offset = copy_vstart - page_vaddr;
                let src_offset = file_offset + (copy_vstart - vaddr);

                if src_offset + copy_len <= elf_data.len() {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            elf_data.as_ptr().add(src_offset),
                            (temp_page + dst_offset) as *mut u8,
                            copy_len,
                        );
                    }
                }
            }

            let flags: u64 = if writable { 1 } else { 0 };
            syscall::sys_addrspace_map(cr3, page_vaddr, frame, 1, flags)?;
        }
    }

    // Set up user stack (4 pages)
    let stack_top: usize = 0x7FFF_FFFF_F000;
    let stack_pages: usize = 4;
    let stack_bottom = stack_top - stack_pages * PAGE_SIZE;
    for p in 0..stack_pages {
        let frame = syscall::sys_phys_alloc(1)?;
        let temp_page = STACK_TEMP + p * PAGE_SIZE;
        syscall::sys_map_phys(frame, temp_page, 1)?;
        unsafe {
            core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE);
        }
        syscall::sys_addrspace_map(cr3, stack_bottom + p * PAGE_SIZE, frame, 1, 1)?;
    }

    Ok(SpawnInfo { tid, entry, stack_top: stack_top as u64, cr3 })
}

// ---------------------------------------------------------------------------
// Program arguments
// ---------------------------------------------------------------------------

fn set_args(info: &SpawnInfo, args: &[&[u8]]) -> Result<(), ()> {
    let frame = syscall::sys_phys_alloc(1)?;
    syscall::sys_map_phys(frame, ARGS_TEMP_PAGE, 1)?;

    let base = ARGS_TEMP_PAGE as *mut u8;
    unsafe {
        core::ptr::write_bytes(base, 0, PAGE_SIZE);
        *(base as *mut u64) = args.len() as u64;

        let mut offset = 8usize;
        for arg in args {
            if offset + 8 + arg.len() > PAGE_SIZE {
                break;
            }
            *(base.add(offset) as *mut u64) = arg.len() as u64;
            offset += 8;
            core::ptr::copy_nonoverlapping(arg.as_ptr(), base.add(offset), arg.len());
            offset += arg.len();
        }
    }

    syscall::sys_addrspace_map(info.cr3, ARGS_PAGE_ADDR, frame, 1, 0)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Load and read a file from VFS into FILE_BUF_BASE
// ---------------------------------------------------------------------------

fn load_file(vfs_tid: usize, path: &[u8]) -> Result<&'static [u8], ()> {
    let (handle, file_size, _) = vfs::open(vfs_tid, path).map_err(|_| ())?;
    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    for p in 0..pages_needed {
        let frame = syscall::sys_phys_alloc(1)?;
        syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1)?;
        let offset = (p * PAGE_SIZE) as u32;
        let to_read = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        vfs::read(vfs_tid, handle, frame, offset, to_read).map_err(|_| ())?;
    }
    let _ = vfs::close(vfs_tid, handle);

    Ok(unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let vfs_tid = match lookup_service_with_retry(b"vfs", 50) {
        Some(tid) => tid,
        None => {
            println!("login: vfs not found");
            syscall::sys_exit();
        }
    };

    let mut line_buf = [0u8; 64];

    loop {
        print!("login: ");
        let n = read_line(&mut line_buf);

        // Trim whitespace
        let mut end = n;
        while end > 0 && (line_buf[end - 1] == b'\n' || line_buf[end - 1] == b'\r' || line_buf[end - 1] == b' ') {
            end -= 1;
        }
        let mut start = 0;
        while start < end && line_buf[start] == b' ' {
            start += 1;
        }

        // Ctrl+C or empty input — re-prompt
        if n == 0 || start >= end {
            continue;
        }

        let username = &line_buf[start..end];

        // Read /etc/PASSWD
        let passwd_data = match load_passwd_file(vfs_tid) {
            Some(data) => data,
            None => {
                println!("login: cannot read /etc/PASSWD");
                continue;
            }
        };

        // Look up user
        let entry = match passwd::lookup_user(passwd_data, username) {
            Some(e) => e,
            None => {
                if let Ok(s) = core::str::from_utf8(username) {
                    println!("Unknown user: {}", s);
                }
                continue;
            }
        };

        // Set our own UID/GID
        let my_tid = syscall::sys_getpid() as usize;
        let _ = syscall::sys_set_uid(my_tid, entry.uid);
        let _ = syscall::sys_set_gid(my_tid, entry.gid);

        // Load the user's shell — try as-is, then lowercase without extension
        let shell_path = entry.shell();
        let elf_data = match load_file(vfs_tid, shell_path) {
            Ok(data) => data,
            Err(()) => {
                // Try lowercase path without .ELF extension (ext2 format)
                let mut alt = [0u8; 64];
                let mut alt_len = 0;
                for &b in shell_path.iter() {
                    if alt_len < 64 {
                        alt[alt_len] = if b >= b'A' && b <= b'Z' { b + 32 } else { b };
                        alt_len += 1;
                    }
                }
                // Strip .elf suffix if present
                if alt_len >= 4 && &alt[alt_len - 4..alt_len] == b".elf" {
                    alt_len -= 4;
                }
                match load_file(vfs_tid, &alt[..alt_len]) {
                    Ok(data) => data,
                    Err(()) => {
                        if let Ok(s) = core::str::from_utf8(shell_path) {
                            println!("login: cannot load shell: {}", s);
                        }
                        continue;
                    }
                }
            }
        };

        let info = match load_elf(elf_data) {
            Ok(i) => i,
            Err(()) => {
                println!("login: failed to load shell ELF");
                continue;
            }
        };

        let tid = info.tid;

        // Set child UID/GID
        let _ = syscall::sys_set_uid(tid, entry.uid);
        let _ = syscall::sys_set_gid(tid, entry.gid);

        // Grant shell capabilities (task mgmt + phys for spawning + ioport for shutdown)
        let _ = syscall::sys_grant_cap(
            tid,
            syscall::CAP_TASK_MGMT | syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS | syscall::CAP_IOPORT,
        );
        // Fine-grained caps for shell: TaskMgmt, PhysAlloc, PhysRange, IOPORT (ACPI shutdown)
        const SCRATCH: usize = 14;
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 0);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_PHYS_ALLOC, 64, 0);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 1);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_PHYS_RANGE, 0, 0x1_0000_0000);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 2);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_IOPORT, 0x604, 0x604);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 3);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_IOPORT, 0xB004, 0xB004);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 4);
        let _ = syscall::sys_cap_delete(SCRATCH);

        // Wire file descriptors
        let _ = syscall::sys_fd_dup(tid, 0, 0); // stdin
        let _ = syscall::sys_fd_dup(tid, 1, 1); // stdout
        let _ = syscall::sys_fd_dup(tid, 2, 2); // stderr

        // Pass shell name and home directory as argv
        let home = entry.home();
        let _ = set_args(&info, &[shell_path, home]);

        // Start shell and wait for it to exit
        if info.start().is_err() {
            println!("login: failed to start shell");
            continue;
        }

        let _ = syscall::sys_wait();

        // Shell exited — reset UID back to root for next login prompt
        let _ = syscall::sys_set_uid(my_tid, 0);
        let _ = syscall::sys_set_gid(my_tid, 0);

        println!(""); // blank line before next login prompt
    }
}

fn load_passwd_file(vfs_tid: usize) -> Option<&'static [u8]> {
    let (handle, file_size, _) = vfs::open(vfs_tid, b"/etc/passwd")
        .or_else(|_| vfs::open(vfs_tid, b"/etc/PASSWD"))
        .ok()?;
    let size = file_size as usize;
    if size == 0 || size > PAGE_SIZE {
        let _ = vfs::close(vfs_tid, handle);
        return None;
    }

    let frame = syscall::sys_phys_alloc(1).ok()?;
    syscall::sys_map_phys(frame, PASSWD_BUF, 1).ok()?;
    vfs::read(vfs_tid, handle, frame, 0, size as u32).ok()?;
    let _ = vfs::close(vfs_tid, handle);

    Some(unsafe { core::slice::from_raw_parts(PASSWD_BUF as *const u8, size) })
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("login: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
