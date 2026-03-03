/// Kernel heap allocator — linked-list free-list with sorted-address coalescing.
///
/// Heap lives at virtual address 0x1_0000_0000 (above the boot identity-mapped
/// 4 GiB), so `paging::map_page` can allocate fresh intermediate tables without
/// hitting any huge-page conflicts from boot.s.
///
/// Uses an interrupt-safe spin lock (cli/sti around critical sections) to
/// prevent deadlock if an IRQ handler ever triggers allocation.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::paging;
use crate::pmm;

const PAGE_SIZE: usize = 4096;
const HEAP_START: usize = 0x1_0000_0000;
const INIT_PAGES: usize = 16; // 64 KiB initial heap
const GROW_PAGES: usize = 16; // 64 KiB growth increment
const HEADER_SIZE: usize = core::mem::size_of::<AllocHeader>(); // 16
const MIN_BLOCK_SIZE: usize = core::mem::size_of::<FreeBlock>(); // 16

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Each free region in the heap is a node in an address-sorted linked list.
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

/// Placed immediately before each user allocation so `dealloc` can recover the
/// original block boundaries regardless of alignment padding.
#[repr(C)]
struct AllocHeader {
    block_start: usize,
    block_size: usize,
}

// ---------------------------------------------------------------------------
// Interrupt-safe spin lock
// ---------------------------------------------------------------------------

struct SpinLock {
    locked: AtomicBool,
}

impl SpinLock {
    const fn new() -> Self {
        SpinLock {
            locked: AtomicBool::new(false),
        }
    }

    /// Acquire the lock. Saves RFLAGS and disables interrupts before spinning.
    /// Returns the saved RFLAGS value (caller must pass it to `unlock`).
    fn lock(&self) -> u64 {
        let saved: u64;
        unsafe {
            core::arch::asm!("pushfq; pop {}; cli", out(reg) saved, options(nostack));
        }
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        saved
    }

    /// Release the lock and restore the interrupt flag from saved RFLAGS.
    unsafe fn unlock(&self, saved: u64) {
        self.locked.store(false, Ordering::Release);
        // Restore IF (bit 9) only — push saved flags then popf.
        if saved & (1 << 9) != 0 {
            core::arch::asm!("sti", options(nostack, nomem));
        }
    }
}

// ---------------------------------------------------------------------------
// Heap inner state
// ---------------------------------------------------------------------------

struct HeapInner {
    free_list_head: *mut FreeBlock,
    heap_end: usize,
    total_size: usize,
    initialized: bool,
}

// ---------------------------------------------------------------------------
// LockedHeap — the #[global_allocator]
// ---------------------------------------------------------------------------

pub struct LockedHeap {
    lock: SpinLock,
    inner: UnsafeCell<HeapInner>,
}

unsafe impl Send for LockedHeap {}
unsafe impl Sync for LockedHeap {}

impl LockedHeap {
    const fn new() -> Self {
        LockedHeap {
            lock: SpinLock::new(),
            inner: UnsafeCell::new(HeapInner {
                free_list_head: ptr::null_mut(),
                heap_end: 0,
                total_size: 0,
                initialized: false,
            }),
        }
    }
}

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::new();

unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let saved = self.lock.lock();
        let inner = &mut *self.inner.get();
        let result = alloc_inner(inner, layout);
        self.lock.unlock(saved);
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let saved = self.lock.lock();
        let inner = &mut *self.inner.get();
        dealloc_inner(inner, ptr);
        self.lock.unlock(saved);
    }
}

// ---------------------------------------------------------------------------
// alloc / dealloc / grow implementations
// ---------------------------------------------------------------------------

const fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

/// First-fit allocation with alignment handling.
unsafe fn alloc_inner(heap: &mut HeapInner, layout: Layout) -> *mut u8 {
    let user_size = layout.size().max(MIN_BLOCK_SIZE);
    let align = layout.align().max(8);

    // Try allocation, grow once if it fails.
    for attempt in 0..2 {
        let mut prev: *mut FreeBlock = ptr::null_mut();
        let mut current = heap.free_list_head;

        while !current.is_null() {
            let block_addr = current as usize;
            let block_size = (*current).size;

            // Where user data would start, after the header and aligned.
            let aligned_data = align_up(block_addr + HEADER_SIZE, align);
            let total_needed = (aligned_data - block_addr) + user_size;

            if block_size >= total_needed {
                let remainder = block_size - total_needed;

                // Unlink this block from the free list.
                let next = (*current).next;

                if remainder >= MIN_BLOCK_SIZE {
                    // Split: create a new free block for the remainder.
                    let split_addr = (block_addr + total_needed) as *mut FreeBlock;
                    (*split_addr).size = remainder;
                    (*split_addr).next = next;

                    if prev.is_null() {
                        heap.free_list_head = split_addr;
                    } else {
                        (*prev).next = split_addr;
                    }
                } else {
                    // Use the entire block (no split).
                    if prev.is_null() {
                        heap.free_list_head = next;
                    } else {
                        (*prev).next = next;
                    }
                    // Adjust: user gets the full block.
                    // total_needed = block_size when no split (use actual block_size).
                }

                let actual_block_size = if remainder >= MIN_BLOCK_SIZE {
                    total_needed
                } else {
                    block_size
                };

                // Write the allocation header just before the user pointer.
                let header = (aligned_data - HEADER_SIZE) as *mut AllocHeader;
                (*header).block_start = block_addr;
                (*header).block_size = actual_block_size;

                return aligned_data as *mut u8;
            }

            prev = current;
            current = (*current).next;
        }

        // First attempt failed — try to grow the heap.
        if attempt == 0 {
            let needed = HEADER_SIZE + align + user_size;
            grow(heap, needed);
        }
    }

    ptr::null_mut()
}

/// Return a block to the free list with sorted-address insertion and coalescing.
unsafe fn dealloc_inner(heap: &mut HeapInner, ptr: *mut u8) {
    let header = (ptr as usize - HEADER_SIZE) as *const AllocHeader;
    let block_start = (*header).block_start;
    let block_size = (*header).block_size;

    let new_block = block_start as *mut FreeBlock;
    (*new_block).size = block_size;
    (*new_block).next = ptr::null_mut();

    // Find insertion point (sorted by address).
    let mut prev: *mut FreeBlock = ptr::null_mut();
    let mut current = heap.free_list_head;

    while !current.is_null() && (current as usize) < block_start {
        prev = current;
        current = (*current).next;
    }

    // Insert between prev and current.
    (*new_block).next = current;
    if prev.is_null() {
        heap.free_list_head = new_block;
    } else {
        (*prev).next = new_block;
    }

    // Coalesce with next neighbor.
    if !current.is_null() {
        let new_end = block_start + (*new_block).size;
        if new_end == current as usize {
            (*new_block).size += (*current).size;
            (*new_block).next = (*current).next;
        }
    }

    // Coalesce with previous neighbor.
    if !prev.is_null() {
        let prev_end = prev as usize + (*prev).size;
        if prev_end == new_block as usize {
            (*prev).size += (*new_block).size;
            (*prev).next = (*new_block).next;
        }
    }
}

/// Grow the heap by mapping new pages at `heap_end`.
unsafe fn grow(heap: &mut HeapInner, min_bytes: usize) {
    let min_pages = (min_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
    let pages = min_pages.max(GROW_PAGES);

    let pml4 = paging::read_cr3();

    for i in 0..pages {
        let frame = match pmm::alloc() {
            Some(f) => f,
            None => return, // Out of physical memory.
        };
        let vaddr = heap.heap_end + i * PAGE_SIZE;
        if paging::map_page(
            pml4,
            vaddr,
            frame.address(),
            paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE,
        )
        .is_err()
        {
            // Mapping failed — free the frame and stop.
            pmm::free(frame);
            return;
        }
        // Zero the new page.
        ptr::write_bytes(vaddr as *mut u8, 0, PAGE_SIZE);
    }

    let new_region_start = heap.heap_end;
    let new_region_size = pages * PAGE_SIZE;
    heap.heap_end += new_region_size;
    heap.total_size += new_region_size;

    // Add the new region as a free block and coalesce with the last free block
    // if it's adjacent.
    let new_block = new_region_start as *mut FreeBlock;
    (*new_block).size = new_region_size;
    (*new_block).next = ptr::null_mut();

    // Walk to find the insertion point (end of list, since grow extends at the top).
    let mut prev: *mut FreeBlock = ptr::null_mut();
    let mut current = heap.free_list_head;
    while !current.is_null() && (current as usize) < new_region_start {
        prev = current;
        current = (*current).next;
    }

    (*new_block).next = current;
    if prev.is_null() {
        heap.free_list_head = new_block;
    } else {
        // Try to coalesce with predecessor.
        let prev_end = prev as usize + (*prev).size;
        if prev_end == new_region_start {
            (*prev).size += new_region_size;
            (*prev).next = current;
            return;
        }
        (*prev).next = new_block;
    }
}

// ---------------------------------------------------------------------------
// Public init
// ---------------------------------------------------------------------------

/// Initialize the kernel heap. Maps `INIT_PAGES` pages at `HEAP_START` and
/// sets up the free list.
///
/// # Safety
/// Must be called exactly once, after PMM and paging are available.
pub unsafe fn init() {
    let inner = &mut *ALLOCATOR.inner.get();
    if inner.initialized {
        return;
    }

    let pml4 = paging::read_cr3();

    for i in 0..INIT_PAGES {
        let frame = pmm::alloc().expect("heap init: out of frames");
        let vaddr = HEAP_START + i * PAGE_SIZE;
        paging::map_page(
            pml4,
            vaddr,
            frame.address(),
            paging::PRESENT | paging::WRITABLE | paging::NO_EXECUTE,
        )
        .expect("heap init: map_page failed");
        ptr::write_bytes(vaddr as *mut u8, 0, PAGE_SIZE);
    }

    let total = INIT_PAGES * PAGE_SIZE;

    // Create a single free block spanning the entire initial region.
    let first_block = HEAP_START as *mut FreeBlock;
    (*first_block).size = total;
    (*first_block).next = ptr::null_mut();

    inner.free_list_head = first_block;
    inner.heap_end = HEAP_START + total;
    inner.total_size = total;
    inner.initialized = true;
}

// ---------------------------------------------------------------------------
// OOM handler
// ---------------------------------------------------------------------------

#[alloc_error_handler]
fn alloc_error(layout: Layout) -> ! {
    let _ = layout;
    panic!("heap allocation failed");
}
