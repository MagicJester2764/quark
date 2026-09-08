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
/* The page file data moves through, shared with the VFS. */
#define QUARK_XFER_PAGE 0x8900000000UL
/* Where malloc looks for pages. Deliberately not the Rust runtime's
   0x90_0000_0000: a program links one or the other, and picking the same
   address would turn a mistake into a confusing failure. */
#define QUARK_HEAP_START 0x9800000000UL
#define QUARK_HEAP_LIMIT 0xA000000000UL

_Static_assert(QUARK_ARGS_PAGE >= QUARK_USER_MIN, "args page is below PML4[1]");
_Static_assert(QUARK_XFER_PAGE >= QUARK_USER_MIN, "transfer page is below PML4[1]");
_Static_assert(QUARK_HEAP_START >= QUARK_USER_MIN, "heap is below PML4[1]");
_Static_assert(QUARK_HEAP_LIMIT > QUARK_HEAP_START, "heap limit is below its start");

#endif
