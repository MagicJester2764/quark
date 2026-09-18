//! Loading an ELF and starting it as a new task.
//!
//! There is no fork or exec: a parent creates a task and an address space,
//! builds the program's pages in its own memory, moves them into the child,
//! and starts it. That is a hundred lines of fiddly page arithmetic, and
//! it was written out three times — in `init`, `shell` and `login` — with the
//! shell's and login's copies byte-identical and init's differing only in
//! comments and hardcoded scratch addresses. Drift between them was a matter of
//! time.
//!
//! Staging needs scratch addresses in the *caller's* address space, and those
//! must differ per caller, so they are a parameter rather than a constant here.

use crate::syscall;

pub const PAGE_SIZE: usize = 4096;

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const PT_LOAD: u32 = 1;
const EHDR_SIZE: usize = 64;
const PHDR_SIZE: usize = 56;

/// Where the argv page lands in the *child's* address space, by convention
/// with `quark_rt::args`.
pub const ARGS_PAGE_ADDR: usize = 0x80_8000_0000;

/// Program headers carried to the child, at most.
///
/// A static binary built here has four or five; a Linux one with everything
/// the toolchain adds has about a dozen.
pub const MAX_PHDRS: usize = 16;

/// Where on the argument page the program header table goes: a fixed place at
/// the end, so that a long command line can never crowd it out.
///
/// Layout from this offset: the entry size, the entry count, then the table.
/// It is what a C library's `AT_PHDR` points into, and it has to exist because
/// a program's own headers are not in any segment it loads — musl finds its
/// thread-local template through them, and without them every thread-local in
/// a C program landed outside its thread's block. Mirrored as
/// `QUARK_PHDRS_AT` in `quark/layout.h`.
pub const PHDRS_AT: usize = PAGE_SIZE - 16 - MAX_PHDRS * PHDR_SIZE;

const PT_PHDR: u32 = 6;

/// Top of the user stack, in the child. Stacks grow down from here.
pub const STACK_TOP: usize = 0x7FFF_FFFF_F000;
/// 1 MiB, matching the kernel's own `USER_STACK_PAGES`. A spawner maps this
/// eagerly, so it is memory spent per task rather than reserved address
/// space — see the note there for why it is this size and not eight
/// megabytes.
pub const STACK_PAGES: usize = 256;

/// The most address space a program may span, from its first loaded page to
/// its last: a gigabyte. The image is built laid out as the child will see it,
/// so this is what a spawner must leave free at `Scratch::elf`.
pub const MAX_IMAGE_SPAN: usize = 1 << 30;

/// Loadable segments a program may have. A program built here has three or
/// four.
const MAX_SEGMENTS: usize = MAX_PHDRS;

/// The most pages `sys_mmap`, `sys_munmap` and `sys_addrspace_give` take at
/// once.
const CHUNK: usize = 256;

/// Scratch virtual addresses in the caller's own address space, where a
/// program is built before it is moved into the child.
///
/// Each caller needs its own, since two spawners building at the same address
/// would collide. A range of `STACK_PAGES` pages must be free at `stack`, one
/// page at `args`, and [`MAX_IMAGE_SPAN`] at `elf`. All three are empty again
/// when a load returns, whether or not it succeeded.
#[derive(Clone, Copy)]
pub struct Scratch {
    pub elf: usize,
    pub stack: usize,
    pub args: usize,
}

/// A task that has been created and loaded but not yet started.
#[derive(Clone, Copy)]
pub struct Spawned {
    pub tid: usize,
    pub entry: u64,
    pub stack_top: u64,
    pub cr3: usize,
    /// The program's own header table, verbatim, for the argument page.
    phdrs: [u8; MAX_PHDRS * PHDR_SIZE],
    phnum: usize,
}

impl Spawned {
    /// A placeholder for an array of spawned tasks not yet filled in. It names
    /// no task; starting it fails.
    pub const EMPTY: Spawned = Spawned {
        tid: 0,
        entry: 0,
        stack_top: 0,
        cr3: 0,
        phdrs: [0; MAX_PHDRS * PHDR_SIZE],
        phnum: 0,
    };

    /// Run it. Nothing happens until this is called, which is what lets a
    /// caller wire capabilities, file descriptors and pipes first.
    pub fn start(&self) -> Result<(), ()> {
        syscall::sys_task_start(self.tid, self.entry, self.stack_top, self.cr3)
    }
}

#[repr(C)]
struct Elf64Header {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
}

#[repr(C)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

/// A loadable segment, checked.
#[derive(Clone, Copy)]
struct Segment {
    /// The pages it occupies in the child, `[first, end)`.
    first: usize,
    end: usize,
    vaddr: usize,
    /// Where its initialised bytes end in the child.
    vend: usize,
    offset: usize,
    filesz: usize,
    writable: bool,
}

impl Segment {
    const EMPTY: Segment = Segment {
        first: 0,
        end: 0,
        vaddr: 0,
        vend: 0,
        offset: 0,
        filesz: 0,
        writable: false,
    };
}

/// Create a task and load `elf` into a fresh address space for it.
///
/// The returned task is not running; call [`Spawned::start`].
///
/// The image is built in the caller's own memory, laid out as the child will
/// see it, and then moved into the child with `sys_addrspace_give`. Moved
/// pages are the child's: they are freed when it is gone, and the caller keeps
/// no way to reach them. Every spawner used to lend the child frames it had
/// allocated itself, which kept them for as long as the *spawner* lived — a
/// megabyte and a half for every program the shell ran, until nothing more
/// could be loaded.
///
/// On failure the task and address space created so far are left behind.
/// That matches what the three copies did, and cleaning it up properly wants a
/// teardown syscall that does not exist yet. The caller's scratch ranges are
/// always emptied.
pub fn load(elf: &[u8], scratch: &Scratch) -> Result<Spawned, ()> {
    if elf.len() < EHDR_SIZE || elf[0..4] != ELF_MAGIC {
        return Err(());
    }

    let hdr = unsafe { &*(elf.as_ptr() as *const Elf64Header) };
    let entry = hdr.e_entry;
    let phoff = hdr.e_phoff as usize;
    let phentsize = hdr.e_phentsize as usize;
    let phnum = hdr.e_phnum as usize;

    // A program header shorter than the struct would have us read past the
    // entry into whatever follows.
    if phentsize < PHDR_SIZE {
        return Err(());
    }

    // The table, for the child to be told about. All of it or none: a
    // partial table is a program being told it has fewer segments than it
    // does, and the one it is looking for may be past the cut.
    let mut phdrs = [0u8; MAX_PHDRS * PHDR_SIZE];
    let mut kept = 0usize;
    let table_end = phoff
        .checked_add(phnum.saturating_mul(phentsize))
        .unwrap_or(usize::MAX);
    if phnum <= MAX_PHDRS && table_end <= elf.len() {
        for i in 0..phnum {
            let from = phoff + i * phentsize;
            phdrs[i * PHDR_SIZE..(i + 1) * PHDR_SIZE]
                .copy_from_slice(&elf[from..from + PHDR_SIZE]);
        }
        kept = phnum;
    }

    let mut segs = [Segment::EMPTY; MAX_SEGMENTS];
    let n = segments(elf, phoff, phentsize, phnum, &mut segs).ok_or(())?;
    let segs = &segs[..n];
    let base = segs[0].first;

    let cr3 = syscall::sys_addrspace_create()?;
    let tid = syscall::sys_task_create_in(cr3 as u64)?;

    let loaded = build(elf, segs, base, scratch.elf)
        .and_then(|()| give_image(cr3, segs, base, scratch.elf))
        .and_then(|()| give_stack(cr3, scratch.stack));
    if loaded.is_err() {
        // What was not given is still ours. Left mapped it would be in the
        // way of the next load, which never maps over anything.
        for s in segs {
            release(scratch.elf + (s.first - base), (s.end - s.first) / PAGE_SIZE);
        }
        release(scratch.stack, STACK_PAGES);
        return Err(());
    }

    Ok(Spawned { tid, entry, stack_top: STACK_TOP as u64, cr3, phdrs, phnum: kept })
}

/// Read and check the loadable segments into `out`, returning how many there
/// are. `None` if there are none, or too many, or any is malformed: out of
/// address order, overlapping, claiming bytes past the end of the file, or
/// making the image wider than [`MAX_IMAGE_SPAN`].
fn segments(
    elf: &[u8],
    phoff: usize,
    phentsize: usize,
    phnum: usize,
    out: &mut [Segment; MAX_SEGMENTS],
) -> Option<usize> {
    let mut n = 0;
    for i in 0..phnum {
        let at = phoff.checked_add(i.checked_mul(phentsize)?)?;
        if at.checked_add(phentsize)? > elf.len() {
            return None;
        }
        let ph = unsafe { &*(elf.as_ptr().add(at) as *const Elf64Phdr) };
        if ph.p_type != PT_LOAD || ph.p_memsz == 0 {
            continue;
        }
        let vaddr = ph.p_vaddr as usize;
        let memsz = ph.p_memsz as usize;
        let filesz = ph.p_filesz as usize;
        let offset = ph.p_offset as usize;
        if filesz > memsz || offset.checked_add(filesz)? > elf.len() {
            return None;
        }
        let vend = vaddr.checked_add(memsz)?;
        let seg = Segment {
            first: vaddr & !(PAGE_SIZE - 1),
            end: vend.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1),
            vaddr,
            vend,
            offset,
            filesz,
            writable: ph.p_flags & 2 != 0,
        };
        // In address order and apart, which the format requires. Two can
        // still share a page — one ending part way through it and the next
        // beginning there — and building the image as one piece is what makes
        // that work: both write into the same page.
        if n > 0 && seg.vaddr < out[n - 1].vend {
            return None;
        }
        let base = if n == 0 { seg.first } else { out[0].first };
        if n == MAX_SEGMENTS || seg.end - base > MAX_IMAGE_SPAN {
            return None;
        }
        out[n] = seg;
        n += 1;
    }
    if n == 0 { None } else { Some(n) }
}

/// Lay the segments out at `at`, as the child will see them from `base`.
fn build(elf: &[u8], segs: &[Segment], base: usize, at: usize) -> Result<(), ()> {
    let mut mapped = base;
    for s in segs {
        // A page shared with the segment before is already there, holding
        // that segment's bytes, which must survive.
        let start = s.first.max(mapped);
        if start < s.end {
            map_fresh(at + (start - base), (s.end - start) / PAGE_SIZE)?;
        }
        mapped = s.end;
        // Fresh memory is zeroed, so the .bss after the file bytes needs
        // nothing more.
        unsafe {
            core::ptr::copy_nonoverlapping(
                elf.as_ptr().add(s.offset),
                (at + (s.vaddr - base)) as *mut u8,
                s.filesz,
            );
        }
    }
    Ok(())
}

/// Move the image built at `at` into the child.
fn give_image(cr3: usize, segs: &[Segment], base: usize, at: usize) -> Result<(), ()> {
    let mut given = base;
    for (i, s) in segs.iter().enumerate() {
        let start = s.first.max(given);
        if start >= s.end {
            continue;
        }
        // A last page that the next segment begins in has to suit both of
        // them, so it goes on its own, writable if any segment in it is.
        let shared = segs.get(i + 1).is_some_and(|next| next.first < s.end);
        let whole = if shared { s.end - PAGE_SIZE } else { s.end };
        give(cr3, start, at + (start - base), (whole - start) / PAGE_SIZE, s.writable)?;
        if shared {
            let last = s.end - PAGE_SIZE;
            let writable = segs.iter().any(|t| t.writable && t.first <= last && last < t.end);
            give(cr3, last, at + (last - base), 1, writable)?;
        }
        given = s.end;
    }
    Ok(())
}

/// Build the stack at `at` and move it into the child. Fresh memory is
/// zeroed, which is all a stack needs.
fn give_stack(cr3: usize, at: usize) -> Result<(), ()> {
    map_fresh(at, STACK_PAGES)?;
    give(cr3, STACK_TOP - STACK_PAGES * PAGE_SIZE, at, STACK_PAGES, true)
}

/// Map `pages` of fresh, zeroed memory at `at`, or nothing.
fn map_fresh(at: usize, pages: usize) -> Result<(), ()> {
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(CHUNK);
        if syscall::sys_mmap(at + done * PAGE_SIZE, n).is_err() {
            release(at, done);
            return Err(());
        }
        done += n;
    }
    Ok(())
}

/// Move `pages` pages at `from` in the caller to `virt` in `cr3`.
fn give(cr3: usize, virt: usize, from: usize, pages: usize, writable: bool) -> Result<(), ()> {
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(CHUNK);
        syscall::sys_addrspace_give(
            cr3,
            virt + done * PAGE_SIZE,
            from + done * PAGE_SIZE,
            n,
            writable as u64,
        )?;
        done += n;
    }
    Ok(())
}

/// Unmap and free whatever is still mapped of `pages` pages at `at`.
fn release(at: usize, pages: usize) {
    let mut done = 0;
    while done < pages {
        let n = (pages - done).min(CHUNK);
        let _ = syscall::sys_munmap(at + done * PAGE_SIZE, n);
        done += n;
    }
}

/// The largest program [`load_path`] will read: thirty-two megabytes.
///
/// It was four, which was generous for a program written here and not nearly
/// enough for one that links a toolkit: pango and its dependencies come to
/// seven megabytes of static library before anything is drawn, and GTK is
/// several times that. The cost is address space in the spawner while the
/// image is being read, not memory — the pages are mapped as they are filled
/// and given to the child.
pub const MAX_IMAGE_PAGES: usize = 8192;

/// Read a program from the filesystem, load it, and give back the memory it
/// was read into.
///
/// Every spawner used to stage a program at one fixed address and never free
/// the frames: the shell leaked a program's size in memory every time it ran
/// one. That went unnoticed while programs were a few pages; a test suite of
/// thirty half-megabyte binaries would not have survived it.
///
/// The image is read into ordinary memory, a page per call, each page lent to
/// the VFS to fill. It needs no capability to allocate frames, and nothing
/// here ever knows where the image is in physical memory.
///
/// `image_at` is a free range of `MAX_IMAGE_PAGES` pages in the caller.
/// `grant` sees the image before it is released, which is when a spawner reads
/// the program's manifest; it is called only if loading succeeded.
pub fn load_path(
    vfs_tid: usize,
    path: &[u8],
    image_at: usize,
    scratch: &Scratch,
    grant: impl FnOnce(&[u8], usize),
) -> Result<Spawned, ()> {
    let (handle, size, _) = crate::vfs::open(vfs_tid, path).map_err(|_| ())?;
    let size = size as usize;
    let pages = size.div_ceil(PAGE_SIZE);
    if pages == 0 || pages > MAX_IMAGE_PAGES || map_fresh(image_at, pages).is_err() {
        let _ = crate::vfs::close(vfs_tid, handle);
        return Err(());
    }

    let image = unsafe { core::slice::from_raw_parts_mut(image_at as *mut u8, size) };
    let read_whole = image.chunks_mut(PAGE_SIZE).enumerate().all(|(p, page)| {
        crate::vfs::read(vfs_tid, handle, page, (p * PAGE_SIZE) as u32) == Ok(page.len() as u32)
    });
    let _ = crate::vfs::close(vfs_tid, handle);

    let result = if read_whole {
        let loaded = load(image, scratch);
        if let Ok(info) = loaded {
            grant(image, info.tid);
        }
        loaded
    } else {
        Err(())
    };

    // The child has pages of its own now; this was only ever a copy.
    release(image_at, pages);
    result
}

/// Write `args` and `env` into the child's argument page, read back by
/// `quark_rt::args`.
///
/// Layout: a count, then each entry as a length followed by its bytes; the
/// arguments first and the environment after them. An entry that would
/// overflow the page is dropped rather than truncated — half an environment
/// variable is worse than a missing one.
///
/// The page is mapped read-only in the child. That is safe for an environment
/// as well as for argv, because nothing writes through these bytes: a C
/// library builds its own array of pointers into them, and `setenv` allocates
/// a new string rather than editing one in place.
pub fn set_args_env(
    info: &Spawned,
    args: &[&[u8]],
    env: &[&[u8]],
    scratch: &Scratch,
) -> Result<(), ()> {
    syscall::sys_mmap(scratch.args, 1)?;

    let base = scratch.args as *mut u8;
    unsafe {
        let mut offset = 0usize;
        for section in [args, env] {
            let count_at = offset;
            offset += 8;
            let mut written = 0u64;
            for item in section {
                // Stop short of the program headers, which own the end of the
                // page whatever the arguments are.
                if offset + 8 + item.len() > PHDRS_AT {
                    break;
                }
                *(base.add(offset) as *mut u64) = item.len() as u64;
                offset += 8;
                core::ptr::copy_nonoverlapping(item.as_ptr(), base.add(offset), item.len());
                offset += item.len();
                written += 1;
            }
            *(base.add(count_at) as *mut u64) = written;
        }

        write_phdrs(base, info);
    }

    let given = syscall::sys_addrspace_give(info.cr3, ARGS_PAGE_ADDR, scratch.args, 1, 0);
    if given.is_err() {
        release(scratch.args, 1);
    }
    given
}

/// Put the program header table at the end of the argument page.
///
/// A `PT_PHDR` entry is rewritten to say the table is where this copy is. A C
/// library takes the difference between `AT_PHDR` and that entry's address as
/// the load base, and for a program that is not relocated the base has to come
/// out as zero; left as it was, every address derived from the headers —
/// the thread-local template among them — would be off by the distance to
/// this page.
unsafe fn write_phdrs(base: *mut u8, info: &Spawned) {
    let at = PHDRS_AT;
    let table = ARGS_PAGE_ADDR + at + 16;
    unsafe {
        *(base.add(at) as *mut u64) = PHDR_SIZE as u64;
        *(base.add(at + 8) as *mut u64) = info.phnum as u64;
        for i in 0..info.phnum {
            let dst = base.add(at + 16 + i * PHDR_SIZE);
            core::ptr::copy_nonoverlapping(
                info.phdrs.as_ptr().add(i * PHDR_SIZE),
                dst,
                PHDR_SIZE,
            );
            let ph = &mut *(dst as *mut Elf64Phdr);
            if ph.p_type == PT_PHDR {
                ph.p_vaddr = table as u64;
                ph.p_paddr = table as u64;
            }
        }
    }
}

/// As [`set_args_env`], with no environment.
pub fn set_args(info: &Spawned, args: &[&[u8]], scratch: &Scratch) -> Result<(), ()> {
    set_args_env(info, args, &[], scratch)
}
