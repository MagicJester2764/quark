#![no_std]
#![no_main]

mod console;
#[allow(dead_code)]
mod fat32;
#[allow(dead_code)]
mod modules;
mod multiboot2;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: usize) -> ! {
    // Initialize module registry first — console::init needs it to find the VGA driver
    unsafe { modules::init(multiboot_info) };

    let fb = unsafe { multiboot2::parse_framebuffer(multiboot_info) };
    console::init(fb);
    console::clear();
    console::puts(b"Quark v0.1.0 - microkernel\n");
    console::puts(b"Booted successfully.\n");

    let mod_count = modules::count();
    if mod_count > 0 {
        console::puts(b"Boot modules:\n");
        for i in 0..mod_count {
            if let Some(m) = modules::get(i) {
                console::puts(b"  Module: ");
                console::puts(modules::name_str(m));
                console::puts(b" [");
                print_hex(m.start);
                console::puts(b"..");
                print_hex(m.end);
                console::puts(b"] (");
                print_dec(m.end - m.start);
                console::puts(b" bytes)\n");
            }
        }
    } else {
        console::puts(b"No boot modules loaded.\n");
    }

    // Initialize FAT32 driver
    fat32::init();
    if fat32::is_loaded() {
        console::puts(b"FAT32 driver loaded.\n");
    }

    // --- DEBUG: mount rootfs image and list contents ---
    debug_list_rootfs();
    // --- END DEBUG ---

    loop {
        core::hint::spin_loop();
    }
}

fn print_hex(val: usize) {
    console::puts(b"0x");
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 16];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        let digit = (n & 0xF) as u8;
        buf[i] = if digit < 10 { b'0' + digit } else { b'A' + digit - 10 };
        n >>= 4;
        i += 1;
    }
    // Reverse
    let mut out = [0u8; 16];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

fn print_dec(val: usize) {
    if val == 0 {
        console::puts(b"0");
        return;
    }
    let mut buf = [0u8; 20];
    let mut n = val;
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = buf[i - 1 - j];
    }
    console::puts(&out[..i]);
}

// --- DEBUG: list files on rootfs FAT32 image ---
fn debug_list_rootfs() {
    if !fat32::is_loaded() {
        return;
    }

    // Find rootfs image in boot modules
    let rootfs = modules::find(b"ROOTFS.IMG")
        .or_else(|| modules::find(b"rootfs.img"));
    let rootfs = match rootfs {
        Some(m) => m,
        None => return,
    };

    let data = unsafe { modules::data(rootfs) };
    if !fat32::mount(data.as_ptr(), data.len()) {
        console::puts(b"[debug] Failed to mount rootfs\n");
        return;
    }

    console::puts(b"[debug] rootfs contents:\n");
    debug_list_dir(b"/", 1);
}

fn debug_list_dir(path: &[u8], depth: usize) {
    let fd = match fat32::open(path) {
        Some(fd) => fd,
        None => return,
    };

    while let Some(entry) = fat32::readdir(fd) {
        // Skip . and .. entries
        if entry.name[0] == b'.' {
            continue;
        }

        // Indent
        for _ in 0..depth {
            console::puts(b"  ");
        }

        // Format and print name
        let mut name_buf = [0u8; 13];
        let name_len = fat32::format_83_name(&entry.name, &mut name_buf);

        let is_dir = entry.attr & 0x10 != 0;
        if is_dir {
            console::puts(&name_buf[..name_len]);
            console::puts(b"/\n");

            // Build child path for recursion
            let mut child_path = [0u8; 128];
            let mut pos = 0;
            // Copy parent path
            for &b in path.iter() {
                if pos < child_path.len() - 1 {
                    child_path[pos] = b;
                    pos += 1;
                }
            }
            // Add separator if needed
            if pos > 0 && child_path[pos - 1] != b'/' && pos < child_path.len() - 1 {
                child_path[pos] = b'/';
                pos += 1;
            }
            // Copy child name
            for i in 0..name_len {
                if pos < child_path.len() - 1 {
                    child_path[pos] = name_buf[i];
                    pos += 1;
                }
            }

            debug_list_dir(&child_path[..pos], depth + 1);
        } else {
            console::puts(&name_buf[..name_len]);
            console::puts(b" (");
            print_dec(entry.size as usize);
            console::puts(b" bytes)\n");
        }
    }

    fat32::close(fd);
}
// --- END DEBUG ---

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console::puts(b"\nKERNEL PANIC!");
    loop {
        core::hint::spin_loop();
    }
}
