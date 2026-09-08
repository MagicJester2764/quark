/// Global allocator for user-space tasks, backed by sys_mmap.
///
/// Uses a linked-list free-list allocator. When no free block is large enough,
/// grows the heap by requesting pages from the kernel via sys_mmap.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use crate::sync::Mutex;

/// Where the heap starts looking for space — above all existing user
/// mappings. A starting point, not a reservation: see [`AllocInner::grow`].
const HEAP_START: usize = 0x90_0000_0000;

/// One past the last address the heap will probe.
const HEAP_LIMIT: usize = 0x98_0000_0000;

/// How many occupied regions a single grow will step over before giving up.
/// Collisions come from a second allocator instance in the same program, so
/// the realistic count is one or two; the cap only stops a pathological loop
/// from making syscalls forever.
const MAX_PROBES: usize = 64;

const PAGE_SIZE: usize = 4096;

/// Three-word header stored just before every returned pointer.
/// [0] = a magic, so a write that lands here is noticed rather than silently
///       redirecting the next free into arbitrary memory
/// [1] = block base address (where the free block started)
/// [2] = block total size (entire block including header + padding)
///
/// The magic exists because the header sits immediately below the pointer
/// handed out, so an underflow of one allocation destroys the bookkeeping for
/// it and the damage only surfaces later, somewhere else, as a wild write.
const HEADER_MAGIC: usize = 0x5152_4B48_4452_0001; // "QRKHDR" + version
const HEADER_WORDS: usize = 3;
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
    ///
    /// `heap_top` is where to *look* next, not memory this allocator holds. A
    /// program can contain more than one instance of this allocator, each with
    /// its own `heap_top` starting at `HEAP_START`: the hosted target links
    /// quark-rt twice, once inside std and once for the program itself, and
    /// std allocates a thread's control block through `System` rather than
    /// through the global allocator — so spawning a thread is enough to bring
    /// the second instance to life. Both would then hand out the same
    /// addresses.
    ///
    /// The kernel refuses to map over a live mapping, so a collision surfaces
    /// as a failed mmap rather than as one allocator's pages being replaced by
    /// another's. Step past the occupied region and ask again.
    fn grow(&mut self, min_bytes: usize) -> bool {
        let pages = (min_bytes + PAGE_SIZE - 1) / PAGE_SIZE;
        let size = pages * PAGE_SIZE;

        let mut vaddr = self.heap_top;
        let mut probes = 0;
        while crate::syscall::sys_mmap(vaddr, pages).is_err() {
            probes += 1;
            // Every step advances by at least a page, so this terminates.
            match vaddr.checked_add(size) {
                Some(next) if probes <= MAX_PROBES && next + size <= HEAP_LIMIT => vaddr = next,
                _ => return false,
            }
        }

        self.heap_top = vaddr + size;

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

        // A base outside the heap, or a size that runs past its top, means the
        // header survived but holds nonsense. Freeing on that basis puts a
        // pointer to arbitrary memory into the free list.
        if block_base < HEAP_START
            || block_base >= self.heap_top
            || block_size < MIN_BLOCK_SIZE
            || block_base + block_size > self.heap_top
        {
            crate::syscall::sys_write(b"\nheap: implausible header on free of 0x");
            write_hex(data_addr);
            crate::syscall::sys_write(b"\n      base 0x");
            write_hex(block_base);
            crate::syscall::sys_write(b" size 0x");
            write_hex(block_size);
            crate::syscall::sys_write(b"\n");
            crate::syscall::sys_exit_code(101);
        }

        let block = block_base as *mut FreeBlock;
        unsafe {
            (*block).size = block_size;
            (*block).next = ptr::null_mut();
        }
        self.insert_free(block);
    }
}

/// Write the header just before `data_start`.
fn write_header(data_start: usize, block_base: usize, block_size: usize) {
    unsafe {
        let header = (data_start - HEADER_SIZE) as *mut usize;
        *header = HEADER_MAGIC;
        *header.add(1) = block_base;
        *header.add(2) = block_size;
    }
}

/// Read the header just before `data_addr`, checking it first.
///
/// A wrong magic means something wrote below this allocation, and there is no
/// sensible value to return: reporting it here names the moment of use, where
/// the free list would otherwise be corrupted silently and fault somewhere
/// unrelated. So this either returns a good header or does not return.
fn read_header(data_addr: usize) -> (usize, usize) {
    unsafe {
        let header = (data_addr - HEADER_SIZE) as *const usize;
        if *header != HEADER_MAGIC {
            report_corruption(data_addr, *header);
        }
        (*header.add(1), *header.add(2))
    }
}

/// Say what happened, as loudly as possible, and stop.
///
/// Continuing past a corrupt header means threading a bad pointer into the
/// free list, and the eventual fault says nothing about the cause.
fn report_corruption(data_addr: usize, found: usize) -> ! {
    crate::syscall::sys_write(b"\nheap: header corrupted below allocation 0x");
    write_hex(data_addr);
    crate::syscall::sys_write(b"\n      expected magic, found 0x");
    write_hex(found);
    crate::syscall::sys_write(b"\n      something wrote to the 24 bytes below that pointer\n");
    crate::syscall::sys_exit_code(101);
}

fn write_hex(mut v: usize) {
    let mut buf = [b'0'; 16];
    for i in (0..16).rev() {
        buf[i] = b"0123456789abcdef"[v & 0xF];
        v >>= 4;
    }
    crate::syscall::sys_write(&buf);
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

/// Standalone allocator instance for use by the std PAL's GlobalAlloc impl.
/// When building as part of std (rustc-dep-of-std), std's System allocator
/// delegates to this. For no_std programs, lib.rs sets up its own #[global_allocator].
pub static SYSTEM_ALLOC: QuarkAllocator = QuarkAllocator::new();
