#![no_std]
#![no_main]

use quark_rt::nameserver;
use quark_rt::{args, println, syscall, vfs};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Discover VFS
    let mut attempts = 0;
    let vfs_tid = loop {
        if let Some(tid) = nameserver::lookup(b"vfs") {
            break tid;
        }
        attempts += 1;
        if attempts >= 20 {
            println!("ls: vfs not found");
            syscall::sys_exit_code(1);
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    };

    // Get path from argv[1], default to /
    let path = if let Some(arg) = args::argv(1) {
        arg
    } else {
        b"/" as &[u8]
    };

    // Open directory
    let (handle, file_size, is_dir) = match vfs::open(vfs_tid, path) {
        Ok(h) => h,
        Err(e) => {
            if let Ok(s) = core::str::from_utf8(path) {
                println!("ls: cannot open '{}': error {}", s, e);
            }
            syscall::sys_exit_code(1);
        }
    };

    if !is_dir {
        // Print file info like a single directory entry
        let name = if let Some(pos) = path.iter().rposition(|&b| b == b'/') {
            &path[pos + 1..]
        } else {
            path
        };
        if let Ok(s) = core::str::from_utf8(name) {
            println!("{}  {}", s, file_size);
        }
        let _ = vfs::close(vfs_tid, handle);
        syscall::sys_exit();
    }

    // A page of entries at a time, however many pages the directory takes.
    static mut ENTRIES: [vfs::DirEntry; 64] = [vfs::DirEntry::empty(); 64];
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(ENTRIES) };
    let mut next = 0u64;
    loop {
        let page = match vfs::readdir_bulk(vfs_tid, handle, next, entries) {
            Ok(p) => p,
            Err(e) => {
                println!("ls: readdir error: {}", e);
                let _ = vfs::close(vfs_tid, handle);
                syscall::sys_exit_code(1);
            }
        };
        for entry in &entries[..page.count] {
            if let Ok(s) = core::str::from_utf8(entry.name_bytes()) {
                if entry.is_dir {
                    println!("{}/ ", s);
                } else {
                    println!("{}  {}", s, entry.size);
                }
            }
        }
        if page.end || page.count == 0 {
            break;
        }
        next = page.next;
    }

    let _ = vfs::close(vfs_tid, handle);
    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("ls: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
