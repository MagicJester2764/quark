/* Replacing a program with another one, which is what `exec` is.
 *
 * Quark loads programs in user space: a spawner reads an ELF, builds the image
 * in its own memory and moves the pages into the address space the new program
 * will run in. `quark_rt::spawn` does that in Rust for a *child*; this does the
 * same in C for the caller itself, and then asks the kernel for the one thing
 * user space cannot do — swap the address space under a running task.
 *
 * The task is the same task afterwards: same id, same descriptors, same
 * capabilities, same parent. What changes is the program, which is exactly
 * what `execve` promises.
 *
 * What does not survive is this layer's own open files. They live in the
 * memory being replaced, and the VFS knows them by the address space they were
 * opened from, which is the one going away. The kernel's descriptors — pipes,
 * ptys, sockets, shared memory — do survive, because they belong to the task.
 * A shell redirecting a child's output does it with those, so what a terminal
 * needs works; a program passing an open file across an exec does not, and
 * that is written down as a gap rather than half-done here.
 */

#include <quark/layout.h>
#include <quark/syscall.h>

#include "abi.h"

#define PAGE_SIZE 4096UL
#define EHDR_SIZE 64UL
#define PHDR_SIZE 56UL
#define PT_LOAD 1U
#define PT_PHDR 6U
#define MAX_SEGMENTS 8
/* As `quark_rt::spawn`: a program may span a gigabyte from its first page to
   its last, which is what has to be free at the staging address. */
#define MAX_IMAGE_SPAN (1UL << 30)
#define STACK_PAGES 256UL
#define MAP_CHUNK 256UL

/* Where the image, the stack and the argument page are built before they are
   moved: clear of the heap, of the layer's anonymous arena and of anything a
   program is loaded at, and a terabyte apart because an image may be a
   gigabyte wide. `layout.h` asserts they are user addresses. */
#define STAGE_ELF   QUARK_STAGE_ELF
#define STAGE_STACK QUARK_STAGE_STACK
#define STAGE_ARGS  QUARK_STAGE_ARGS

struct ehdr {
    unsigned char e_ident[16];
    unsigned short e_type;
    unsigned short e_machine;
    unsigned int e_version;
    unsigned long e_entry;
    unsigned long e_phoff;
    unsigned long e_shoff;
    unsigned int e_flags;
    unsigned short e_ehsize;
    unsigned short e_phentsize;
    unsigned short e_phnum;
    unsigned short e_shentsize;
    unsigned short e_shnum;
    unsigned short e_shstrndx;
};

struct phdr {
    unsigned int p_type;
    unsigned int p_flags;
    unsigned long p_offset;
    unsigned long p_vaddr;
    unsigned long p_paddr;
    unsigned long p_filesz;
    unsigned long p_memsz;
    unsigned long p_align;
};

struct segment {
    unsigned long first;   /* the page its first byte is in */
    unsigned long end;     /* one past the page its last byte is in */
    unsigned long vaddr;
    unsigned long vend;
    unsigned long offset;
    unsigned long filesz;
    int writable;
};

static void unmap_stage(unsigned long at, unsigned long pages) {
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        __syscall2(SYS_MUNMAP, at + done * PAGE_SIZE, n);
        done += n;
    }
}

/* Fresh, zeroed pages at `at`, or nothing at all. */
static int map_stage(unsigned long at, unsigned long pages) {
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        if (__syscall2(SYS_MMAP, at + done * PAGE_SIZE, n) == QUARK_ERR) {
            unmap_stage(at, done);
            return 0;
        }
        done += n;
    }
    return 1;
}

/* Move staged pages into the new address space. They are the new space's from
   then on: it frees them when it goes, and this one keeps no way to reach
   them. */
static int give(unsigned long cr3, unsigned long there, unsigned long here,
                unsigned long pages, int writable) {
    unsigned long done = 0;
    while (done < pages) {
        unsigned long n = pages - done;
        if (n > MAP_CHUNK) {
            n = MAP_CHUNK;
        }
        if (__syscall5(SYS_ADDRSPACE_GIVE, cr3, there + done * PAGE_SIZE,
                       here + done * PAGE_SIZE, n, writable ? 1UL : 0UL) == QUARK_ERR) {
            return 0;
        }
        done += n;
    }
    return 1;
}

/* Read the loadable segments, checked the way `quark_rt::spawn` checks them:
   in address order, apart, inside the file, and not spanning more than a
   gigabyte. Returns how many, or -1. */
static int read_segments(const struct phdr *ph, int phnum, unsigned long file_size,
                         struct segment *out) {
    int n = 0;
    for (int i = 0; i < phnum; i++) {
        if (ph[i].p_type != PT_LOAD || ph[i].p_memsz == 0) {
            continue;
        }
        unsigned long vaddr = ph[i].p_vaddr;
        unsigned long memsz = ph[i].p_memsz;
        unsigned long filesz = ph[i].p_filesz;
        unsigned long offset = ph[i].p_offset;
        if (filesz > memsz || offset + filesz < offset || offset + filesz > file_size) {
            return -1;
        }
        if (vaddr < QUARK_USER_MIN) {
            return -1; /* below PML4[1] is the kernel's, whatever the file says */
        }
        struct segment s;
        s.first = vaddr & ~(PAGE_SIZE - 1);
        s.end = (vaddr + memsz + PAGE_SIZE - 1) & ~(PAGE_SIZE - 1);
        s.vaddr = vaddr;
        s.vend = vaddr + memsz;
        s.offset = offset;
        s.filesz = filesz;
        s.writable = (ph[i].p_flags & 2) != 0;
        if (n > 0 && s.vaddr < out[n - 1].vend) {
            return -1;
        }
        unsigned long base = (n == 0) ? s.first : out[0].first;
        if (n == MAX_SEGMENTS || s.end - base > MAX_IMAGE_SPAN) {
            return -1;
        }
        out[n++] = s;
    }
    return n == 0 ? -1 : n;
}

/* Write the argument page: the arguments, then the environment, then the
   program's own header table at the end.
 *
 * The table is not optional. A program's headers are in no segment it loads,
 * and musl finds its thread-local template through them; without them every
 * thread-local in a C program lands outside its block. The end of the page
 * belongs to them however long the command line is. */
static void build_args(unsigned char *page, char *const argv[], char *const envp[],
                       const struct phdr *ph, int phnum) {
    unsigned long off = 0;
    for (int section = 0; section < 2; section++) {
        char *const *list = section == 0 ? argv : envp;
        unsigned long count_at = off;
        off += 8;
        unsigned long written = 0;
        for (int i = 0; list && list[i]; i++) {
            unsigned long len = 0;
            while (list[i][len]) {
                len++;
            }
            if (off + 8 + len > (unsigned long)QUARK_PHDRS_AT) {
                break;
            }
            *(unsigned long *)(page + off) = len;
            off += 8;
            for (unsigned long j = 0; j < len; j++) {
                page[off + j] = (unsigned char)list[i][j];
            }
            off += len;
            written++;
        }
        *(unsigned long *)(page + count_at) = written;
    }

    unsigned long at = (unsigned long)QUARK_PHDRS_AT;
    unsigned long table = QUARK_ARGS_PAGE + at + 16;
    int kept = phnum > QUARK_MAX_PHDRS ? QUARK_MAX_PHDRS : phnum;
    *(unsigned long *)(page + at) = PHDR_SIZE;
    *(unsigned long *)(page + at + 8) = (unsigned long)kept;
    for (int i = 0; i < kept; i++) {
        struct phdr *dst = (struct phdr *)(page + at + 16 + (unsigned long)i * PHDR_SIZE);
        *dst = ph[i];
        if (dst->p_type == PT_PHDR) {
            /* A C library takes the difference between AT_PHDR and this entry
               as the load base, and for a program that is not relocated it has
               to come out zero. Left as it was, every address derived from the
               headers would be off by the distance to this page. */
            dst->p_vaddr = table;
            dst->p_paddr = table;
        }
    }
}

/* Everything staged, nothing given: throw it away. */
static void drop_stage(const struct segment *segs, int n, unsigned long base,
                       int mapped_stack, int mapped_args) {
    for (int i = 0; i < n; i++) {
        unmap_stage(STAGE_ELF + (segs[i].first - base),
                    (segs[i].end - segs[i].first) / PAGE_SIZE);
    }
    if (mapped_stack) {
        unmap_stage(STAGE_STACK, STACK_PAGES);
    }
    if (mapped_args) {
        unmap_stage(STAGE_ARGS, 1);
    }
}

long __quark_execve(const char *path, char *const argv[], char *const envp[]) {
    if (!path || !path[0]) {
        return -LX_ENOENT;
    }

    /* The file first, and every check it can fail, before an address space
       exists to leak: `execl` returning an error is an ordinary path, and a
       program that carries on after one must be no worse off for having
       tried. */
    long fd = __quark_open(path, 0 /* O_RDONLY */);
    if (fd < 0) {
        return fd;
    }

    struct ehdr eh;
    if (__quark_pread(fd, &eh, sizeof eh, 0) != (long)sizeof eh) {
        __quark_close(fd);
        return -LX_ENOEXEC;
    }
    if (eh.e_ident[0] != 0x7F || eh.e_ident[1] != 'E' || eh.e_ident[2] != 'L' ||
        eh.e_ident[3] != 'F' || eh.e_ident[4] != 2 /* 64-bit */ ||
        eh.e_machine != 62 /* x86-64 */ || eh.e_phentsize < PHDR_SIZE ||
        eh.e_phnum == 0 || eh.e_phnum > QUARK_MAX_PHDRS) {
        __quark_close(fd);
        return -LX_ENOEXEC;
    }

    unsigned long file_size = (unsigned long)__quark_lseek(fd, 0, 2 /* SEEK_END */);

    struct phdr ph[QUARK_MAX_PHDRS];
    for (int i = 0; i < eh.e_phnum; i++) {
        long got = __quark_pread(fd, &ph[i], PHDR_SIZE,
                                 (long)(eh.e_phoff + (unsigned long)i * eh.e_phentsize));
        if (got != (long)PHDR_SIZE) {
            __quark_close(fd);
            return -LX_ENOEXEC;
        }
    }

    struct segment segs[MAX_SEGMENTS];
    int n = read_segments(ph, eh.e_phnum, file_size, segs);
    if (n < 0) {
        __quark_close(fd);
        return -LX_ENOEXEC;
    }
    unsigned long base = segs[0].first;

    /* Stage the image where the program will see it, laid out as it will see
       it: two segments that share a page write into the same staged page. */
    unsigned long mapped = base;
    for (int i = 0; i < n; i++) {
        unsigned long start = segs[i].first > mapped ? segs[i].first : mapped;
        if (start < segs[i].end) {
            if (!map_stage(STAGE_ELF + (start - base), (segs[i].end - start) / PAGE_SIZE)) {
                drop_stage(segs, i, base, 0, 0);
                __quark_close(fd);
                return -LX_ENOMEM;
            }
        }
        mapped = segs[i].end;
        /* Fresh pages are zeroed, so the .bss past the file bytes is done. */
        if (segs[i].filesz &&
            __quark_pread(fd, (void *)(STAGE_ELF + (segs[i].vaddr - base)),
                          segs[i].filesz, (long)segs[i].offset) != (long)segs[i].filesz) {
            drop_stage(segs, i + 1, base, 0, 0);
            __quark_close(fd);
            return -LX_ENOEXEC;
        }
    }
    __quark_close(fd);

    if (!map_stage(STAGE_STACK, STACK_PAGES)) {
        drop_stage(segs, n, base, 0, 0);
        return -LX_ENOMEM;
    }
    if (!map_stage(STAGE_ARGS, 1)) {
        drop_stage(segs, n, base, 1, 0);
        return -LX_ENOMEM;
    }
    build_args((unsigned char *)STAGE_ARGS, argv, envp, ph, eh.e_phnum);

    unsigned long cr3 = __syscall0(SYS_ADDRSPACE_CREATE);
    if (cr3 == QUARK_ERR) {
        drop_stage(segs, n, base, 1, 1);
        return -LX_ENOMEM;
    }

    int ok = 1;
    unsigned long given = base;
    for (int i = 0; i < n && ok; i++) {
        unsigned long start = segs[i].first > given ? segs[i].first : given;
        if (start >= segs[i].end) {
            continue;
        }
        /* A last page the next segment begins in has to suit both, so it goes
           on its own and writable if either wants it. */
        int shared = (i + 1 < n) && segs[i + 1].first < segs[i].end;
        unsigned long whole = shared ? segs[i].end - PAGE_SIZE : segs[i].end;
        if (whole > start) {
            ok = give(cr3, start, STAGE_ELF + (start - base),
                      (whole - start) / PAGE_SIZE, segs[i].writable);
        }
        if (ok && shared) {
            unsigned long last = segs[i].end - PAGE_SIZE;
            int writable = 0;
            for (int j = 0; j < n; j++) {
                if (segs[j].writable && segs[j].first <= last && last < segs[j].end) {
                    writable = 1;
                }
            }
            ok = give(cr3, last, STAGE_ELF + (last - base), 1, writable);
        }
        given = segs[i].end;
    }
    if (ok) {
        ok = give(cr3, QUARK_STACK_TOP - STACK_PAGES * PAGE_SIZE, STAGE_STACK,
                  STACK_PAGES, 1);
    }
    if (ok) {
        ok = give(cr3, QUARK_ARGS_PAGE, STAGE_ARGS, 1, 0);
    }
    if (!ok) {
        /* Whatever was given belongs to the new space, and destroying it frees
           exactly that; whatever was not is still ours to unmap. */
        __syscall1(SYS_ADDRSPACE_DESTROY, cr3);
        drop_stage(segs, n, base, 1, 1);
        return -LX_ENOMEM;
    }

    /* The last call this program makes. Everything above it was preparation
       that could fail and leave the caller as it was; this does not return. */
    __syscall3(SYS_EXEC_SPACE, cr3, eh.e_entry, QUARK_STACK_TOP);
    /* Only reached if the kernel refused, which means the space is not the
       caller's or has a task in it — neither of which can be true here. */
    __syscall1(SYS_ADDRSPACE_DESTROY, cr3);
    return -LX_ENOEXEC;
}
