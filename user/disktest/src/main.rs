#![no_std]
#![no_main]

use quark_rt::nameserver;
use quark_rt::{println, syscall, vfs};

use quark_rt::manifest::CapReq;

quark_rt::manifest!([CapReq::phys_alloc(64)]);

const PAGE_SIZE: usize = 4096;

// Address where we map our shared buffer page
const BUF_VADDR: usize = 0x87_0000_0000;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[disktest] Started.");

    // Look up VFS service
    let mut attempts = 0;
    let vfs_tid = loop {
        if let Some(tid) = nameserver::lookup(b"vfs") {
            println!("[disktest] Found VFS at TID {}.", tid);
            break tid;
        }
        attempts += 1;
        if attempts >= 20 {
            println!("[disktest] VFS not found.");
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
            println!("[disktest] Failed to allocate physical page!");
            syscall::sys_exit();
        }
    };
    if syscall::sys_map_phys(phys, BUF_VADDR, 1).is_err() {
        println!("[disktest] Failed to map buffer page!");
        syscall::sys_exit();
    }

    // List root directory
    println!("[disktest] Listing /:");
    match vfs::open(vfs_tid, b"/") {
        Ok((handle, _, _)) => {
            let mut idx = 0u32;
            loop {
                match vfs::readdir(vfs_tid, handle, idx) {
                    Ok(Some(entry)) => {
                        let name = entry.name_bytes();
                        let kind = if entry.is_dir { "DIR " } else { "FILE" };
                        if let Ok(s) = core::str::from_utf8(name) {
                            println!("  {} {} ({} bytes)", kind, s, entry.size);
                        }
                        idx += 1;
                    }
                    Ok(None) => break,
                    Err(e) => {
                        println!("[disktest] readdir error: {}", e);
                        break;
                    }
                }
            }
            let _ = vfs::close(vfs_tid, handle);
        }
        Err(e) => println!("[disktest] Failed to open /: {}", e),
    }

    // Read first 64 bytes of /usr/bin/HELLO.ELF via VFS
    println!("[disktest] Reading /usr/bin/HELLO.ELF...");
    match vfs::open(vfs_tid, b"/usr/bin/HELLO.ELF") {
        Ok((handle, size, _)) => {
            println!("[disktest] Opened (size={}).", size);

            // Read first page
            match vfs::read(vfs_tid, handle, phys, 0, PAGE_SIZE as u32) {
                Ok(bytes_read) => {
                    println!("[disktest] Read {} bytes.", bytes_read);
                    let dump_len = 64.min(bytes_read as usize);
                    let data = unsafe {
                        core::slice::from_raw_parts(BUF_VADDR as *const u8, dump_len)
                    };
                    for row in 0..(dump_len / 16) {
                        let off = row * 16;
                        let mut line = [0u8; 80];
                        let mut pos = 0;
                        let hex_chars = b"0123456789abcdef";
                        line[pos] = hex_chars[(off >> 12) & 0xF]; pos += 1;
                        line[pos] = hex_chars[(off >> 8) & 0xF]; pos += 1;
                        line[pos] = hex_chars[(off >> 4) & 0xF]; pos += 1;
                        line[pos] = hex_chars[off & 0xF]; pos += 1;
                        line[pos] = b':'; pos += 1;
                        line[pos] = b' '; pos += 1;
                        for i in 0..16 {
                            let b = data[off + i];
                            line[pos] = hex_chars[(b >> 4) as usize]; pos += 1;
                            line[pos] = hex_chars[(b & 0xF) as usize]; pos += 1;
                            line[pos] = b' '; pos += 1;
                        }
                        line[pos] = b' '; pos += 1;
                        line[pos] = b'|'; pos += 1;
                        for i in 0..16 {
                            let b = data[off + i];
                            line[pos] = if b >= 0x20 && b < 0x7F { b } else { b'.' };
                            pos += 1;
                        }
                        line[pos] = b'|'; pos += 1;
                        if let Ok(s) = core::str::from_utf8(&line[..pos]) {
                            println!("{}", s);
                        }
                    }
                }
                Err(e) => println!("[disktest] Read error: {}", e),
            }

            let _ = vfs::close(vfs_tid, handle);
        }
        Err(e) => println!("[disktest] Failed to open file: {}", e),
    }

    println!("[disktest] Done.");
    syscall::sys_exit();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[disktest] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
