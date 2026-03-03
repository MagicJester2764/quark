/// ELF64 loader for the Quark microkernel.
///
/// BOOTSTRAP ONLY — used once to load init. After boot, user-space
/// processes are responsible for ELF loading via microkernel syscalls.
///
/// Parses ELF headers from boot modules and maps PT_LOAD segments
/// into a user address space.

use crate::{pmm, userspace};

const ELF_MAGIC: [u8; 4] = [0x7F, b'E', b'L', b'F'];
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

const PAGE_SIZE: usize = 4096;

#[derive(Debug)]
pub enum ElfError {
    BadMagic,
    Not64Bit,
    NotLittleEndian,
    NotExecutable,
    WrongArch,
    NoLoadSegments,
    MapFailed,
}

/// ELF64 file header.
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
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
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// ELF64 program header (segment descriptor).
#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
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

/// Validate an ELF64 header.
fn validate_header(hdr: &Elf64Header) -> Result<(), ElfError> {
    if hdr.e_ident[0..4] != ELF_MAGIC {
        return Err(ElfError::BadMagic);
    }
    if hdr.e_ident[4] != ELFCLASS64 {
        return Err(ElfError::Not64Bit);
    }
    if hdr.e_ident[5] != ELFDATA2LSB {
        return Err(ElfError::NotLittleEndian);
    }
    if hdr.e_type != ET_EXEC {
        return Err(ElfError::NotExecutable);
    }
    if hdr.e_machine != EM_X86_64 {
        return Err(ElfError::WrongArch);
    }
    Ok(())
}

/// Load an ELF64 binary from raw bytes into a user address space.
///
/// Returns (pml4_phys, entry_point, stack_top).
pub fn load_elf(data: &[u8]) -> Result<(usize, u64, u64), ElfError> {
    if data.len() < core::mem::size_of::<Elf64Header>() {
        return Err(ElfError::BadMagic);
    }

    let hdr = unsafe { &*(data.as_ptr() as *const Elf64Header) };
    validate_header(hdr)?;

    let pml4 = userspace::create_address_space().ok_or(ElfError::MapFailed)?;

    let phoff = hdr.e_phoff as usize;
    let phentsize = hdr.e_phentsize as usize;
    let phnum = hdr.e_phnum as usize;
    let mut loaded = false;

    for i in 0..phnum {
        let offset = phoff + i * phentsize;
        if offset + phentsize > data.len() {
            break;
        }
        let phdr = unsafe { &*(data.as_ptr().add(offset) as *const Elf64Phdr) };

        if phdr.p_type != PT_LOAD {
            continue;
        }

        let vaddr = phdr.p_vaddr as usize;
        let filesz = phdr.p_filesz as usize;
        let memsz = phdr.p_memsz as usize;
        let file_offset = phdr.p_offset as usize;
        let writable = phdr.p_flags & 2 != 0; // PF_W

        // Correct page range: from page-aligned start to page-aligned end
        let vaddr_page_start = vaddr & !0xFFF;
        let vaddr_end = vaddr + memsz;
        let pages = (vaddr_end - vaddr_page_start + PAGE_SIZE - 1) / PAGE_SIZE;

        // File data range in virtual address space
        let file_start = vaddr;
        let file_end = vaddr + filesz;

        for p in 0..pages {
            let page_vaddr = vaddr_page_start + p * PAGE_SIZE;
            let frame = pmm::alloc().ok_or(ElfError::MapFailed)?;
            userspace::map_user_page(pml4, page_vaddr, frame.address(), writable)
                .map_err(|_| ElfError::MapFailed)?;

            unsafe {
                // Zero the entire page first
                core::ptr::write_bytes(frame.address() as *mut u8, 0, PAGE_SIZE);

                // Compute intersection of [page_vaddr, page_vaddr+PAGE_SIZE) and
                // [file_start, file_end) for correct src/dst offsets
                let page_end = page_vaddr + PAGE_SIZE;
                if file_start < page_end && file_end > page_vaddr {
                    let copy_vstart = file_start.max(page_vaddr);
                    let copy_vend = file_end.min(page_end);
                    let copy_len = copy_vend - copy_vstart;
                    let dst_offset = copy_vstart - page_vaddr;
                    let src_offset = file_offset + (copy_vstart - vaddr);

                    if src_offset + copy_len <= data.len() {
                        core::ptr::copy_nonoverlapping(
                            data.as_ptr().add(src_offset),
                            (frame.address() + dst_offset) as *mut u8,
                            copy_len,
                        );
                    }
                }
            }
        }
        loaded = true;
    }

    if !loaded {
        return Err(ElfError::NoLoadSegments);
    }

    let stack_top = userspace::setup_user_stack(pml4).ok_or(ElfError::MapFailed)?;
    Ok((pml4, hdr.e_entry, stack_top))
}
