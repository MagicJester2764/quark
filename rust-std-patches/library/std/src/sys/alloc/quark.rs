use crate::alloc::{GlobalAlloc, Layout, System};

unsafe impl GlobalAlloc for System {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Delegate to quark-rt's allocator.
        // The quark-rt allocator uses sys_mmap for heap growth.
        unsafe { quark_rt::allocator::SYSTEM_ALLOC.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { quark_rt::allocator::SYSTEM_ALLOC.dealloc(ptr, layout) }
    }
}
