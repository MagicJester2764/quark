#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{args, println, syscall, vfs};

const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;

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

/// Convert a DirEntry name to a displayable string.
/// For FAT32 8.3 names (name_len=11 with spaces), converts to "NAME.EXT" format.
/// For ext2 names, uses the name directly.
fn entry_name_to_str<'a>(entry: &vfs::DirEntry, buf: &'a mut [u8; 48]) -> &'a [u8] {
    let name = entry.name_bytes();
    // Detect FAT32 8.3 format: exactly 11 bytes, no dot, has trailing spaces
    if entry.name_len == 11 && !name.contains(&b'.') {
        let base_len = name[0..8]
            .iter()
            .rposition(|&b| b != b' ')
            .map_or(0, |p| p + 1);
        let mut pos = 0;
        for i in 0..base_len {
            buf[pos] = name[i];
            pos += 1;
        }
        let ext_len = name[8..11]
            .iter()
            .rposition(|&b| b != b' ')
            .map_or(0, |p| p + 1);
        if ext_len > 0 {
            buf[pos] = b'.';
            pos += 1;
            for i in 0..ext_len {
                buf[pos] = name[8 + i];
                pos += 1;
            }
        }
        &buf[..pos]
    } else {
        // ext2 or other: use name directly
        buf[..name.len()].copy_from_slice(name);
        &buf[..name.len()]
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Discover VFS
    let mut attempts = 0;
    let vfs_tid = loop {
        if let Some(tid) = lookup_service(b"vfs") {
            break tid;
        }
        attempts += 1;
        if attempts >= 20 {
            println!("ls: vfs not found");
            syscall::sys_exit();
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
            syscall::sys_exit();
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

    // Read all directory entries in one bulk IPC call (static to avoid stack overflow)
    static mut ENTRIES: [vfs::DirEntry; 64] = [vfs::DirEntry::empty(); 64];
    let entries = unsafe { &mut ENTRIES };
    let count = match vfs::readdir_bulk(vfs_tid, handle, entries) {
        Ok(n) => n,
        Err(e) => {
            println!("ls: readdir error: {}", e);
            let _ = vfs::close(vfs_tid, handle);
            syscall::sys_exit();
        }
    };

    for i in 0..count {
        let entry = &entries[i];
        let name = entry.name_bytes();
        if let Ok(s) = core::str::from_utf8(name) {
            if entry.is_dir {
                println!("{}/ ", s);
            } else {
                println!("{}  {}", s, entry.size);
            }
        }
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
