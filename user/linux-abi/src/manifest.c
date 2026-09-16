/* What a hosted C program needs in order to be one: nothing.
 *
 * File data used to move through a page the program allocated and the VFS
 * mapped, so every program that might open a file asked for the capability to
 * allocate one — declared here, because the program only called `fopen` and
 * could not be expected to know. File data is lent to the VFS with each call
 * now, and nothing else this layer does takes a capability either.
 *
 * The object stays because the toolchain's specs file names it on every link
 * line. A program that does need a capability declares its own manifest.
 */

#include <quark/manifest.h>
