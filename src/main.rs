#![no_std]
#![no_main]
#![feature(alloc_error_handler)]

extern crate alloc;

mod acpi;
pub mod cap;
mod console;
mod cpu;
mod context;
mod devmem;
mod fat32;
mod fpu;
mod heap;
mod idt;
mod intc;
mod io;
mod ioapic;
pub mod ipc;
pub mod irq_dispatch;
mod lend;
mod memobj;
mod modules;
mod multiboot2;
mod percpu;
pub mod paging;
mod pic;
mod pit;
mod random;
mod pmm;
mod rtc;
pub mod scheduler;
pub mod sync;
pub mod syscall;
mod elf;
mod futex;
mod services;
mod shmem;
mod pollset;
mod served;
mod signal;
mod smp;
mod job;
mod klock;
mod lapic;
mod stream;
pub mod pipe;
mod pty;
mod timerfd;
mod tlb;
mod eventfd;
mod fdtable;
pub mod serial;
pub mod task;
mod userspace;

use core::panic::PanicInfo;

core::arch::global_asm!(include_str!("boot.s"), options(att_syntax));

#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(multiboot_info: usize) -> ! {
    // Before anything else: which task is running is asked through GS, and
    // GS finds this processor's own state only once this has said where it is.
    unsafe {
        unsafe extern "C" {
            static boot_stack_bottom: u8;
            static boot_stack_top: u8;
        }
        percpu::init(0, (&raw const boot_stack_bottom as usize, &raw const boot_stack_top as usize));
    }
    // This processor is in the kernel, and until it has nothing to do it
    // stays there.
    klock::acquire();

    // Initialize serial debug output early
    serial::init();
    serial::puts(b"[serial] Quark booting\n");

    // Parse multiboot2 info (modules list, memory map, framebuffer)
    unsafe { modules::init(multiboot_info) };
    let fb = unsafe { multiboot2::parse_framebuffer(multiboot_info) };
    let (mmap_count, mmap_regions) = unsafe { multiboot2::parse_memory_map(multiboot_info) };

    // Initialize PMM before drivers so they can allocate pages
    let mb_info_size = unsafe { *(multiboot_info as *const u32) } as usize;
    unsafe { pmm::init(&mmap_regions, mmap_count, multiboot_info, mb_info_size) };

    // Initialize console (VGA driver receives kernel services)
    paging::save_kernel_cr3();
    console::init(fb);
    console::clear();
    unsafe { heap::init() };
    console::puts(b"Heap initialized.\n");
    // What the machine is made of, as its firmware tells it: how many
    // processors, where the interrupt controllers are, how to turn it off.
    unsafe {
        let rsdp = multiboot2::rsdp(multiboot_info);
        acpi::init(rsdp.as_ref().map(|(bytes, len)| &bytes[..*len]));
    }
    // Before any task exists: the clean state every task is created with is
    // captured here, and the first task is entered without a switch to load it.
    fpu::init();
    unsafe { idt::init() };

    // SMEP/SMAP: block ring 0 from executing or casually touching user pages.
    // Must follow paging setup and precede the first user-mode entry.
    unsafe { cpu::init_protections() };

    // Initialize hardware interrupts: through the I/O APIC where the
    // firmware lists one, and the 8259s where it does not.
    unsafe {
        intc::init();
        // Which local APIC is the first processor's: where every device's
        // interrupt is sent, a message's included.
        if lapic::present() {
            percpu::set_apic_id(lapic::id());
        }
        pit::init(100); // 100 Hz timer
        intc::enable(0); // timer
        intc::enable(1); // keyboard
        core::arch::asm!("sti", options(nostack, nomem));
    }
    console::puts(b"Interrupts enabled.\n");
    if ioapic::in_use() {
        serial::puts(b"Interrupts: through the I/O APIC, to the first processor.\n");
        ioapic::describe();
    } else {
        serial::puts(b"Interrupts: through the 8259.\n");
    }
    rtc::init();
    random::init();

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

    // Initialize syscall/sysret mechanism
    unsafe { syscall::init() };

    // The other processors, if the machine has any. Here: after everything a
    // processor is given has been decided on this one, and before there is
    // a task for a tick to switch to.
    unsafe { smp::start(&mmap_regions[..mmap_count], (multiboot_info, mb_info_size)) };

    // Where devices are, for the first task to be given.
    unsafe { devmem::init(&mmap_regions[..mmap_count]) };

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

    console::puts(b"Welcome to Quark (v0.1.0)\n");

    // What a processor does when nothing is ready: the scheduler comes back
    // to here.
    scheduler::idle()
}

#[allow(dead_code)] // panic/exception diagnostic helper
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
    // Stop the machine. The old handler spun with interrupts still enabled, so
    // the timer kept firing and the scheduler kept switching tasks *after* a
    // panic — running the rest of the system on top of whatever inconsistent
    // kernel state caused it.
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    // And the other processors with it, before they can do more with
    // whatever state this one found itself unable to go on with.
    smp::halt_others();
    serial::puts(b"\nKERNEL PANIC!");
    // Where, and what was said: every panic in the kernel is a sentence.
    if let Some(at) = _info.location() {
        serial::puts(b" at ");
        serial::puts(at.file().as_bytes());
        serial::puts(b":");
        serial::put_usize(at.line() as usize);
    }
    if let Some(said) = _info.message().as_str() {
        serial::puts(b": ");
        serial::puts(said.as_bytes());
    }
    serial::puts(b"\n");
    console::puts(b"\nKERNEL PANIC!");
    loop {
        unsafe { core::arch::asm!("hlt", options(nostack, nomem)) };
    }
}
