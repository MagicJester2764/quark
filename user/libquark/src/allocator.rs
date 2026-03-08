/// Global allocator for user-space tasks, backed by sys_mmap.
///
/// Uses a linked-list free-list allocator. When no free block is large enough,
/// grows the heap by requesting pages from the kernel via sys_mmap.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::sync::Mutex;

/// Heap starts at 0x90_0000_0000 — above all existing user mappings.
const HEAP_START: usize = 0x90_0000_0000;
const PAGE_SIZE: usize = 4096;

/// Two-word header stored just before every returned pointer.
/// [0] = block base address (where the free block started)
/// [1] = block total size (entire block including header + padding)
const HEADER_WORDS: usize = 2;
const HEADER_SIZE: usize = HEADER_WORDS * core::mem::size_of::<usize>();

/// Minimum block size (must fit a FreeBlock header).
const MIN_BLOCK_SIZE: usize = core::mem::size_of::<FreeBlock>();

/// Header stored at the start of each free block in the free list.
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

struct AllocInner {
    free_head: *mut FreeBlock,
    heap_top: usize,
}

unsafe impl Send for AllocInner {}

impl AllocInner {
    const fn new() -> Self {
        AllocInner {
            free_head: ptr::null_mut(),
            heap_top: HEAP_START,
        }
    }

    /// Grow the heap by at least `min_bytes`, mapping new pages via sys_mmap.
    fn grow(&mut self, min_bytes: usize) -> bool {
        let pages = (min_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
        let vaddr = self.heap_top;
        if crate::syscall::sys_mmap(vaddr, pages).is_err() {
            return false;
        }
        let size = pages * PAGE_SIZE;
        self.heap_top += size;

        let block = vaddr as *mut FreeBlock;
        unsafe {
            (*block).size = size;
            (*block).next = ptr::null_mut();
        }
        self.insert_free(block);
        true
    }

    /// Insert a block into the free list (sorted by address) and coalesce.
    fn insert_free(&mut self, block: *mut FreeBlock) {
        let addr = block as usize;

        let mut prev: *mut FreeBlock = ptr::null_mut();
        let mut curr = self.free_head;
        while !curr.is_null() && (curr as usize) < addr {
            prev = curr;
            curr = unsafe { (*curr).next };
        }

        unsafe { (*block).next = curr };
        if prev.is_null() {
            self.free_head = block;
        } else {
            unsafe { (*prev).next = block };
        }

        // Coalesce with next
        unsafe {
            if !curr.is_null() && addr + (*block).size == curr as usize {
                (*block).size += (*curr).size;
                (*block).next = (*curr).next;
            }
        }

        // Coalesce with prev
        if !prev.is_null() {
            unsafe {
                if prev as usize + (*prev).size == addr {
                    (*prev).size += (*block).size;
                    (*prev).next = (*block).next;
                }
            }
        }
    }

    fn alloc_inner(&mut self, size: usize, align: usize) -> *mut u8 {
        let alloc_align = align.max(core::mem::align_of::<usize>());

        // Try free list (first fit)
        let mut prev: *mut FreeBlock = ptr::null_mut();
        let mut curr = self.free_head;
        while !curr.is_null() {
            let block_addr = curr as usize;
            let block_size = unsafe { (*curr).size };

            // Data pointer must be aligned, with HEADER_SIZE bytes before it
            let data_start = align_up(block_addr + HEADER_SIZE, alloc_align);
            let total_needed = (data_start - block_addr) + size;

            if block_size >= total_needed {
                let remainder = block_size - total_needed;

                if remainder >= MIN_BLOCK_SIZE {
                    // Split: remainder becomes a new free block
                    let new_block = (block_addr + total_needed) as *mut FreeBlock;
                    unsafe {
                        (*new_block).size = remainder;
                        (*new_block).next = (*curr).next;
                    }
                    if prev.is_null() {
                        self.free_head = new_block;
                    } else {
                        unsafe { (*prev).next = new_block };
                    }
                    // Write header: [block_base, total_size]
                    write_header(data_start, block_addr, total_needed);
                } else {
                    // Use entire block (include remainder in the allocation)
                    if prev.is_null() {
                        self.free_head = unsafe { (*curr).next };
                    } else {
                        unsafe { (*prev).next = (*curr).next };
                    }
                    write_header(data_start, block_addr, block_size);
                }

                return data_start as *mut u8;
            }

            prev = curr;
            curr = unsafe { (*curr).next };
        }

        // No suitable block — grow
        let needed = HEADER_SIZE + alloc_align + size;
        if !self.grow(needed) {
            return ptr::null_mut();
        }

        // Retry (new region is now in the free list)
        self.alloc_inner(size, align)
    }

    fn dealloc_inner(&mut self, ptr: *mut u8) {
        let data_addr = ptr as usize;
        let (block_base, block_size) = read_header(data_addr);

        let block = block_base as *mut FreeBlock;
        unsafe {
            (*block).size = block_size;
            (*block).next = ptr::null_mut();
        }
        self.insert_free(block);
    }
}

/// Write the 2-word header just before `data_start`.
fn write_header(data_start: usize, block_base: usize, block_size: usize) {
    unsafe {
        let header = (data_start - HEADER_SIZE) as *mut usize;
        *header = block_base;
        *header.add(1) = block_size;
    }
}

/// Read the 2-word header just before `data_addr`.
fn read_header(data_addr: usize) -> (usize, usize) {
    unsafe {
        let header = (data_addr - HEADER_SIZE) as *const usize;
        (*header, *header.add(1))
    }
}

fn align_up(addr: usize, align: usize) -> usize {
    (addr + align - 1) & !(align - 1)
}

pub struct QuarkAllocator {
    inner: Mutex<AllocInner>,
}

unsafe impl Sync for QuarkAllocator {}

impl QuarkAllocator {
    pub const fn new() -> Self {
        QuarkAllocator {
            inner: Mutex::new(AllocInner::new()),
        }
    }
}

unsafe impl GlobalAlloc for QuarkAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut inner = self.inner.lock();
        inner.alloc_inner(layout.size(), layout.align())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        let mut inner = self.inner.lock();
        inner.dealloc_inner(ptr);
    }
}
