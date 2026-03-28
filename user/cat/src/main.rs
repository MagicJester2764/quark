#![no_std]
#![no_main]

use quark_rt::ipc::Message;
use quark_rt::{args, print, println, syscall, vfs};

const PAGE_SIZE: usize = 4096;
const NAMESERVER_TID: usize = 2;
const TAG_NS_LOOKUP: u64 = 2;
const BUF_VADDR: usize = 0x90_0000_0000;

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

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let argc = args::argc();
    if argc < 2 {
        println!("usage: cat <file> [file...]");
        syscall::sys_exit();
    }

    // Discover VFS
    let mut attempts = 0;
    let vfs_tid = loop {
        if let Some(tid) = lookup_service(b"vfs") {
            break tid;
        }
        attempts += 1;
        if attempts >= 20 {
            println!("cat: vfs not found");
            syscall::sys_exit();
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    };

    // Allocate a physical page for VFS reads
    let phys = match syscall::sys_phys_alloc(1) {
        Ok(addr) => addr,
        Err(()) => {
            println!("cat: failed to allocate buffer");
            syscall::sys_exit();
        }
    };
    if syscall::sys_map_phys(phys, BUF_VADDR, 1).is_err() {
        println!("cat: failed to map buffer");
        syscall::sys_exit();
    }

    for i in 1..argc {
        let path = match args::argv(i) {
            Some(p) => p,
            None => continue,
        };

        let (handle, size, is_dir) = match vfs::open(vfs_tid, path) {
            Ok(h) => h,
            Err(_) => {
                if let Ok(s) = core::str::from_utf8(path) {
                    println!("cat: {}: not found", s);
                }
                continue;
            }
        };

        if is_dir {
            if let Ok(s) = core::str::from_utf8(path) {
                println!("cat: {}: is a directory", s);
            }
            let _ = vfs::close(vfs_tid, handle);
            continue;
        }

        // Read and print file contents page by page
        let mut offset = 0u32;
        while offset < size {
            let to_read = PAGE_SIZE.min((size - offset) as usize) as u32;
            match vfs::read(vfs_tid, handle, phys, offset, to_read) {
                Ok(bytes_read) => {
                    if bytes_read == 0 {
                        break;
                    }
                    let data = unsafe {
                        core::slice::from_raw_parts(BUF_VADDR as *const u8, bytes_read as usize)
                    };
                    // Print as text
                    if let Ok(s) = core::str::from_utf8(data) {
                        print!("{}", s);
                    } else {
                        // Binary file — just print what we can
                        for &b in data {
                            if b >= 0x20 && b < 0x7F || b == b'\n' || b == b'\r' || b == b'\t' {
                                print!("{}", b as char);
                            } else {
                                print!(".");
                            }
                        }
                    }
                    offset += bytes_read;
                }
                Err(e) => {
                    println!("\ncat: read error: {}", e);
                    break;
                }
            }
        }

        let _ = vfs::close(vfs_tid, handle);
    }

    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("cat: PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
