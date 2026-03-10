#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

mod console;
mod context;
mod fat32;
mod heap;
mod idt;
mod io;
pub mod ipc;
pub mod irq_dispatch;
mod modules;
mod multiboot2;
pub mod paging;
mod pic;
mod pit;
mod pmm;
pub mod scheduler;
pub mod sync;
pub mod syscall;
mod elf;
mod futex;
mod services;
mod shmem;
pub mod task;
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

    // Print PMM stats
    console::puts(b"PMM initialized: ");
    print_dec(pmm::free_count());
    console::puts(b" free frames (");
    print_dec(pmm::free_count() * 4);
    console::puts(b" KiB)\n");

    // Save kernel CR3 before any user address spaces are created
    paging::save_kernel_cr3();

    // Initialize syscall/sysret mechanism
    unsafe { syscall::init() };

    // Initialize scheduler
    scheduler::init();
    console::puts(b"Scheduler initialized.\n");

    // Load init process from boot module named "init"
    if let Some(m) = modules::find(b"init") {
        let elf_data = unsafe { modules::data(m) };
        console::puts(b"Loading init from module: ");
        console::puts(modules::name_str(m));
        console::puts(b"\n");
        match userspace::spawn_init(elf_data, fb) {
            Some(tid) => {
                console::puts(b"Init spawned (TID ");
                print_dec(tid);
                console::puts(b").\n");
            }
            None => {
                console::puts(b"FATAL: Failed to load init!\n");
            }
        }
    } else {
        console::puts(b"No init module found.\n");
    }

    // Idle loop — the scheduler returns here when no tasks are ready
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) };
        scheduler::reap_dead();
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

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console::puts(b"\nKERNEL PANIC!");
    loop {
        core::hint::spin_loop();
    }
}
