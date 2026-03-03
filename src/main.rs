#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

mod console;
mod context;
#[allow(dead_code)]
mod fat32;
mod heap;
mod idt;
mod io;
pub mod ipc;
#[allow(dead_code)]
mod modules;
mod multiboot2;
#[allow(dead_code)]
pub mod paging;
mod pic;
mod pit;
#[allow(dead_code)]
mod pmm;
pub mod scheduler;
mod syscall;
mod services;
mod task;
#[allow(dead_code)]
mod userspace;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: usize) -> ! {
    // Parse multiboot2 info (modules list, memory map, framebuffer)
    unsafe { modules::init(multiboot_info) };
    let fb = unsafe { multiboot2::parse_framebuffer(multiboot_info) };
    let (mmap_count, mmap_regions) = unsafe { multiboot2::parse_memory_map(multiboot_info) };

    // Initialize PMM before drivers so they can allocate pages
    let mb_info_size = unsafe { *(multiboot_info as *const u32) } as usize;
    unsafe { pmm::init(&mmap_regions, mmap_count, multiboot_info, mb_info_size) };

    // Initialize console (VGA driver receives kernel services)
    console::init(fb);
    console::clear();
    console::puts(b"Quark v0.1.0 - microkernel\n");
    unsafe { heap::init() };
    console::puts(b"Heap initialized.\n");
    unsafe { idt::init() };

    // Initialize hardware interrupts
    unsafe {
        pic::init();
        pit::init(100); // 100 Hz timer
        pic::enable_irq(0); // timer
        pic::enable_irq(1); // keyboard
        core::arch::asm!("sti", options(nostack, nomem));
    }
    console::puts(b"Interrupts enabled.\n");

    // Print boot modules
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

    // Initialize FAT32 driver (receives kernel services)
    fat32::init();
    if fat32::is_loaded() {
        console::puts(b"FAT32 driver loaded.\n");
    }

    // Print memory map
    console::puts(b"Memory map:\n");
    for i in 0..mmap_count {
        let r = &mmap_regions[i];
        console::puts(b"  ");
        print_hex(r.base as usize);
        console::puts(b" - ");
        print_hex((r.base + r.length) as usize);
        console::puts(b" (");
        print_dec(r.length as usize / 1024);
        console::puts(b" KiB) type=");
        print_dec(r.region_type as usize);
        console::puts(b"\n");
    }

    // Print PMM stats
    console::puts(b"PMM initialized: ");
    print_dec(pmm::free_count());
    console::puts(b" free frames (");
    print_dec(pmm::free_count() * 4);
    console::puts(b" KiB)\n");

    // Initialize syscall/sysret mechanism
    unsafe { syscall::init() };

    // Initialize scheduler and spawn test tasks
    scheduler::init();
    console::puts(b"Scheduler initialized.\n");

    scheduler::spawn(task_a);
    scheduler::spawn(task_b);
    scheduler::spawn(task_c);
    console::puts(b"Spawned 3 test tasks.\n");

    // Idle loop — the scheduler returns here when no tasks are ready
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) };
        scheduler::reap_dead();
    }
}

fn task_a() {
    for i in 0..5u32 {
        console::puts(b"[Task A] iteration ");
        print_dec(i as usize);
        console::puts(b"\n");
        // Busy-wait a bit so we can see interleaving
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
    }
    console::puts(b"[Task A] done.\n");
}

fn task_b() {
    for i in 0..5u32 {
        console::puts(b"[Task B] iteration ");
        print_dec(i as usize);
        console::puts(b"\n");
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
    }
    console::puts(b"[Task B] done.\n");
}

fn task_c() {
    for i in 0..5u32 {
        console::puts(b"[Task C] iteration ");
        print_dec(i as usize);
        console::puts(b"\n");
        for _ in 0..100_000 {
            core::hint::spin_loop();
        }
    }
    console::puts(b"[Task C] done.\n");
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console::puts(b"\nKERNEL PANIC!");
    loop {
        core::hint::spin_loop();
    }
}
