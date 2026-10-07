//! Kernel stacks, in a region of their own, each with a page below it that
//! is not mapped.
//!
//! A task's kernel stack was 64 KiB of the heap, with whatever the heap had
//! put below it: a task that ran out wrote on down through other tasks'
//! records and stacks, and the fault it came to at the bottom of the heap —
//! a double fault, its stack being gone — said nothing of why. A stack is in
//! the gigabyte below the heap now ([`REGION`]), in 4 KiB pages under a page
//! directory every address space shares, made here before there is an
//! address space ([`init`]); and the page below each is left unmapped, so
//! that running out is a fault on that page, and the report says so
//! ([`is_guard`], `idt.rs`). The first processor's boot stack, in the
//! kernel's image, has such a page too; and the stacks the other processors
//! idle on and every processor's stack for double faults come from here.
//!
//! Every stack is painted when it is made, and how much of it was ever used
//! is read off when it is given back: the deepest any has gone is kept, and
//! serial says it each time it is passed and at shutdown ([`say_deepest`]).
//!
//! A stack given back is unmapped and its frames freed, with every processor
//! told first: every address space has the region (`tlb::stale_everywhere`).

use crate::paging;
use crate::pmm;
use core::sync::atomic::{AtomicUsize, Ordering};

const PAGE: usize = 4096;

/// A kernel stack's pages.
pub const KSTACK_PAGES: usize = 16;
pub const KSTACK_SIZE: usize = KSTACK_PAGES * PAGE;

/// The gigabyte below the heap's: the second-to-last of PML4[0].
pub const REGION: usize = 0x7F_8000_0000;
const REGION_END: usize = 0x7F_C000_0000;
/// A slot: the page left unmapped, and the stack above it.
const SLOT: usize = (KSTACK_PAGES + 1) * PAGE;
const SLOTS: usize = (REGION_END - REGION) / SLOT;

/// A bit for each slot that holds a stack.
static mut USED: [u64; SLOTS.div_ceil(64)] = [0; SLOTS.div_ceil(64)];

/// What a stack is painted with when it is made. What is still there was
/// never used.
const PAINT: u64 = 0x6B63_6174_536B_2121;

/// The deepest any stack has been used, in bytes.
static DEEPEST: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" {
    static boot_stack_guard: u8;
    static boot_stack_bottom: u8;
    static boot_stack_top: u8;
}

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

/// Make the region's page directory, while there is no address space to
/// have missed it; leave the page below the boot stack unmapped, and paint
/// the boot stack below where it is; and give the first processor its
/// stack for double faults.
///
/// # Safety
/// Once, on the first processor, after the kernel's map of memory and
/// before any address space is made or another processor started.
pub unsafe fn init() {
    unsafe {
        let kernel = paging::kernel_cr3();
        // The region's first page is the first slot's: leaving it unmapped
        // makes the page directory, under PML4[0], which every address space
        // is made sharing.
        paging::leave_unmapped(kernel, REGION).expect("kstack: no page directory for kernel stacks");
        // The boot stack's is in the kernel's image, in a page of two
        // megabytes, which is split for it.
        paging::leave_unmapped(kernel, &raw const boot_stack_guard as usize)
            .expect("kstack: the boot stack's guard page");
        paging::write_cr3(paging::read_cr3());
        let here: usize;
        core::arch::asm!("mov {}, rsp", out(reg) here, options(nostack, nomem));
        paint(&raw const boot_stack_bottom as usize, (here - 256) & !7);
        let (_, top) = alloc().expect("kstack: no stack for double faults");
        crate::percpu::set_df_stack(0, top);
    }
}

/// A stack, as `(base, top)`: `KSTACK_PAGES` pages mapped from `base`, with
/// the page below it unmapped, and painted. `None` if every slot is in use or
/// there is no memory for it.
pub fn alloc() -> Option<(usize, usize)> {
    let flags = irq_save();
    let made = unsafe { make() };
    irq_restore(flags);
    made
}

/// [`alloc`], with interrupts off: two tasks on one processor making stacks
/// would otherwise make one table between them, twice.
unsafe fn make() -> Option<(usize, usize)> {
    unsafe {
        let used = &mut *(&raw mut USED);
        let word = used.iter().position(|&w| w != u64::MAX)?;
        let slot = word * 64 + (!used[word]).trailing_zeros() as usize;
        if slot >= SLOTS {
            return None;
        }
        used[word] |= 1 << (slot % 64);
        let base = REGION + slot * SLOT + PAGE;
        for i in 0..KSTACK_PAGES {
            let Some(frame) = pmm::alloc() else {
                give_back(base, i);
                return None;
            };
            let flags = paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE;
            if paging::map_page(paging::kernel_cr3(), base + i * PAGE, frame.address(), flags).is_err() {
                pmm::free(frame);
                give_back(base, i);
                return None;
            }
        }
        paint(base, base + KSTACK_SIZE);
        Some((base, base + KSTACK_SIZE))
    }
}

/// Give back the stack at `base`, saying how much of it was used if that is
/// the most any has.
pub fn free(base: usize) {
    if !(REGION..REGION_END).contains(&base) {
        return;
    }
    let flags = irq_save();
    unsafe {
        note(used(base, base + KSTACK_SIZE), KSTACK_SIZE);
        give_back(base, KSTACK_PAGES);
    }
    irq_restore(flags);
}

/// Unmap the first `pages` of the stack at `base` and free their frames,
/// every processor told first, and free its slot.
///
/// # Safety
/// Interrupts off; nothing is running on the stack.
unsafe fn give_back(base: usize, pages: usize) {
    unsafe {
        let mut frames = [0usize; KSTACK_PAGES];
        let mut n = 0;
        for i in 0..pages {
            if let Ok((frame, _)) = paging::unmap_page(paging::kernel_cr3(), base + i * PAGE) {
                frames[n] = frame;
                n += 1;
            }
        }
        // Every address space has the region, and so may every processor
        // still have the pages in its cache.
        crate::tlb::stale_everywhere();
        for &frame in &frames[..n] {
            pmm::free(pmm::PhysFrame::from_address(frame));
        }
        let slot = (base - PAGE - REGION) / SLOT;
        (*(&raw mut USED))[slot / 64] &= !(1 << (slot % 64));
    }
}

/// Paint `[from, to)`.
///
/// # Safety
/// It is mapped, and nothing is on it.
unsafe fn paint(from: usize, to: usize) {
    let mut at = from;
    while at < to {
        unsafe { (at as *mut u64).write_volatile(PAINT) };
        at += 8;
    }
}

/// How much of the stack `[base, top)` has ever been used: from its top to
/// the lowest word that is not the paint.
pub fn used(base: usize, top: usize) -> usize {
    let mut at = base;
    while at < top && unsafe { (at as *const u64).read_volatile() } == PAINT {
        at += 8;
    }
    top - at
}

/// `used` bytes of a stack of `size` were used: kept, and said, if it is the
/// most any has.
fn note(used: usize, size: usize) {
    if used > DEEPEST.fetch_max(used, Ordering::Relaxed) {
        say(used, size);
    }
}

fn say(used: usize, size: usize) {
    crate::serial::puts(b"[kstack] deepest ");
    crate::serial::put_usize(used);
    crate::serial::puts(b" of ");
    crate::serial::put_usize(size);
    crate::serial::puts(b" bytes\n");
}

/// The deepest any kernel stack has been used, in bytes.
pub fn deepest() -> usize {
    DEEPEST.load(Ordering::Relaxed)
}

/// At shutdown: how deep the stacks still in use have gone counted too —
/// the servers' are never given back — and the deepest said.
pub fn say_deepest() {
    let flags = irq_save();
    unsafe {
        let used_slots = &*(&raw const USED);
        for slot in 0..SLOTS {
            if used_slots[slot / 64] & (1 << (slot % 64)) != 0 {
                let base = REGION + slot * SLOT + PAGE;
                note(used(base, base + KSTACK_SIZE), KSTACK_SIZE);
            }
        }
        let (bottom, top) = (&raw const boot_stack_bottom as usize, &raw const boot_stack_top as usize);
        note(used(bottom, top), top - bottom);
    }
    irq_restore(flags);
    say(deepest(), KSTACK_SIZE);
}

/// Whether `addr` is in a page left unmapped below a kernel stack: a fault
/// there is a stack that has run out.
pub fn is_guard(addr: usize) -> bool {
    let boot = &raw const boot_stack_guard as usize;
    (boot..boot + PAGE).contains(&addr) || ((REGION..REGION_END).contains(&addr) && (addr - REGION) % SLOT < PAGE)
}
