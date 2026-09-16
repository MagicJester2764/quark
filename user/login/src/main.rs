#![no_std]
#![no_main]

use quark_rt::nameserver;
use quark_rt::stdio::read_line;
use quark_rt::spawn::{self, Scratch};
use quark_rt::{passwd, print, println, syscall, vfs};

use quark_rt::manifest::CapReq;

// SetUid is the point of login; the rest mirrors the shell it spawns.
quark_rt::manifest!([
    CapReq::task_mgmt(0),
    CapReq::phys_alloc(64),
    CapReq::set_uid(),
    CapReq::ioport(0x604, 0x604),
    CapReq::ioport(0xB004, 0xB004),
]);

const PAGE_SIZE: usize = 4096;

// Login temp address ranges (non-overlapping with init 0x82-0x88, shell 0x90-0x93)
const FILE_BUF_BASE: usize = 0x94_0000_0000;
// Staging areas for quark_rt::spawn, in this task's own address space.
const ELF_TEMP: usize = 0x95_0000_0000;
const STACK_TEMP: usize = 0x96_0000_0000;
const ARGS_TEMP_PAGE: usize = 0x97_0000_0000;

/// Staging areas quark_rt::spawn maps through while building a child.
const SPAWN_SCRATCH: Scratch = Scratch {
    elf: ELF_TEMP,
    stack: STACK_TEMP,
    args: ARGS_TEMP_PAGE,
};
/// Where `/etc/passwd` is read to, a page at most. Read again at every prompt.
static mut PASSWD: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

// ---------------------------------------------------------------------------
// ELF loader
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Program arguments
// ---------------------------------------------------------------------------

/// Load the program at `path`, staged at `FILE_BUF_BASE` and released again.
/// login grants the shell its capabilities itself, so the manifest is not read.
fn load_program(vfs_tid: usize, path: &[u8]) -> Result<spawn::Spawned, ()> {
    spawn::load_path(vfs_tid, path, FILE_BUF_BASE, &SPAWN_SCRATCH, |_, _| {})
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let vfs_tid = match nameserver::lookup_retry(b"vfs", 50) {
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
        let info = match load_program(vfs_tid, shell_path) {
            Ok(info) => info,
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
                match load_program(vfs_tid, &alt[..alt_len]) {
                    Ok(info) => info,
                    Err(()) => {
                        if let Ok(s) = core::str::from_utf8(shell_path) {
                            println!("login: cannot load shell: {}", s);
                        }
                        continue;
                    }
                }
            }
        };

        let tid = info.tid;

        // Set child UID/GID
        let _ = syscall::sys_set_uid(tid, entry.uid);
        let _ = syscall::sys_set_gid(tid, entry.gid);

        // Grant shell capabilities (task mgmt + phys for spawning + ioport for shutdown)
        // Not CAP_MAP_PHYS: it mints a full-range PhysRange into the shell,
        // which would undo the narrowing below and in init.
        let _ = syscall::sys_grant_cap(
            tid,
            syscall::CAP_TASK_MGMT | syscall::CAP_PHYS_ALLOC | syscall::CAP_IOPORT,
        );
        // Pass on our own IPC reach. Delegating the capability itself rather
        // than minting a new one means we do not need to know which services
        // it names — sys_cap_inspect cannot report a 64-bit destination set.
        let _ = syscall::sys_cap_grant(tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);

        // Fine-grained caps for shell: TaskMgmt, PhysAlloc, IOPORT (ACPI
        // shutdown). No PhysRange: the shell stages its children out of frames
        // it allocated itself, and login holds none to mint from in any case.
        const SCRATCH: usize = 14;
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_TASK_MGMT, 0, 0);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 0);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_PHYS_ALLOC, 64, 0);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 1);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_IOPORT, 0x604, 0x604);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 2);
        let _ = syscall::sys_cap_delete(SCRATCH);
        let _ = syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_IOPORT, 0xB004, 0xB004);
        let _ = syscall::sys_cap_grant(tid, SCRATCH, 3);
        let _ = syscall::sys_cap_delete(SCRATCH);

        // Wire file descriptors
        let _ = syscall::sys_fd_dup(tid, 0, 0); // stdin
        let _ = syscall::sys_fd_dup(tid, 1, 1); // stdout
        let _ = syscall::sys_fd_dup(tid, 2, 2); // stderr

        // Pass shell name and home directory as argv
        let home = entry.home();
        let _ = spawn::set_args(&info, &[shell_path, home], &SPAWN_SCRATCH);

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

    let buf = unsafe { &mut *core::ptr::addr_of_mut!(PASSWD) };
    let got = vfs::read(vfs_tid, handle, &mut buf[..size], 0);
    let _ = vfs::close(vfs_tid, handle);
    match got {
        Ok(n) if n as usize == size => Some(&buf[..size]),
        _ => None,
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("login: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
