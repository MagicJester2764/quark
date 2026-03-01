//! FAT32 filesystem driver for the Quark microkernel.
//!
//! Compiled as a position-independent flat binary. The kernel loads this module
//! and calls `_entry` at offset 0 to obtain a vtable of filesystem operations.
//!
//! The driver operates on a contiguous memory region (ramdisk image passed via
//! `mount`). It supports reading files and listing directories using 8.3 short
//! filenames.

#![no_std]
#![no_main]
#![allow(dead_code)]

use core::panic::PanicInfo;
use core::ptr;

// --- FAT32 on-disk structures ---

const SECTOR_SIZE: usize = 512;
const DIR_ENTRY_SIZE: usize = 32;
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

const FAT32_EOC: u32 = 0x0FFF_FFF8; // end-of-chain marker (>= this value)
const FAT32_MASK: u32 = 0x0FFF_FFFF;
const MAX_OPEN_FILES: usize = 16;

/// BPB (BIOS Parameter Block) fields we care about.
struct Bpb {
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    total_sectors_32: u32,
    fat_size_32: u32,
    root_cluster: u32,
}

/// Mounted filesystem state.
struct MountState {
    base: *const u8,
    size: usize,
    bpb: Bpb,
    fat_offset: usize,
    data_offset: usize,
    cluster_size: usize,
}

/// An open file handle.
#[derive(Clone, Copy)]
struct FileHandle {
    in_use: bool,
    start_cluster: u32,
    file_size: u32,
    cur_cluster: u32,
    cur_offset: u32,
    is_dir: bool,
}

impl FileHandle {
    const fn empty() -> Self {
        FileHandle {
            in_use: false,
            start_cluster: 0,
            file_size: 0,
            cur_cluster: 0,
            cur_offset: 0,
            is_dir: false,
        }
    }
}

/// A directory entry returned by list_dir.
#[repr(C)]
pub struct DirEntry {
    pub name: [u8; 11],
    pub attr: u8,
    pub size: u32,
    pub cluster: u32,
}

/// Vtable returned to the kernel.
#[repr(C)]
pub struct Fat32Vtable {
    /// Mount a FAT32 image. Returns 0 on success, -1 on error.
    /// `base` = pointer to image, `size` = image size in bytes.
    pub mount: extern "C" fn(base: *const u8, size: usize) -> i32,

    /// Open a file by absolute path (e.g., "/DRIVERS/TEST.BIN").
    /// Returns a file descriptor (0..MAX_OPEN_FILES-1) or -1 on error.
    pub open: extern "C" fn(path: *const u8, path_len: usize) -> i32,

    /// Read up to `count` bytes from fd into `buf`. Returns bytes read, 0 on EOF, -1 on error.
    pub read: extern "C" fn(fd: i32, buf: *mut u8, count: usize) -> i32,

    /// Get file size for an open fd. Returns size or -1 on error.
    pub stat: extern "C" fn(fd: i32) -> i32,

    /// Close a file descriptor.
    pub close: extern "C" fn(fd: i32) -> i32,

    /// List directory entries. `fd` must refer to an open directory.
    /// Writes one DirEntry to `out` and returns 1, or 0 when no more entries, -1 on error.
    pub readdir: extern "C" fn(fd: i32, out: *mut DirEntry) -> i32,
}

// --- Global state ---

static mut MOUNTED: bool = false;
static mut STATE: MountState = MountState {
    base: ptr::null(),
    size: 0,
    bpb: Bpb {
        bytes_per_sector: 0,
        sectors_per_cluster: 0,
        reserved_sectors: 0,
        num_fats: 0,
        total_sectors_32: 0,
        fat_size_32: 0,
        root_cluster: 0,
    },
    fat_offset: 0,
    data_offset: 0,
    cluster_size: 0,
};

static mut FILES: [FileHandle; MAX_OPEN_FILES] = [FileHandle::empty(); MAX_OPEN_FILES];

// --- Driver entry point ---

#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _entry(out: *mut Fat32Vtable) {
    unsafe {
        MOUNTED = false;
        FILES = [FileHandle::empty(); MAX_OPEN_FILES];

        (*out).mount = fat32_mount;
        (*out).open = fat32_open;
        (*out).read = fat32_read;
        (*out).stat = fat32_stat;
        (*out).close = fat32_close;
        (*out).readdir = fat32_readdir;
    }
}

// --- Helpers ---

unsafe fn read_u16(base: *const u8, offset: usize) -> u16 {
    (base.add(offset) as *const u16).read_unaligned()
}

unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
    (base.add(offset) as *const u32).read_unaligned()
}

/// Get the byte offset of a cluster's data region.
unsafe fn cluster_offset(cluster: u32) -> usize {
    STATE.data_offset + (cluster as usize - 2) * STATE.cluster_size
}

/// Read the next cluster number from the FAT.
unsafe fn fat_next(cluster: u32) -> u32 {
    let offset = STATE.fat_offset + cluster as usize * 4;
    if offset + 4 > STATE.size {
        return FAT32_EOC;
    }
    read_u32(STATE.base, offset) & FAT32_MASK
}

/// Check if a cluster number indicates end-of-chain.
fn is_eoc(cluster: u32) -> bool {
    cluster >= FAT32_EOC
}

/// Allocate a file descriptor.
unsafe fn alloc_fd() -> i32 {
    for i in 0..MAX_OPEN_FILES {
        if !FILES[i].in_use {
            return i as i32;
        }
    }
    -1
}

/// Compare an 8.3 name (11 bytes, space-padded) against a filename component.
/// `component` is a normal filename like "FILE.TXT" (no padding).
fn match_83_name(dir_name: &[u8; 11], component: &[u8]) -> bool {
    // Build the expected 8.3 name from the component
    let mut expected = [b' '; 11];

    // Find the dot
    let mut dot_pos = component.len();
    for i in 0..component.len() {
        if component[i] == b'.' {
            dot_pos = i;
            break;
        }
    }

    // Copy base name (up to 8 chars)
    let base_len = if dot_pos > 8 { 8 } else { dot_pos };
    for i in 0..base_len {
        expected[i] = to_upper(component[i]);
    }

    // Copy extension (up to 3 chars)
    if dot_pos < component.len() {
        let ext_start = dot_pos + 1;
        let ext_len = component.len() - ext_start;
        let ext_copy = if ext_len > 3 { 3 } else { ext_len };
        for i in 0..ext_copy {
            expected[8 + i] = to_upper(component[ext_start + i]);
        }
    }

    *dir_name == expected
}

fn to_upper(c: u8) -> u8 {
    if c >= b'a' && c <= b'z' {
        c - 32
    } else {
        c
    }
}

// --- Vtable functions ---

extern "C" fn fat32_mount(base: *const u8, size: usize) -> i32 {
    if base.is_null() || size < SECTOR_SIZE {
        return -1;
    }

    unsafe {
        // Check FAT32 signature: 0x55AA at offset 510
        if *base.add(510) != 0x55 || *base.add(511) != 0xAA {
            return -1;
        }

        let bytes_per_sector = read_u16(base, 11);
        let sectors_per_cluster = *base.add(13);
        let reserved_sectors = read_u16(base, 14);
        let num_fats = *base.add(16);

        // root_entry_count at offset 17 must be 0 for FAT32
        let root_entry_count = read_u16(base, 17);
        if root_entry_count != 0 {
            return -1; // Not FAT32
        }

        let total_sectors_32 = read_u32(base, 32);
        let fat_size_32 = read_u32(base, 36);
        let root_cluster = read_u32(base, 44);

        let fat_offset = reserved_sectors as usize * bytes_per_sector as usize;
        let data_offset = fat_offset + num_fats as usize * fat_size_32 as usize * bytes_per_sector as usize;
        let cluster_size = sectors_per_cluster as usize * bytes_per_sector as usize;

        STATE = MountState {
            base,
            size,
            bpb: Bpb {
                bytes_per_sector,
                sectors_per_cluster,
                reserved_sectors,
                num_fats,
                total_sectors_32,
                fat_size_32,
                root_cluster,
            },
            fat_offset,
            data_offset,
            cluster_size,
        };

        MOUNTED = true;
    }

    0
}

extern "C" fn fat32_open(path: *const u8, path_len: usize) -> i32 {
    if path.is_null() || path_len == 0 {
        return -1;
    }
    unsafe {
        if !MOUNTED {
            return -1;
        }
    }

    // Build a safe view of the path
    let path_slice = unsafe { core::slice::from_raw_parts(path, path_len) };

    // Start at root directory cluster
    let mut current_cluster = unsafe { STATE.bpb.root_cluster };
    let mut is_dir = true;
    let mut file_size: u32 = 0;

    // Skip leading '/'
    let mut pos = 0;
    while pos < path_slice.len() && path_slice[pos] == b'/' {
        pos += 1;
    }

    // If path is just "/", open root directory
    if pos >= path_slice.len() {
        let fd = unsafe { alloc_fd() };
        if fd < 0 {
            return -1;
        }
        unsafe {
            FILES[fd as usize] = FileHandle {
                in_use: true,
                start_cluster: current_cluster,
                file_size: 0,
                cur_cluster: current_cluster,
                cur_offset: 0,
                is_dir: true,
            };
        }
        return fd;
    }

    // Traverse path components
    while pos < path_slice.len() {
        if !is_dir {
            return -1; // Tried to traverse into a file
        }

        // Extract the next component
        let comp_start = pos;
        while pos < path_slice.len() && path_slice[pos] != b'/' {
            pos += 1;
        }
        let component = &path_slice[comp_start..pos];

        // Skip trailing slashes
        while pos < path_slice.len() && path_slice[pos] == b'/' {
            pos += 1;
        }

        // Search directory for this component
        let mut found = false;
        let mut search_cluster = current_cluster;

        'dir_search: loop {
            if is_eoc(search_cluster) || search_cluster < 2 {
                break;
            }

            let off = unsafe { cluster_offset(search_cluster) };
            let entries_per_cluster = unsafe { STATE.cluster_size / DIR_ENTRY_SIZE };

            for i in 0..entries_per_cluster {
                let entry_off = off + i * DIR_ENTRY_SIZE;
                let entry_ptr = unsafe { STATE.base.add(entry_off) };

                let first_byte = unsafe { *entry_ptr };
                if first_byte == 0x00 {
                    break 'dir_search; // No more entries
                }
                if first_byte == 0xE5 {
                    continue; // Deleted entry
                }

                let attr = unsafe { *entry_ptr.add(11) };
                if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                    continue; // Skip long name entries
                }
                if attr & ATTR_VOLUME_ID != 0 {
                    continue; // Skip volume label
                }

                let mut dir_name = [0u8; 11];
                unsafe {
                    ptr::copy_nonoverlapping(entry_ptr, dir_name.as_mut_ptr(), 11);
                }

                if match_83_name(&dir_name, component) {
                    let cluster_hi = unsafe { read_u16(STATE.base, entry_off + 20) } as u32;
                    let cluster_lo = unsafe { read_u16(STATE.base, entry_off + 26) } as u32;
                    current_cluster = (cluster_hi << 16) | cluster_lo;
                    file_size = unsafe { read_u32(STATE.base, entry_off + 28) };
                    is_dir = attr & ATTR_DIRECTORY != 0;
                    found = true;
                    break 'dir_search;
                }
            }

            search_cluster = unsafe { fat_next(search_cluster) };
        }

        if !found {
            return -1;
        }
    }

    let fd = unsafe { alloc_fd() };
    if fd < 0 {
        return -1;
    }

    unsafe {
        FILES[fd as usize] = FileHandle {
            in_use: true,
            start_cluster: current_cluster,
            file_size: if is_dir { 0 } else { file_size },
            cur_cluster: current_cluster,
            cur_offset: 0,
            is_dir,
        };
    }

    fd
}

extern "C" fn fat32_read(fd: i32, buf: *mut u8, count: usize) -> i32 {
    if fd < 0 || fd as usize >= MAX_OPEN_FILES || buf.is_null() {
        return -1;
    }

    unsafe {
        if !MOUNTED || !FILES[fd as usize].in_use || FILES[fd as usize].is_dir {
            return -1;
        }

        let file = &mut FILES[fd as usize];
        let remaining = file.file_size - file.cur_offset;
        if remaining == 0 {
            return 0; // EOF
        }

        let to_read = if count as u32 > remaining {
            remaining as usize
        } else {
            count
        };

        let cluster_size = STATE.cluster_size as u32;
        let mut bytes_read: usize = 0;

        while bytes_read < to_read {
            if is_eoc(file.cur_cluster) || file.cur_cluster < 2 {
                break;
            }

            let offset_in_cluster = file.cur_offset % cluster_size;
            let cluster_remaining = cluster_size - offset_in_cluster;
            let chunk = core::cmp::min(
                (to_read - bytes_read) as u32,
                cluster_remaining,
            ) as usize;

            let src = STATE.base.add(cluster_offset(file.cur_cluster) + offset_in_cluster as usize);
            ptr::copy_nonoverlapping(src, buf.add(bytes_read), chunk);

            bytes_read += chunk;
            file.cur_offset += chunk as u32;

            // Move to next cluster if we've consumed this one
            if file.cur_offset % cluster_size == 0 {
                file.cur_cluster = fat_next(file.cur_cluster);
            }
        }

        bytes_read as i32
    }
}

extern "C" fn fat32_stat(fd: i32) -> i32 {
    if fd < 0 || fd as usize >= MAX_OPEN_FILES {
        return -1;
    }
    unsafe {
        if !FILES[fd as usize].in_use {
            return -1;
        }
        FILES[fd as usize].file_size as i32
    }
}

extern "C" fn fat32_close(fd: i32) -> i32 {
    if fd < 0 || fd as usize >= MAX_OPEN_FILES {
        return -1;
    }
    unsafe {
        if !FILES[fd as usize].in_use {
            return -1;
        }
        FILES[fd as usize] = FileHandle::empty();
    }
    0
}

extern "C" fn fat32_readdir(fd: i32, out: *mut DirEntry) -> i32 {
    if fd < 0 || fd as usize >= MAX_OPEN_FILES || out.is_null() {
        return -1;
    }

    unsafe {
        if !MOUNTED || !FILES[fd as usize].in_use || !FILES[fd as usize].is_dir {
            return -1;
        }

        let file = &mut FILES[fd as usize];
        let cluster_size = STATE.cluster_size as u32;

        loop {
            if is_eoc(file.cur_cluster) || file.cur_cluster < 2 {
                return 0; // No more entries
            }

            let offset_in_cluster = file.cur_offset % cluster_size;
            let entry_off = cluster_offset(file.cur_cluster) + offset_in_cluster as usize;
            let entry_ptr = STATE.base.add(entry_off);

            // Advance cursor
            file.cur_offset += DIR_ENTRY_SIZE as u32;
            if file.cur_offset % cluster_size == 0 {
                file.cur_cluster = fat_next(file.cur_cluster);
            }

            let first_byte = *entry_ptr;
            if first_byte == 0x00 {
                return 0; // No more entries
            }
            if first_byte == 0xE5 {
                continue; // Deleted
            }

            let attr = *entry_ptr.add(11);
            if attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                continue; // Skip LFN entries
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue; // Skip volume label
            }

            let mut name = [0u8; 11];
            ptr::copy_nonoverlapping(entry_ptr, name.as_mut_ptr(), 11);

            let cluster_hi = read_u16(STATE.base, entry_off + 20) as u32;
            let cluster_lo = read_u16(STATE.base, entry_off + 26) as u32;
            let cluster = (cluster_hi << 16) | cluster_lo;
            let size = read_u32(STATE.base, entry_off + 28);

            ptr::write(
                out,
                DirEntry {
                    name,
                    attr,
                    size,
                    cluster,
                },
            );

            return 1;
        }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
