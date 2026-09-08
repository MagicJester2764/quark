#![no_std]
#![no_main]

use quark_rt::ipc::Message;
use quark_rt::nameserver;
use quark_rt::spawn::{self, Scratch, Spawned};
use quark_rt::{println, syscall, vfs};

const PAGE_SIZE: usize = 4096;
const BOOT_INFO_ADDR: usize = 0x80_4000_0000;
const FILE_BUF_BASE: usize = 0x82_0000_0000;
const BOOT_IMG_BASE: usize = 0x85_0000_0000;
const NAMESERVER_TID: usize = 2;

// ---------------------------------------------------------------------------
// Boot info structures (matches kernel's BootInfo)
// ---------------------------------------------------------------------------

#[repr(C)]
struct BootInfo {
    module_count: u64,
    fb_addr: u64,
    fb_pitch: u32,
    fb_width: u32,
    fb_height: u32,
    fb_bpp: u8,
    fb_type: u8,
    fb_red_pos: u8,
    fb_green_pos: u8,
    fb_blue_pos: u8,
    _pad: [u8; 3],
    modules: [BootModuleDesc; 32],
}

#[repr(C)]
struct BootModuleDesc {
    phys_start: u64,
    phys_end: u64,
    name: [u8; 48],
}

// ---------------------------------------------------------------------------
// ELF64 structures
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Minimal FAT32 reader (read-only, root directory only)
// ---------------------------------------------------------------------------

struct Bpb {
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    reserved_sectors: u32,
    num_fats: u32,
    fat_size_32: u32,
    root_cluster: u32,
}

fn parse_bpb(data: &[u8]) -> Bpb {
    Bpb {
        bytes_per_sector: read_u16(data, 11) as u32,
        sectors_per_cluster: data[13] as u32,
        reserved_sectors: read_u16(data, 14) as u32,
        num_fats: data[16] as u32,
        fat_size_32: read_u32(data, 36),
        root_cluster: read_u32(data, 44),
    }
}

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn fat_offset(bpb: &Bpb) -> usize {
    (bpb.reserved_sectors * bpb.bytes_per_sector) as usize
}

fn data_region_offset(bpb: &Bpb) -> usize {
    ((bpb.reserved_sectors + bpb.num_fats * bpb.fat_size_32) * bpb.bytes_per_sector) as usize
}

fn cluster_data_offset(bpb: &Bpb, cluster: u32) -> usize {
    data_region_offset(bpb)
        + ((cluster - 2) as usize)
            * (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize
}

fn fat_next(rootfs: &[u8], bpb: &Bpb, cluster: u32) -> Option<u32> {
    let off = fat_offset(bpb) + (cluster as usize) * 4;
    let next = read_u32(rootfs, off) & 0x0FFF_FFFF;
    if next >= 0x0FFF_FFF8 {
        None
    } else {
        Some(next)
    }
}

const MAX_DIR_ENTRIES: usize = 32;

struct RootDirEntry {
    name: [u8; 11],
    first_cluster: u32,
    file_size: u32,
}

fn scan_root_dir(rootfs: &[u8], bpb: &Bpb) -> ([RootDirEntry; MAX_DIR_ENTRIES], usize) {
    let mut entries: [RootDirEntry; MAX_DIR_ENTRIES] = unsafe { core::mem::zeroed() };
    let mut count = 0;
    let cluster_bytes = (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize;
    let mut cluster = bpb.root_cluster;

    loop {
        let base = cluster_data_offset(bpb, cluster);
        let num_entries = cluster_bytes / 32;

        for i in 0..num_entries {
            if count >= MAX_DIR_ENTRIES {
                return (entries, count);
            }

            let off = base + i * 32;
            let first_byte = rootfs[off];

            // End of directory
            if first_byte == 0x00 {
                return (entries, count);
            }
            // Deleted entry
            if first_byte == 0xE5 {
                continue;
            }

            let attr = rootfs[off + 11];
            if attr & 0x0F == 0x0F { continue; } // LFN
            if attr & 0x08 != 0 { continue; }     // volume label
            if attr & 0x10 != 0 { continue; }     // subdirectory

            let mut name = [0u8; 11];
            name.copy_from_slice(&rootfs[off..off + 11]);

            let cluster_hi = read_u16(rootfs, off + 20) as u32;
            let cluster_lo = read_u16(rootfs, off + 26) as u32;

            entries[count] = RootDirEntry {
                name,
                first_cluster: (cluster_hi << 16) | cluster_lo,
                file_size: read_u32(rootfs, off + 28),
            };
            count += 1;
        }

        match fat_next(rootfs, bpb, cluster) {
            Some(next) => cluster = next,
            None => break,
        }
    }

    (entries, count)
}

// ---------------------------------------------------------------------------
// File reading: assemble file data from cluster chain into contiguous buffer
// ---------------------------------------------------------------------------

fn read_file_to_buffer<'a>(
    rootfs: &[u8],
    bpb: &Bpb,
    first_cluster: u32,
    file_size: u32,
) -> Result<&'a [u8], ()> {
    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Allocate physical pages and map at FILE_BUF_BASE
    for p in 0..pages_needed {
        let frame = syscall::sys_phys_alloc(1)?;
        syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1)?;
    }

    let cluster_bytes = (bpb.sectors_per_cluster * bpb.bytes_per_sector) as usize;
    let mut cluster = first_cluster;
    let mut copied = 0usize;

    while copied < size {
        let src_off = cluster_data_offset(bpb, cluster);
        let chunk = cluster_bytes.min(size - copied);

        unsafe {
            core::ptr::copy_nonoverlapping(
                rootfs.as_ptr().add(src_off),
                (FILE_BUF_BASE + copied) as *mut u8,
                chunk,
            );
        }

        copied += chunk;

        if copied < size {
            match fat_next(rootfs, bpb, cluster) {
                Some(next) => cluster = next,
                None => break,
            }
        }
    }

    Ok(unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) })
}

// ---------------------------------------------------------------------------
// ELF spawning (takes pre-mapped byte slice)
// ---------------------------------------------------------------------------

/// Load an ELF into a new task but do NOT start it.
/// Call info.start() after wiring fds / granting caps.

// ---------------------------------------------------------------------------
// Program arguments
// ---------------------------------------------------------------------------

const ARGS_TEMP_PAGE: usize = 0x88_0000_0000;

/// Staging areas quark_rt::spawn maps through while building a child. init's
/// were written inline in its loader rather than named; 0x83 staged ELF pages
/// and 0x84 staged stacks.
const SPAWN_SCRATCH: Scratch = Scratch {
    elf: 0x83_0000_0000,
    stack: 0x84_0000_0000,
    args: ARGS_TEMP_PAGE,
};

/// Write program arguments into the child task's address space.
/// `args` is a list of byte slices (argv[0], argv[1], ...).
/// Allocates one physical page, writes the args layout, maps into child's CR3.

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn module_name(name: &[u8; 48]) -> &[u8] {
    let len = name.iter().position(|&b| b == 0).unwrap_or(48);
    &name[..len]
}

fn starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack[..needle.len()] == *needle
}

fn to_upper(b: u8) -> u8 {
    if b >= b'a' && b <= b'z' { b - 32 } else { b }
}

/// Match a DirEntry against base+ext (e.g., b"LOGIN", b"ELF").
/// Works with both FAT32 8.3 names and ext2 actual names.
fn name_matches_entry(entry: &vfs::DirEntry, base: &[u8], ext: &[u8]) -> bool {
    let name = entry.name_bytes();
    // Try matching "BASE.EXT" (ext2 format)
    let expected_len = base.len() + 1 + ext.len(); // "LOGIN.ELF"
    if name.len() == expected_len {
        let mut ok = true;
        for i in 0..base.len() {
            if to_upper(name[i]) != to_upper(base[i]) { ok = false; break; }
        }
        if ok && name[base.len()] == b'.' {
            for i in 0..ext.len() {
                if to_upper(name[base.len() + 1 + i]) != to_upper(ext[i]) { ok = false; break; }
            }
            if ok { return true; }
        }
    }
    // Try matching bare name without extension (ext2 lowercase format, e.g., "login")
    if name.len() == base.len() {
        let mut ok = true;
        for i in 0..base.len() {
            if to_upper(name[i]) != to_upper(base[i]) { ok = false; break; }
        }
        if ok { return true; }
    }
    // Try FAT32 8.3 format
    if name.len() >= 11 {
        let mut fat_ok = true;
        for i in 0..8 {
            let expected = if i < base.len() { to_upper(base[i]) } else { b' ' };
            if to_upper(name[i]) != expected { fat_ok = false; break; }
        }
        if fat_ok {
            for i in 0..3 {
                let expected = if i < ext.len() { to_upper(ext[i]) } else { b' ' };
                if to_upper(name[8 + i]) != expected { fat_ok = false; break; }
            }
            if fat_ok { return true; }
        }
    }
    false
}

fn fat_name_to_buf(name: &[u8; 11], buf: &mut [u8; 16]) -> usize {
    let base_len = name[0..8]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    let mut pos = 0;
    for i in 0..base_len {
        if pos < buf.len() {
            buf[pos] = name[i];
            pos += 1;
        }
    }
    let ext_len = name[8..11]
        .iter()
        .rposition(|&b| b != b' ')
        .map_or(0, |p| p + 1);
    if ext_len > 0 {
        if pos < buf.len() {
            buf[pos] = b'.';
            pos += 1;
        }
        for i in 0..ext_len {
            if pos < buf.len() {
                buf[pos] = name[8 + i];
                pos += 1;
            }
        }
    }
    pos
}

/// Check if a FAT 8.3 name is an essential boot service (loaded from boot image).
fn is_essential_elf(name: &[u8; 11]) -> bool {
    if &name[8..11] != b"ELF" { return false; }
    let base = &name[0..8];
    base == b"NAMESRVR" || base == b"CONSOLE " || base == b"KEYBOARD"
        || base == b"DISK    " || base == b"INPUT   " || base == b"VFS     "
        || base == b"NET     "
}

/// Set of TIDs that programs are allowed to originate IPC to.
///
/// Grows as system services are spawned. Everything init starts gets an
/// Endpoint capability carrying this mask, which is what lets a program reach
/// the nameserver and the services — and nothing else. Two user programs are
/// never in each other's mask, so they cannot talk to one another directly.
static mut SERVICE_MASK: u64 = (1 << INIT_TID) | (1 << NAMESERVER_TID);

/// init is always TID 1; NAMESERVER_TID is defined at the top of this file.
const INIT_TID: usize = 1;

/// Record `tid` as a system service reachable by everything init starts.
fn add_service(tid: usize) {
    if tid < 64 {
        unsafe { SERVICE_MASK |= 1u64 << tid };
    }
}

fn service_mask() -> u64 {
    unsafe { SERVICE_MASK }
}

/// Give `tid` permission to send to the current service set.
fn grant_endpoints(tid: usize, slot: usize) {
    mint_and_grant(tid, slot, syscall::CAP_TYPE_ENDPOINT, service_mask(), 0);
}

/// Mint a cap in a temporary slot, grant it to a child task, then delete it.
/// Uses slot 14 as a scratch slot for minting.
fn mint_and_grant(tid: usize, dest_slot: usize, cap_type: u64, param0: u64, param1: u64) {
    const SCRATCH_SLOT: usize = 14;
    let _ = syscall::sys_cap_mint(SCRATCH_SLOT, cap_type, param0, param1);
    let _ = syscall::sys_cap_grant(tid, SCRATCH_SLOT, dest_slot);
    let _ = syscall::sys_cap_delete(SCRATCH_SLOT);
}

/// Grant capabilities based on FAT 8.3 name using fine-grained object capabilities.
/// Grant a freshly loaded program the capabilities its manifest asks for.
///
/// This used to be a chain of `base == b"KEYBOARD"` comparisons — eleven
/// branches naming every program the tree shipped. A package installed later
/// had no branch and could not be given one without rebuilding init, which is
/// not a thing a distro can live with.
///
/// The program now says what it needs and init reads it out of the image. init
/// holds every capability, so it can satisfy any request; a spawner that holds
/// less simply cannot mint what it does not have, and the kernel enforces that
/// rather than trusting the caller.
fn grant_caps_from_manifest(image: &[u8], tid: usize) {
    // Everything init starts may reach the service set. Without this no
    // program could even look up a name, since the nameserver is itself an
    // IPC destination.
    grant_endpoints(tid, syscall::SLOT_ENDPOINT);

    if let Some(reqs) = quark_rt::manifest::find(image) {
        quark_rt::manifest::grant(tid, reqs, MANIFEST_SCRATCH_SLOT);
    }
}

/// Slot in init's own CSpace used to hold a capability while handing it over.
/// Below SLOT_ENDPOINT_EXTRA (13) so it cannot tread on the endpoint sets.
const MANIFEST_SCRATCH_SLOT: usize = 12;

// (Disk-based FAT32 reader removed — init now uses VFS for disk files)

// ---------------------------------------------------------------------------
// Framebuffer info handoff to console server
// ---------------------------------------------------------------------------

/// Physical extent of the framebuffer, page aligned.
///
/// Console maps exactly this and nothing else, so this is exactly what it is
/// granted. Deriving it here rather than hardcoding a range keeps the grant
/// correct across whatever mode the bootloader actually set.
fn framebuffer_range() -> (u64, u64) {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    let size = (info.fb_pitch as u64) * (info.fb_height as u64);
    let base = info.fb_addr & !0xFFF;
    let end = (info.fb_addr + size + 0xFFF) & !0xFFF;
    (base, end)
}

fn send_fb_info(console_tid: usize) {
    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };

    // Query the kernel console's current cursor position so the
    // user-space console server can continue where the kernel left off.
    let (row, col) = syscall::sys_console_pos();

    // Pack framebuffer info into one IPC message
    // data[0] = physical address
    // data[1] = (width << 32) | height
    // data[2] = (pitch << 32) | bpp
    // data[3] = (red_pos << 16) | (green_pos << 8) | blue_pos
    // data[4] = (cursor_row << 32) | cursor_col
    let msg = Message {
        sender: 0,
        tag: 100, // TAG_FB_INIT
        data: [
            info.fb_addr,
            ((info.fb_width as u64) << 32) | (info.fb_height as u64),
            ((info.fb_pitch as u64) << 32) | (info.fb_bpp as u64),
            ((info.fb_red_pos as u64) << 16) | ((info.fb_green_pos as u64) << 8) | (info.fb_blue_pos as u64),
            ((row as u64) << 32) | (col as u64),
            0,
        ],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(console_tid, &msg, &mut reply).is_err() {
        println!("[init] Failed to send FB info to console");
    }
}

// ---------------------------------------------------------------------------
// Phase 1: Load essential services from boot image
// ---------------------------------------------------------------------------

struct BootContext {
    console_pipe: usize, // pipe handle for stdout/stderr
    input_tid: usize,
    vfs_spawn: Option<Spawned>,
}

fn load_essentials_from_boot_image(rootfs_phys: usize, rootfs_size: usize) -> BootContext {
    println!("[init] Mounting boot image");

    // Map the entire rootfs image
    let rootfs_pages = (rootfs_size + PAGE_SIZE - 1) / PAGE_SIZE;
    if syscall::sys_map_phys(rootfs_phys, BOOT_IMG_BASE, rootfs_pages).is_err() {
        println!("[init] Failed to map boot image");
        return BootContext { console_pipe: 0, input_tid: 0, vfs_spawn: None };
    }

    let rootfs = unsafe { core::slice::from_raw_parts(BOOT_IMG_BASE as *const u8, rootfs_size) };
    let bpb = parse_bpb(rootfs);
    let (entries, count) = scan_root_dir(rootfs, &bpb);

    println!("[init] Files on boot image: {}", count);

    // Pass 1: find and spawn NAMESRVR.ELF first (guarantees TID 2)
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"NAMESRVR" && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn::load(data, &SPAWN_SCRATCH) {
                    Ok(info) => {
                        // Every pass has to do this for itself; there is no
                        // shared path that does it for them. The nameserver
                        // asks for no capabilities, but it does ask to be
                        // scheduled as a server, and that arrives the same way.
                        grant_caps_from_manifest(data, info.tid);
                        let _ = spawn::set_args(&info, &[b"nameserver"], &SPAWN_SCRATCH);
                        let _ = info.start();
                        println!("[init] Spawned nameserver (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn nameserver"),
                }
            }
            break;
        }
    }

    // Pass 1b: spawn FB.ELF, the framebuffer device.
    //
    // Before the console, because the console asks it for the display. The
    // framebuffer is a device with one owner at a time, and this is what
    // decides who: the text console at boot, a compositor when one is run.
    //
    // Being first means there is nowhere for it to write yet — the pipe every
    // other service prints to is the console's, and the console does not
    // exist. Its descriptors are wired below, once there is one.
    let mut fb_tid: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"FB      " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn::load(data, &SPAWN_SCRATCH) {
                    Ok(info) => {
                        // The framebuffer, and nothing else. Its address comes
                        // from whatever mode the bootloader set, so this is the
                        // one grant a manifest cannot express. It goes into
                        // slot 0 and stays there: everything the device lends
                        // out is derived from it.
                        let (fb_base, fb_end) = framebuffer_range();
                        mint_and_grant(info.tid, 0, syscall::CAP_TYPE_PHYS_RANGE, fb_base, fb_end);
                        grant_caps_from_manifest(data, info.tid);
                        add_service(info.tid);
                        grant_endpoints(info.tid, syscall::SLOT_ENDPOINT);
                        let _ = spawn::set_args(&info, &[b"fb"], &SPAWN_SCRATCH);
                        fb_tid = info.tid;
                        let _ = info.start();
                        send_fb_info(info.tid);
                        println!("[init] Spawned fb (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn fb"),
                }
            }
            break;
        }
    }

    // Pass 2: spawn CONSOLE.ELF
    let mut console_pipe: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"CONSOLE " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn::load(data, &SPAWN_SCRATCH) {
                    Ok(info) => {
                        // No framebuffer grant: the console asks the
                        // framebuffer device for the display, and is lent the
                        // right to map it for as long as it holds it.
                        grant_caps_from_manifest(data, info.tid);
                        add_service(info.tid);
                        grant_endpoints(info.tid, syscall::SLOT_ENDPOINT);
                        let _ = spawn::set_args(&info, &[b"console"], &SPAWN_SCRATCH);
                        // Create console pipe and set fds BEFORE starting console
                        // to avoid race where console reaches main loop before fd 0 is set
                        if let Ok(pipe) = syscall::sys_pipe_create() {
                            console_pipe = pipe;
                            let _ = syscall::sys_pipe_fd_set(info.tid, 0, pipe, false);
                            let my_tid = syscall::sys_getpid() as usize;
                            let _ = syscall::sys_pipe_fd_set(my_tid, 1, pipe, true);
                            let _ = syscall::sys_pipe_fd_set(my_tid, 2, pipe, true);
                        }
                        let _ = info.start();
                        println!("[init] Spawned console (TID {})", info.tid);
                        // The framebuffer device could not be given a stdout
                        // when it started, because this pipe is what a stdout
                        // is and the console owns the other end of it. It is
                        // the one service whose diagnostics you want when the
                        // display misbehaves, so it should not be the one
                        // service that cannot speak.
                        if console_pipe != 0 && fb_tid != 0 {
                            let _ = syscall::sys_pipe_fd_set(fb_tid, 1, console_pipe, true);
                            let _ = syscall::sys_pipe_fd_set(fb_tid, 2, console_pipe, true);
                        }
                    }
                    Err(()) => println!("[init] FAILED to spawn console"),
                }
            }
            break;
        }
    }

    // Pass 3: spawn essential ELFs (KEYBOARD, DISK) — skip INPUT and non-essentials
    let mut spawned_tids = [0usize; 32];
    let mut spawned_count = 0usize;
    for i in 0..count {
        let e = &entries[i];

        // Skip already-spawned
        if &e.name[0..8] == b"NAMESRVR" || (&e.name[0..8] == b"CONSOLE " && &e.name[8..11] == b"ELF") {
            continue;
        }
        // Skip INPUT (deferred to after keyboard) and VFS (deferred to after phase 2)
        if (&e.name[0..8] == b"INPUT   " || &e.name[0..8] == b"VFS     ") && &e.name[8..11] == b"ELF" {
            continue;
        }
        // Only spawn .ELF files
        if &e.name[8..11] != b"ELF" {
            continue;
        }
        // Skip non-essential ELFs — they will be loaded from disk later
        if !is_essential_elf(&e.name) {
            continue;
        }

        let mut namebuf = [0u8; 16];
        let namelen = fat_name_to_buf(&e.name, &mut namebuf);
        if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
            match spawn::load(data, &SPAWN_SCRATCH) {
                Ok(info) => {
                    let tid = info.tid;
                    add_service(tid);
                    grant_caps_from_manifest(data, tid);
                    if console_pipe != 0 {
                        let _ = syscall::sys_pipe_fd_set(tid, 1, console_pipe, true);
                        let _ = syscall::sys_pipe_fd_set(tid, 2, console_pipe, true);
                    }
                    let _ = spawn::set_args(&info, &[&namebuf[..namelen]], &SPAWN_SCRATCH);
                    let _ = info.start();
                    if spawned_count < 32 {
                        spawned_tids[spawned_count] = tid;
                        spawned_count += 1;
                    }
                    let base_len = e.name[0..8].iter().rposition(|&b| b != b' ').map_or(0, |p| p + 1);
                    let mut lbuf = [0u8; 8];
                    for j in 0..base_len { lbuf[j] = e.name[j].to_ascii_lowercase(); }
                    if let Ok(name) = core::str::from_utf8(&lbuf[..base_len]) {
                        println!("[init] Spawned {} (TID {})", name, tid);
                    }
                }
                Err(()) => println!("[init] FAILED to spawn"),
            }
        }
    }

    // Pass 4: spawn INPUT.ELF (needs keyboard to be running)
    let mut input_tid: usize = 0;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"INPUT   " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn::load(data, &SPAWN_SCRATCH) {
                    Ok(info) => {
                        input_tid = info.tid;
                        // This pass once granted nothing at all, so input ran
                        // with an empty CSpace — invisible while UID 0 bypassed
                        // every check. Each pass must grant; there is no shared
                        // path that does it for them.
                        add_service(info.tid);
                        grant_caps_from_manifest(data, info.tid);
                        if console_pipe != 0 {
                            let _ = syscall::sys_pipe_fd_set(info.tid, 1, console_pipe, true);
                            let _ = syscall::sys_pipe_fd_set(info.tid, 2, console_pipe, true);
                        }
                        let _ = spawn::set_args(&info, &[b"input"], &SPAWN_SCRATCH);
                        let _ = info.start();
                        println!("[init] Spawned input (TID {})", info.tid);
                    }
                    Err(()) => println!("[init] FAILED to spawn input"),
                }
            }
            break;
        }
    }

    // Wire fd 0 (stdin) to input server for all previously spawned tasks
    if input_tid != 0 {
        for i in 0..spawned_count {
            let _ = syscall::sys_fd_set(spawned_tids[i], 0, input_tid, 1); // TAG_READ=1
        }
    }

    // Pass 5: load VFS.ELF but do NOT start it yet (deferred to after phase 2)
    let mut vfs_spawn: Option<Spawned> = None;
    for i in 0..count {
        let e = &entries[i];
        if &e.name[0..8] == b"VFS     " && &e.name[8..11] == b"ELF" {
            if let Ok(data) = read_file_to_buffer(rootfs, &bpb, e.first_cluster, e.file_size) {
                match spawn::load(data, &SPAWN_SCRATCH) {
                    Ok(info) => {
                        add_service(info.tid);
                        grant_caps_from_manifest(data, info.tid);
                        if console_pipe != 0 {
                            let _ = syscall::sys_pipe_fd_set(info.tid, 1, console_pipe, true);
                            let _ = syscall::sys_pipe_fd_set(info.tid, 2, console_pipe, true);
                        }
                        if input_tid != 0 {
                            let _ = syscall::sys_fd_set(info.tid, 0, input_tid, 1);
                        }
                        let _ = spawn::set_args(&info, &[b"vfs"], &SPAWN_SCRATCH);
                        println!("[init] Spawned vfs (TID {}, deferred start)", info.tid);
                        vfs_spawn = Some(info);
                    }
                    Err(()) => println!("[init] FAILED to spawn vfs"),
                }
            }
            break;
        }
    }

    BootContext { console_pipe, input_tid, vfs_spawn }
}

// ---------------------------------------------------------------------------
// Phase 2: Load remaining programs from disk
// ---------------------------------------------------------------------------

const MAX_DEFERRED: usize = 16;

struct DeferredTasks {
    spawns: [Option<Spawned>; MAX_DEFERRED],
    count: usize,
}

impl DeferredTasks {
    fn new() -> Self {
        const NONE: Option<Spawned> = None;
        DeferredTasks { spawns: [NONE; MAX_DEFERRED], count: 0 }
    }

    /// Start each deferred task one at a time, waiting for each to exit
    /// before starting the next (prevents interleaved output).
    fn start_sequentially(&mut self) {
        for i in 0..self.count {
            if let Some(info) = self.spawns[i].take() {
                let _ = info.start();
                let _ = syscall::sys_wait();
            }
        }
    }
}

fn load_from_vfs(vfs_tid: usize, console_pipe: usize, input_tid: usize) -> DeferredTasks {
    let mut deferred = DeferredTasks::new();

    // Open /usr/bin directory via VFS
    let (dir_handle, _, _) = match vfs::open(vfs_tid, b"/usr/bin") {
        Ok(h) => h,
        Err(_) => {
            println!("[init] /usr/bin not found on VFS.");
            return deferred;
        }
    };

    // Find LOGIN.ELF (or SHELL.ELF as fallback) in /usr/bin
    let mut login_entry: Option<vfs::DirEntry> = None;
    let mut shell_entry: Option<vfs::DirEntry> = None;
    let mut index = 0u32;

    loop {
        match vfs::readdir(vfs_tid, dir_handle, index) {
            Ok(Some(entry)) => {
                if !entry.is_dir && name_matches_entry(&entry, b"LOGIN", b"ELF") {
                    login_entry = Some(entry);
                    break;
                }
                if !entry.is_dir && name_matches_entry(&entry, b"SHELL", b"ELF") {
                    shell_entry = Some(entry);
                }
                index += 1;
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = vfs::close(vfs_tid, dir_handle);

    let entry = match login_entry.or(shell_entry) {
        Some(e) => e,
        None => {
            println!("[init] login/shell not found in /usr/bin");
            return deferred;
        }
    };

    // Get the actual filename from the entry
    let name_bytes = entry.name_bytes();
    let mut namebuf = [0u8; 48];
    let namelen = name_bytes.len();
    namebuf[..namelen].copy_from_slice(name_bytes);
    let loading_name = if login_entry.is_some() { "login" } else { "shell" };

    // Build path: "/usr/bin/SHELL.ELF" or "/usr/bin/LOGIN.ELF"
    let mut path = [0u8; 48];
    let prefix = b"/usr/bin/";
    path[..prefix.len()].copy_from_slice(prefix);
    path[prefix.len()..prefix.len() + namelen].copy_from_slice(&namebuf[..namelen]);
    let path_len = prefix.len() + namelen;

    // Open file via VFS
    let (file_handle, file_size, _) = match vfs::open(vfs_tid, &path[..path_len]) {
        Ok(h) => h,
        Err(_) => {
            println!("[init]   FAILED to open via VFS");
            return deferred;
        }
    };

    let size = file_size as usize;
    let pages_needed = (size + PAGE_SIZE - 1) / PAGE_SIZE;

    // Allocate pages and read file content via VFS directly into them
    let mut success = true;
    for p in 0..pages_needed {
        let frame = match syscall::sys_phys_alloc(1) {
            Ok(f) => f,
            Err(()) => { success = false; break; }
        };
        if syscall::sys_map_phys(frame, FILE_BUF_BASE + p * PAGE_SIZE, 1).is_err() {
            success = false; break;
        }
        let offset = (p * PAGE_SIZE) as u32;
        let to_read = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        if vfs::read(vfs_tid, file_handle, frame, offset, to_read).is_err() {
            success = false; break;
        }
    }

    let _ = vfs::close(vfs_tid, file_handle);

    if !success {
        println!("[init]   FAILED to read from VFS");
        return deferred;
    }

    let data = unsafe { core::slice::from_raw_parts(FILE_BUF_BASE as *const u8, size) };
    match spawn::load(data, &SPAWN_SCRATCH) {
        Ok(info) => {
            let tid = info.tid;
            grant_caps_from_manifest(data, tid);
            if console_pipe != 0 {
                let _ = syscall::sys_pipe_fd_set(tid, 1, console_pipe, true);
                let _ = syscall::sys_pipe_fd_set(tid, 2, console_pipe, true);
            }
            if input_tid != 0 {
                let _ = syscall::sys_fd_set(tid, 0, input_tid, 1);
            }
            let _ = spawn::set_args(&info, &[&namebuf[..namelen]], &SPAWN_SCRATCH);
            println!("[init] Spawned {} (TID {}, deferred start)", loading_name, tid);
            if deferred.count < MAX_DEFERRED {
                deferred.spawns[deferred.count] = Some(info);
                deferred.count += 1;
            }
        }
        Err(()) => println!("[init]   FAILED to spawn"),
    }

    deferred
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[init] Starting init process.");

    let info = unsafe { &*(BOOT_INFO_ADDR as *const BootInfo) };
    let mod_count = info.module_count as usize;

    // Find boot image module
    let mut found = false;
    for i in 0..mod_count {
        let m = &info.modules[i];
        let name = module_name(&m.name);
        if starts_with(name, b"boot") {
            let phys = m.phys_start as usize;
            let size = (m.phys_end - m.phys_start) as usize;

            // Phase 1: Load essential services from boot image
            let ctx = load_essentials_from_boot_image(phys, size);

            // Unload boot image — return pages to the physical memory allocator
            let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
            let _ = syscall::sys_phys_free(phys, pages);
            println!("[init] Boot image freed ({} pages).", pages);

            // Phase 2: Start VFS, wait for it to register
            let vfs_tid = if let Some(vfs) = ctx.vfs_spawn {
                println!("[init] Starting VFS (TID {})", vfs.tid);
                let _ = vfs.start();
                match nameserver::lookup_retry(b"vfs", 50) {
                    Some(tid) => {
                        println!("[init] VFS ready.");
                        Some(tid)
                    }
                    None => {
                        println!("[init] VFS failed to register.");
                        None
                    }
                }
            } else {
                None
            };

            // Phase 3: Load remaining programs from VFS (loaded but not started)
            let mut deferred = if let Some(vfs) = vfs_tid {
                load_from_vfs(vfs, ctx.console_pipe, ctx.input_tid)
            } else {
                println!("[init] No VFS, skipping disk program loading.");
                DeferredTasks::new()
            };

            // Phase 4: Start non-essential programs
            // Services are started as they are spawned, so an early one holds a
            // mask that predates its peers. Hand every service a second
            // Endpoint capability carrying the completed set; task_has_endpoint
            // takes the union across a CSpace, so this only ever widens.
            let final_mask = service_mask();
            for tid in 0..64usize {
                if tid > NAMESERVER_TID && final_mask & (1u64 << tid) != 0 {
                    mint_and_grant(
                        tid,
                        syscall::SLOT_ENDPOINT_EXTRA,
                        syscall::CAP_TYPE_ENDPOINT,
                        final_mask,
                        0,
                    );
                }
            }

            println!("[init] All programs loaded. Starting deferred tasks.");
            deferred.start_sequentially();

            found = true;
            break;
        }
    }

    if !found {
        println!("[init] ERROR: boot image module not found!");
    }

    // Everything is started, so step out of the band that let those grants be
    // made — nothing init does from here needs to come before a driver.
    let me = syscall::sys_getpid() as usize;
    let _ = syscall::sys_task_priority(me, syscall::PRIO_NORMAL);

    // Collect the dead. A task that has exited keeps its slot, its kernel
    // stack and its address space until a parent collects it, and init is the
    // parent of everything the system starts — so an init that never waits is
    // a steady leak of the table that decides how many tasks can exist at all.
    //
    // Blocking here rather than spinning: this is the same wait a shell does
    // for a foreground command, and it is what init has to do for the rest of
    // the machine's life.
    loop {
        if syscall::sys_wait().is_err() {
            // No children at all, which should not happen while the servers
            // are up. Wait rather than ask again as fast as the machine can.
            syscall::sleep_ticks(100);
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[init] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
