#![no_std]
#![no_main]

use quark_rt::nameserver;
use quark_rt::{args, println, syscall, vfs};

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(vfs_tid) = nameserver::lookup_retry(b"vfs", 20) else {
        println!("ls: vfs not found");
        syscall::sys_exit_code(1);
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
                } else if entry.kind == vfs::DT_LNK {
                    print_link(vfs_tid, path, entry.name_bytes(), s);
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

/// `name -> target` for the link `name` in directory `dir`.
fn print_link(vfs_tid: usize, dir: &[u8], name: &[u8], shown: &str) {
    let mut full = [0u8; vfs::MAX_PATH + 1];
    let slash = !dir.ends_with(b"/");
    let len = dir.len() + slash as usize + name.len();
    let mut target = [0u8; 4096];
    let answer = if len > vfs::MAX_PATH {
        Err(vfs::ERR_NAME_TOO_LONG)
    } else {
        full[..dir.len()].copy_from_slice(dir);
        if slash {
            full[dir.len()] = b'/';
        }
        full[len - name.len()..len].copy_from_slice(name);
        vfs::readlink(vfs_tid, &full[..len], &mut target)
    };
    match answer {
        Ok(n) => match core::str::from_utf8(&target[..n.min(target.len())]) {
            Ok(t) => println!("{} -> {}", shown, t),
            Err(_) => println!("{} -> (not text)", shown),
        },
        Err(e) => println!("{} -> (error {})", shown, e),
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("ls: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
