#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{print, println, syscall, vfs};
use libquark::stdio::read_line;

const PAGE_SIZE: usize = 4096;
const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;

// Shell temp address ranges (non-overlapping with init's 0x82-0x88)
const FILE_BUF_BASE: usize = 0x90_0000_0000;
const ELF_TEMP: usize = 0x91_0000_0000;
const STACK_TEMP: usize = 0x92_0000_0000;
const ARGS_TEMP_PAGE: usize = 0x93_0000_0000;
const ARGS_PAGE_ADDR: usize = 0x80_8000_0000;

// ---------------------------------------------------------------------------
// ELF64 structures (copied from init)
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
// ELF loader (mirrors init's load_elf using shell temp addresses)
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
// Capability granting for child tasks
// ---------------------------------------------------------------------------

fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for i in 0..a.len() {
        let ca = if a[i] >= b'a' && a[i] <= b'z' { a[i] - 32 } else { a[i] };
        let cb = if b[i] >= b'a' && b[i] <= b'z' { b[i] - 32 } else { b[i] };
        if ca != cb {
            return false;
        }
    }
    true
}

fn grant_caps_by_name(name: &[u8], tid: usize) {
    if eq_ignore_case(name, b"cat") || eq_ignore_case(name, b"disktest") {
        let _ = syscall::sys_grant_cap(tid, syscall::CAP_PHYS_ALLOC | syscall::CAP_MAP_PHYS);
    }
}

fn build_path(cmd: &[u8], path_buf: &mut [u8; 64]) -> usize {
    let has_slash = cmd.iter().any(|&b| b == b'/');

    if has_slash {
        // Absolute/relative path — copy directly
        let len = cmd.len().min(64);
        path_buf[..len].copy_from_slice(&cmd[..len]);
        len
    } else {
        // Bare command — prepend /usr/bin/, append .ELF
        let prefix = b"/usr/bin/";
        let suffix = b".ELF";
        let cmd_len = cmd.len().min(64 - prefix.len() - suffix.len());
        path_buf[..prefix.len()].copy_from_slice(prefix);
        let mut pos = prefix.len();
        path_buf[pos..pos + cmd_len].copy_from_slice(&cmd[..cmd_len]);
        pos += cmd_len;
        path_buf[pos..pos + suffix.len()].copy_from_slice(suffix);
        pos += suffix.len();
        pos
    }
}

fn ends_with_elf(path: &[u8]) -> bool {
    path.len() >= 4 && eq_ignore_case(&path[path.len() - 4..], b".elf")
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

fn cmd_exec(
    cmd: &[u8],
    args_str: &[u8],
    vfs_tid: usize,
) {
    let mut path = [0u8; 64];
    let pos = build_path(cmd, &mut path);
    let has_slash = cmd.iter().any(|&b| b == b'/');

    // Open ELF file via VFS — try exact path first, then with .ELF appended
    let (file_handle, file_size, _) = match vfs::open(vfs_tid, &path[..pos]) {
        Ok(h) => h,
        Err(_) => {
            // If path had a slash and doesn't end in .ELF, retry with .ELF appended
            if has_slash && !ends_with_elf(&path[..pos]) && pos + 4 <= 64 {
                let suffix = b".ELF";
                path[pos..pos + 4].copy_from_slice(suffix);
                match vfs::open(vfs_tid, &path[..pos + 4]) {
                    Ok(h) => h,
                    Err(_) => {
                        if let Ok(s) = core::str::from_utf8(cmd) {
                            println!("{}: not found", s);
                        }
                        return;
                    }
                }
            } else {
                if let Ok(s) = core::str::from_utf8(cmd) {
                    println!("{}: not found", s);
                }
                return;
            }
        }
    };

    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Allocate pages and read file content into FILE_BUF_BASE
    let mut success = true;
    for p in 0..pages_needed {
        let frame = match syscall::sys_phys_alloc(1) {
            Ok(f) => f,
            Err(()) => { success = false; break; }
        };
        if syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1).is_err() {
            success = false; break;
        }
        let offset = (p * PAGE_SIZE) as u32;
        let to_read = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        if vfs::read(vfs_tid, file_handle, frame, offset, to_read).is_err() {
            success = false; break;
        }
    }
    let _ = vfs::close(vfs_tid, file_handle);

    if !success {
        println!("shell: failed to read ELF");
        return;
    }

    let elf_data = unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) };

    // Load ELF
    let info = match load_elf(elf_data) {
        Ok(i) => i,
        Err(()) => {
            println!("shell: failed to load ELF");
            return;
        }
    };

    let tid = info.tid;

    // Grant capabilities based on command basename (strip path and .ELF extension)
    let basename = if let Some(slash_pos) = cmd.iter().rposition(|&b| b == b'/') {
        &cmd[slash_pos + 1..]
    } else {
        cmd
    };
    let name = if ends_with_elf(basename) {
        &basename[..basename.len() - 4]
    } else {
        basename
    };
    grant_caps_by_name(name, tid);

    // Wire file descriptors — duplicate shell's own fds to child
    let _ = syscall::sys_fd_dup(tid, 0, 0); // stdin
    let _ = syscall::sys_fd_dup(tid, 1, 1); // stdout
    let _ = syscall::sys_fd_dup(tid, 2, 2); // stderr

    // Build argv: [command_name, ...split args]
    let mut argv_bufs: [&[u8]; 16] = [b""; 16];
    let mut argc = 0;
    argv_bufs[argc] = cmd;
    argc += 1;

    // Split args_str by spaces into argv
    if !args_str.is_empty() {
        let mut i = 0;
        while i < args_str.len() && argc < 16 {
            // Skip spaces
            while i < args_str.len() && args_str[i] == b' ' {
                i += 1;
            }
            if i >= args_str.len() {
                break;
            }
            let start = i;
            while i < args_str.len() && args_str[i] != b' ' {
                i += 1;
            }
            argv_bufs[argc] = &args_str[start..i];
            argc += 1;
        }
    }

    let _ = set_args(&info, &argv_bufs[..argc]);

    // Start and wait
    if info.start().is_err() {
        println!("shell: failed to start task");
        return;
    }

    let _ = syscall::sys_wait();
}

// ---------------------------------------------------------------------------
// Current working directory
// ---------------------------------------------------------------------------

static mut CWD: [u8; 64] = [0; 64];
static mut CWD_LEN: usize = 0;

fn cwd_init() {
    unsafe {
        let home = b"/home/root";
        CWD[..home.len()].copy_from_slice(home);
        CWD_LEN = home.len();
    }
}

fn cwd_get() -> &'static [u8] {
    unsafe { &CWD[..CWD_LEN] }
}

fn cwd_set(path: &[u8]) {
    unsafe {
        let len = path.len().min(64);
        CWD[..len].copy_from_slice(&path[..len]);
        CWD_LEN = len;
    }
}

/// Resolve a path relative to cwd. Handles `.`, `..`, absolute paths.
/// Returns the resolved path length written into `out`.
fn resolve_path(arg: &[u8], out: &mut [u8; 128]) -> usize {
    if arg.first() == Some(&b'/') {
        // Absolute path — start fresh
        out[0] = b'/';
        resolve_components(&arg[1..], out, 1)
    } else {
        // Relative path — start from cwd
        let cwd = cwd_get();
        out[..cwd.len()].copy_from_slice(cwd);
        let mut pos = cwd.len();
        // Ensure trailing slash for joining
        if pos > 0 && out[pos - 1] != b'/' && pos < 128 {
            out[pos] = b'/';
            pos += 1;
        }
        resolve_components(arg, out, pos)
    }
}

fn resolve_components(components: &[u8], out: &mut [u8; 128], start: usize) -> usize {
    let mut pos = start;
    let mut i = 0;
    while i <= components.len() {
        // Find next component (split by '/')
        let comp_start = i;
        while i < components.len() && components[i] != b'/' {
            i += 1;
        }
        let comp = &components[comp_start..i];
        i += 1; // skip '/'

        if comp.is_empty() || comp == b"." {
            continue;
        } else if comp == b".." {
            // Go up: remove last component
            if pos > 1 {
                // Remove trailing slash
                if out[pos - 1] == b'/' {
                    pos -= 1;
                }
                // Find previous slash
                while pos > 1 && out[pos - 1] != b'/' {
                    pos -= 1;
                }
            }
        } else {
            // Append component
            if pos > 0 && out[pos - 1] != b'/' && pos < 128 {
                out[pos] = b'/';
                pos += 1;
            }
            let len = comp.len().min(128 - pos);
            out[pos..pos + len].copy_from_slice(&comp[..len]);
            pos += len;
        }
    }
    // Ensure at least "/"
    if pos == 0 {
        out[0] = b'/';
        pos = 1;
    }
    pos
}

fn cmd_cd(args_str: &[u8], vfs_tid: usize) {
    let arg = args_str.trim_ascii();
    if arg.is_empty() {
        // cd with no args goes to /
        cwd_set(b"/");
        return;
    }

    let mut resolved = [0u8; 128];
    let len = resolve_path(arg, &mut resolved);
    let path = &resolved[..len];

    // Verify it's a valid directory via VFS
    match vfs::open(vfs_tid, path) {
        Ok((handle, _, is_dir)) => {
            let _ = vfs::close(vfs_tid, handle);
            if is_dir {
                cwd_set(path);
            } else {
                if let Ok(s) = core::str::from_utf8(arg) {
                    println!("cd: not a directory: {}", s);
                }
            }
        }
        Err(_) => {
            if let Ok(s) = core::str::from_utf8(arg) {
                println!("cd: no such directory: {}", s);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    cwd_init();

    // Discover services
    let vfs_tid = match lookup_service_with_retry(b"vfs", 50) {
        Some(tid) => tid,
        None => {
            println!("shell: vfs not found");
            syscall::sys_exit();
        }
    };

    // Main loop
    let mut line_buf = [0u8; 256];
    loop {
        if let Ok(s) = core::str::from_utf8(cwd_get()) {
            print!("{}", s);
            if !s.ends_with('/') {
                print!("/");
            }
        }
        print!("$ ");

        let n = read_line(&mut line_buf);
        if n == 0 {
            continue;
        }

        let line = &line_buf[..n];

        // Trim trailing newline/whitespace
        let mut end = line.len();
        while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r' || line[end - 1] == b' ') {
            end -= 1;
        }
        if end == 0 {
            continue;
        }
        let line = &line[..end];

        // Trim leading whitespace
        let mut start = 0;
        while start < line.len() && line[start] == b' ' {
            start += 1;
        }
        if start >= line.len() {
            continue;
        }
        let line = &line[start..];

        // Split into command and args
        let mut split = line.len();
        for i in 0..line.len() {
            if line[i] == b' ' {
                split = i;
                break;
            }
        }
        let cmd = &line[..split];
        let args_str = if split < line.len() {
            &line[split + 1..]
        } else {
            &[] as &[u8]
        };

        // Builtin: exit
        if cmd == b"exit" {
            syscall::sys_exit();
        }

        // Builtin: cd
        if cmd == b"cd" {
            cmd_cd(args_str, vfs_tid);
            continue;
        }

        // Builtin: pwd
        if cmd == b"pwd" {
            if let Ok(s) = core::str::from_utf8(cwd_get()) {
                println!("{}", s);
            }
            continue;
        }

        // Resolve `.` and `..` in arguments for external commands
        let mut resolved_args_buf = [0u8; 256];
        let resolved_args = resolve_args(cmd, args_str, &mut resolved_args_buf);

        // External command
        cmd_exec(cmd, resolved_args, vfs_tid);
    }
}

/// For commands that take path arguments, resolve `.` and `..` relative to cwd.
/// For `ls` with no args, inject cwd as the argument.
fn resolve_args<'a>(cmd: &[u8], args_str: &[u8], buf: &'a mut [u8; 256]) -> &'a [u8] {
    let trimmed = args_str.trim_ascii();

    // ls with no args → use cwd
    if eq_ignore_case(cmd, b"ls") && trimmed.is_empty() {
        let cwd = cwd_get();
        buf[..cwd.len()].copy_from_slice(cwd);
        return &buf[..cwd.len()];
    }

    // Resolve each argument that looks like a path (contains . or ..)
    // For simplicity, resolve each space-separated token individually
    let mut out_pos = 0;
    let mut i = 0;
    let bytes = args_str;
    while i < bytes.len() {
        // Copy leading spaces
        while i < bytes.len() && bytes[i] == b' ' {
            if out_pos < 256 { buf[out_pos] = b' '; out_pos += 1; }
            i += 1;
        }
        if i >= bytes.len() { break; }

        // Extract token
        let tok_start = i;
        while i < bytes.len() && bytes[i] != b' ' {
            i += 1;
        }
        let token = &bytes[tok_start..i];

        // Check if token needs path resolution (starts with . or ..)
        if token == b"." || token == b".."
            || token.starts_with(b"./") || token.starts_with(b"../")
        {
            let mut resolved = [0u8; 128];
            let len = resolve_path(token, &mut resolved);
            let copy_len = len.min(256 - out_pos);
            buf[out_pos..out_pos + copy_len].copy_from_slice(&resolved[..copy_len]);
            out_pos += copy_len;
        } else {
            let copy_len = token.len().min(256 - out_pos);
            buf[out_pos..out_pos + copy_len].copy_from_slice(&token[..copy_len]);
            out_pos += copy_len;
        }
    }
    &buf[..out_pos]
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("shell: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
