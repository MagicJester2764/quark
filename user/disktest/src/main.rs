#![no_std]
#![no_main]

use libquark::ipc::Message;
use libquark::{println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_LOOKUP: u64 = 2;

// Disk IPC tags
const TAG_READ_SECTOR: u64 = 1;
const TAG_OK: u64 = 0;

// Address where we map our shared buffer page
const BUF_VADDR: usize = 0x87_0000_0000;

fn lookup_service(name: &[u8]) -> Option<usize> {
    let mut buf = [0u8; 24];
    let len = name.len().min(24);
    buf[..len].copy_from_slice(&name[..len]);
    let w0 = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let w1 = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
    let w2 = u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]);

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.data[0] != 0 {
        Some(reply.data[0] as usize)
    } else {
        None
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[disktest] Started.");

    // Wait a bit for disk driver to register
    for _ in 0..10 {
        syscall::sys_yield();
    }

    // Look up disk service
    let disk_tid = loop {
        if let Some(tid) = lookup_service(b"disk") {
            println!("[disktest] Found disk server at TID {}.", tid);
            break tid;
        }
        println!("[disktest] Waiting for disk server...");
        for _ in 0..100 {
            syscall::sys_yield();
        }
    };

    // Allocate a physical page for the shared buffer
    let phys = match syscall::sys_phys_alloc(1) {
        Ok(addr) => addr,
        Err(()) => {
            println!("[disktest] Failed to allocate physical page!");
            syscall::sys_exit();
        }
    };

    // Map it into our address space
    if syscall::sys_map_phys(phys, BUF_VADDR, 1).is_err() {
        println!("[disktest] Failed to map buffer page!");
        syscall::sys_exit();
    }

    // Zero the buffer
    unsafe {
        core::ptr::write_bytes(BUF_VADDR as *mut u8, 0, 4096);
    }

    // Read sector 0
    println!("[disktest] Reading sector 0...");
    let msg = Message {
        sender: 0,
        tag: TAG_READ_SECTOR,
        data: [0, phys as u64, 0, 0, 0, 0], // LBA=0, phys_addr
    };

    let mut reply = Message::empty();
    if syscall::sys_call(disk_tid, &msg, &mut reply).is_err() {
        println!("[disktest] IPC call to disk server failed!");
        syscall::sys_exit();
    }

    if reply.tag != TAG_OK {
        println!("[disktest] Disk read failed (tag={}).", reply.tag);
        syscall::sys_exit();
    }

    let bytes_read = reply.data[0];
    println!("[disktest] Read {} bytes from sector 0.", bytes_read);

    // Print first 64 bytes as hex + ASCII
    let data = unsafe { core::slice::from_raw_parts(BUF_VADDR as *const u8, 64) };
    for row in 0..4 {
        let off = row * 16;
        // Build entire line in a buffer to print atomically
        let mut line = [0u8; 80]; // "XXXX: XX XX ... XX  |................|\n"
        let mut pos = 0;
        // Offset
        let hex_chars = b"0123456789abcdef";
        line[pos] = hex_chars[((off >> 12) & 0xF) as usize]; pos += 1;
        line[pos] = hex_chars[((off >> 8) & 0xF) as usize]; pos += 1;
        line[pos] = hex_chars[((off >> 4) & 0xF) as usize]; pos += 1;
        line[pos] = hex_chars[(off & 0xF) as usize]; pos += 1;
        line[pos] = b':'; pos += 1;
        line[pos] = b' '; pos += 1;
        // Hex bytes
        for i in 0..16 {
            let b = data[off + i];
            line[pos] = hex_chars[(b >> 4) as usize]; pos += 1;
            line[pos] = hex_chars[(b & 0xF) as usize]; pos += 1;
            line[pos] = b' '; pos += 1;
        }
        // ASCII
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
