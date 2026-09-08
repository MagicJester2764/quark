//! Loading an ELF and starting it as a new task.
//!
//! There is no fork or exec: a parent creates a task and an address space,
//! stages the program's pages through its own address space, maps them into the
//! child, and starts it. That is a hundred lines of fiddly page arithmetic, and
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

/// Top of the user stack, in the child. Stacks grow down from here.
pub const STACK_TOP: usize = 0x7FFF_FFFF_F000;
pub const STACK_PAGES: usize = 4;

/// Scratch virtual addresses in the caller's own address space, used to stage
/// pages before they are mapped into the child.
///
/// Each caller needs its own: they are mapped and remapped repeatedly, so two
/// spawners sharing a region would overwrite each other. A range of at least
/// `STACK_PAGES` pages is needed at `stack`, and as many pages as the largest
/// program segment at `elf`.
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
}

impl Spawned {
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

/// Create a task and load `elf` into a fresh address space for it.
///
/// The returned task is not running; call [`Spawned::start`].
///
/// On failure the task and address space created so far are left behind. That
/// matches what the three copies did, and cleaning it up properly wants a
/// teardown syscall that does not exist yet.
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

    let cr3 = syscall::sys_addrspace_create()?;
    let tid = syscall::sys_task_create()?;

    // Adjacent segments can share a page: a read-only one ending part way
    // through it and the next beginning in the same one. Allocating a fresh
    // frame per page per segment then maps the second over the first, losing
    // whatever the first had written — the GOT, in the case that found this,
    // leaving every call through it going to zero.
    //
    // Only neighbours can overlap, since segments are laid out in address
    // order, so remembering the last page of the previous one is enough.
    let mut prev_page: usize = usize::MAX;
    let mut prev_frame: usize = 0;

    for i in 0..phnum {
        let offset = match phoff.checked_add(i * phentsize) {
            Some(o) => o,
            None => break,
        };
        if offset + phentsize > elf.len() {
            break;
        }
        let phdr = unsafe { &*(elf.as_ptr().add(offset) as *const Elf64Phdr) };
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;
        let file_offset = phdr.p_offset as usize;
        let writable = phdr.p_flags & 2 != 0;

        let vaddr_page_start = vaddr & !0xFFF;
        let vaddr_end = vaddr + memsz;
        let pages = (vaddr_end - vaddr_page_start + PAGE_SIZE - 1) / PAGE_SIZE;

        let file_start = vaddr;
        let file_end = vaddr + filesz;

        for p in 0..pages {
            let page_vaddr = vaddr_page_start + p * PAGE_SIZE;

            let reused = page_vaddr == prev_page;
            let frame = if reused {
                prev_frame
            } else {
                syscall::sys_phys_alloc(1)?
            };
            let temp_page = scratch.elf + p * PAGE_SIZE;
            syscall::sys_map_phys(frame, temp_page, 1)?;

            // Zero first: the tail of the last page of a segment is .bss, and
            // a fresh frame is not guaranteed to be clear. A reused page
            // already holds the previous segment's bytes, which must survive.
            if !reused {
                unsafe { core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE) };
            }

            let page_end = page_vaddr + PAGE_SIZE;
            if file_start < page_end && file_end > page_vaddr {
                let copy_vstart = file_start.max(page_vaddr);
                let copy_vend = file_end.min(page_end);
                let copy_len = copy_vend - copy_vstart;
                let dst_offset = copy_vstart - page_vaddr;
                let src_offset = file_offset + (copy_vstart - vaddr);

                if src_offset + copy_len <= elf.len() {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            elf.as_ptr().add(src_offset),
                            (temp_page + dst_offset) as *mut u8,
                            copy_len,
                        );
                    }
                }
            }

            let flags: u64 = if writable { 1 } else { 0 };
            syscall::sys_addrspace_map(cr3, page_vaddr, frame, 1, flags)?;

            prev_page = page_vaddr;
            prev_frame = frame;
        }
    }

    let stack_bottom = STACK_TOP - STACK_PAGES * PAGE_SIZE;
    for p in 0..STACK_PAGES {
        let frame = syscall::sys_phys_alloc(1)?;
        let temp_page = scratch.stack + p * PAGE_SIZE;
        syscall::sys_map_phys(frame, temp_page, 1)?;
        unsafe { core::ptr::write_bytes(temp_page as *mut u8, 0, PAGE_SIZE) };
        syscall::sys_addrspace_map(cr3, stack_bottom + p * PAGE_SIZE, frame, 1, 1)?;
    }

    Ok(Spawned { tid, entry, stack_top: STACK_TOP as u64, cr3 })
}

/// Write `args` into the child's argv page, read back by `quark_rt::args`.
///
/// Layout: a count, then each argument as a length followed by its bytes.
/// Arguments that would overflow the page are dropped rather than truncated.
pub fn set_args(info: &Spawned, args: &[&[u8]], scratch: &Scratch) -> Result<(), ()> {
    let frame = syscall::sys_phys_alloc(1)?;
    syscall::sys_map_phys(frame, scratch.args, 1)?;

    let base = scratch.args as *mut u8;
    unsafe {
        core::ptr::write_bytes(base, 0, PAGE_SIZE);
        *(base as *mut u64) = args.len() as u64;

        let mut offset = 8usize;
        for arg in args {
            if offset + 8 + arg.len() > PAGE_SIZE {
                break;
            }
            *(base.add(offset) as *mut u64) = arg.len() as u64;
            offset += 8;
            core::ptr::copy_nonoverlapping(arg.as_ptr(), base.add(offset), arg.len());
            offset += arg.len();
        }
    }

    // Read-only in the child: argv is not its to rewrite.
    syscall::sys_addrspace_map(info.cr3, ARGS_PAGE_ADDR, frame, 1, 0)?;
    Ok(())
}
