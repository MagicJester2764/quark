/* Where this library puts things in the address space.
 *
 * Every one of these must be at or above QUARK_USER_MIN. Below it lies PML4[0],
 * whose page directories every address space shares: a mapping there is
 * promoted to user-accessible everywhere at once, and the kernel's own
 * identity map is in it. The kernel refuses such a mapping, so the mistake
 * shows up as a fault on first use rather than as a hole — but it is a hex
 * digit's worth of typo away at all times, so the checks below make it a
 * compile error instead. Two of these constants were written a digit short
 * the first time.
 */
#ifndef _QUARK_LAYOUT_H
#define _QUARK_LAYOUT_H

/* PML4[1]: 512 GiB. */
#define QUARK_USER_MIN 0x8000000000UL

/* Where the spawner maps the argument page, by convention with
   quark_rt::spawn. */
#define QUARK_ARGS_PAGE 0x8080000000UL
/* The program's own header table, at the end of the argument page: the entry
   size, the entry count, then the entries. Mirrors quark_rt::spawn::PHDRS_AT,
   and exists because a program's headers are not in any segment it loads —
   a C library finds its thread-local template through them. */
#define QUARK_MAX_PHDRS 16
#define QUARK_PHDR_SIZE 56
#define QUARK_PHDRS_AT (4096 - 16 - QUARK_MAX_PHDRS * QUARK_PHDR_SIZE)
/* The top of the user stack, which a program is started with. Mirrors
   quark_rt::spawn::STACK_TOP: the last page of PML4[255], which is the last
   page of user space. */
#define QUARK_STACK_TOP 0x7FFFFFFFF000UL
/* One past the last user address: PML4[256] and above is the kernel's. */
#define QUARK_USER_END 0x800000000000UL

/* Where `execve` builds the next program before moving it across. Twenty
   terabytes up, which is clear of the heap, of the layer's anonymous arena and
   of anything a program is loaded at — and the image may be a gigabyte wide,
   so they are a terabyte apart. */
#define QUARK_STAGE_ELF   0x140000000000UL
#define QUARK_STAGE_STACK 0x150000000000UL
#define QUARK_STAGE_ARGS  0x160000000000UL

/* Where malloc looks for pages. Deliberately not the Rust runtime's
   0x90_0000_0000: a program links one or the other, and picking the same
   address would turn a mistake into a confusing failure. */
#define QUARK_HEAP_START 0x9800000000UL
#define QUARK_HEAP_LIMIT 0xA000000000UL

_Static_assert(QUARK_ARGS_PAGE >= QUARK_USER_MIN, "args page is below PML4[1]");
_Static_assert(QUARK_HEAP_START >= QUARK_USER_MIN, "heap is below PML4[1]");
_Static_assert(QUARK_HEAP_LIMIT > QUARK_HEAP_START, "heap limit is below its start");
_Static_assert(QUARK_PHDRS_AT == 3184, "program header offset disagrees with quark_rt::spawn");
_Static_assert(QUARK_STACK_TOP < QUARK_USER_END, "stack top is not a user address");
_Static_assert(QUARK_STAGE_ELF >= QUARK_USER_MIN && QUARK_STAGE_ARGS < QUARK_USER_END,
               "exec staging is outside user space");

#endif
