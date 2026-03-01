/// FAT32 filesystem driver interface.
///
/// Loads the fat32.drv boot module and dispatches operations through its vtable.

use core::ptr;

use crate::modules;

/// Directory entry returned by readdir.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirEntry {
    pub name: [u8; 11],
    pub attr: u8,
    pub size: u32,
    pub cluster: u32,
}

/// Driver vtable — filled by the driver's entry function. Must match the driver's layout.
#[repr(C)]
#[derive(Clone, Copy)]
struct Fat32Vtable {
    mount: extern "C" fn(*const u8, usize) -> i32,
    open: extern "C" fn(*const u8, usize) -> i32,
    read: extern "C" fn(i32, *mut u8, usize) -> i32,
    stat: extern "C" fn(i32) -> i32,
    close: extern "C" fn(i32) -> i32,
    readdir: extern "C" fn(i32, *mut DirEntry) -> i32,
}

static mut VTABLE: Option<Fat32Vtable> = None;

/// Initialize the FAT32 driver from a boot module.
///
/// # Safety
/// `module_start` must point to a valid FAT32 driver flat binary.
unsafe fn init_from_driver(module_start: usize) {
    type EntryFn = unsafe extern "C" fn(*mut Fat32Vtable);
    let entry: EntryFn = core::mem::transmute(module_start);

    let mut vtable = core::mem::MaybeUninit::<Fat32Vtable>::uninit();
    entry(vtable.as_mut_ptr());

    let vt_ptr = &raw mut VTABLE;
    ptr::write(vt_ptr, Some(vtable.assume_init()));
}

/// Try to load the FAT32 driver from boot modules.
pub fn init() {
    let m = modules::find(b"FAT32.DRV")
        .or_else(|| modules::find(b"fat32.drv"));

    if let Some(module) = m {
        unsafe { init_from_driver(module.start) };
    }
}

/// Returns true if the FAT32 driver was loaded.
pub fn is_loaded() -> bool {
    unsafe { ptr::read(&raw const VTABLE).is_some() }
}

/// Mount a FAT32 image at the given memory region.
/// Returns true on success.
pub fn mount(base: *const u8, size: usize) -> bool {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            (vt.mount)(base, size) == 0
        } else {
            false
        }
    }
}

/// Open a file by path. Returns a file descriptor or None.
pub fn open(path: &[u8]) -> Option<i32> {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            let fd = (vt.open)(path.as_ptr(), path.len());
            if fd >= 0 { Some(fd) } else { None }
        } else {
            None
        }
    }
}

/// Read from an open file into `buf`. Returns bytes read (0 = EOF).
pub fn read(fd: i32, buf: &mut [u8]) -> i32 {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            (vt.read)(fd, buf.as_mut_ptr(), buf.len())
        } else {
            -1
        }
    }
}

/// Get the size of an open file.
pub fn stat(fd: i32) -> Option<u32> {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            let s = (vt.stat)(fd);
            if s >= 0 { Some(s as u32) } else { None }
        } else {
            None
        }
    }
}

/// Close a file descriptor.
pub fn close(fd: i32) {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            (vt.close)(fd);
        }
    }
}

/// Read the next directory entry from an open directory.
/// Returns Some(entry) or None when no more entries.
pub fn readdir(fd: i32) -> Option<DirEntry> {
    unsafe {
        if let Some(vt) = ptr::read(&raw const VTABLE) {
            let mut entry = core::mem::MaybeUninit::<DirEntry>::uninit();
            let result = (vt.readdir)(fd, entry.as_mut_ptr());
            if result == 1 {
                Some(entry.assume_init())
            } else {
                None
            }
        } else {
            None
        }
    }
}

/// Format an 8.3 directory name into a human-readable form (e.g., "FILE    TXT" -> "FILE.TXT").
/// Writes into `out` and returns the number of bytes written.
pub fn format_83_name(raw: &[u8; 11], out: &mut [u8]) -> usize {
    let mut pos = 0;

    // Copy base name, trimming trailing spaces
    let mut base_end = 8;
    while base_end > 0 && raw[base_end - 1] == b' ' {
        base_end -= 1;
    }
    for i in 0..base_end {
        if pos < out.len() {
            out[pos] = raw[i];
            pos += 1;
        }
    }

    // Copy extension, trimming trailing spaces
    let mut ext_end = 11;
    while ext_end > 8 && raw[ext_end - 1] == b' ' {
        ext_end -= 1;
    }
    if ext_end > 8 {
        if pos < out.len() {
            out[pos] = b'.';
            pos += 1;
        }
        for i in 8..ext_end {
            if pos < out.len() {
                out[pos] = raw[i];
                pos += 1;
            }
        }
    }

    pos
}
