/* What a hosted C program needs in order to be one.
 *
 * File data moves through a page this program owns and the VFS maps, so a
 * program that opens a file needs a page it can allocate. The program does not
 * know that — it called `fopen` — and cannot be expected to declare it. The
 * library that does know declares it here, and the spawner grants it the same
 * way it grants everything else.
 *
 * This is a separate object rather than a member of the library because
 * nothing references it: it is linked for its section, not for its symbols,
 * so it goes on the link line beside the entry point.
 */

#include <quark/manifest.h>

QUARK_MANIFEST(QUARK_CAP_PHYS_ALLOC_N(8));
