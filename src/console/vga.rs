/// VGA text mode console backed by a loadable driver module.
///
/// The kernel finds the `vga.drv` boot module, calls its entry function to obtain
/// a vtable of console operations, and dispatches all VGA calls through it.

use core::ptr;

use crate::services::{KernelServices, SERVICES};

/// Driver vtable — filled by the driver's entry function. Must match the driver's layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConsoleVtable {
    clear: extern "C" fn(),
    putc: extern "C" fn(u8),
    puts: extern "C" fn(*const u8, usize),
}

static mut VTABLE: Option<ConsoleVtable> = None;

/// Initialize VGA console from a loaded driver module.
/// `module_start` is the physical address of the flat binary (entry at offset 0).
///
/// # Safety
/// `module_start` must point to a valid VGA driver flat binary.
pub unsafe fn init_from_driver(module_start: usize) {
    type EntryFn = unsafe extern "C" fn(*mut ConsoleVtable, *const KernelServices);
    let entry: EntryFn = core::mem::transmute(module_start);

    let mut vtable = core::mem::MaybeUninit::<ConsoleVtable>::uninit();
    entry(vtable.as_mut_ptr(), &SERVICES);

    let vt_ptr = &raw mut VTABLE;
    ptr::write(vt_ptr, Some(vtable.assume_init()));
}

/// Returns true if the VGA driver was loaded successfully.
pub fn is_loaded() -> bool {
    unsafe { ptr::read(&raw const VTABLE).is_some() }
}

pub fn clear() {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            (vt.clear)();
        }
    }
}

pub fn puts(s: &[u8]) {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            (vt.puts)(s.as_ptr(), s.len());
        }
    }
}
