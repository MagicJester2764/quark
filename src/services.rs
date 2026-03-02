/// Kernel services exposed to loadable drivers via function pointers.
///
/// Drivers are flat binaries with no access to kernel symbols. The kernel passes
/// a pointer to this struct at driver init time so drivers can allocate and free
/// physical page frames.

use crate::pmm;

/// Function pointer table passed to drivers at init.
/// Must be `#[repr(C)]` so drivers compiled separately can read it.
#[repr(C)]
pub struct KernelServices {
    /// Allocate a 4 KiB physical page frame.
    /// Returns the physical address, or 0 on failure.
    pub alloc_page: extern "C" fn() -> usize,

    /// Free a previously allocated 4 KiB physical page frame.
    pub free_page: extern "C" fn(addr: usize),
}

extern "C" fn svc_alloc_page() -> usize {
    match pmm::alloc() {
        Some(frame) => frame.address(),
        None => 0,
    }
}

extern "C" fn svc_free_page(addr: usize) {
    pmm::free(pmm::PhysFrame::from_address(addr));
}

/// Global instance passed to all drivers.
pub static SERVICES: KernelServices = KernelServices {
    alloc_page: svc_alloc_page,
    free_page: svc_free_page,
};
