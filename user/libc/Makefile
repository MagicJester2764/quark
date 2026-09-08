# A C library for Quark.
#
# Built with the host compiler rather than a cross one: Quark is x86-64 and
# the host is too, so `-ffreestanding -nostdlib` plus our own headers is
# already a cross-compilation in every way that matters. A real cross
# toolchain buys a target triple and a sysroot, which matter for porting
# other people's build systems and not for building this.

CC      ?= gcc
AR      ?= ar
CFLAGS  := -ffreestanding -nostdlib -fno-builtin -fno-stack-protector \
           -fno-pic -no-pie -mcmodel=large -mno-red-zone \
           -std=c11 -O2 -Wall -Wextra -Iinclude
# The entry point is a file of its own, not a member of the archive: a
# toolchain links crt0 first and pulls the rest out of the library only as it
# is referenced, which is the difference between `_start` always being there
# and it being there when something happens to mention it.
OBJS    := src/string.o src/quark.o src/io.o src/stdio.o src/stdlib.o
CRT0    := src/start.o

all: libquark.a $(CRT0)

# Built from nothing each time: `ar r` replaces members and adds new ones, but
# never drops one that is no longer listed, so an object that moves out of the
# library stays in it until somebody notices the duplicate symbol.
libquark.a: $(OBJS)
	rm -f $@
	$(AR) rcs $@ $(OBJS)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

# Where the cross toolchain looks for all of this. Installing here is what
# turns "a library in this tree" into "the C library of the x86_64-quark
# target", which is the difference between building our own programs and
# building somebody else's.
SYSROOT ?= $(HOME)/opt/cross/x86_64-quark/sys-root

install-sysroot: all
	mkdir -p $(SYSROOT)/usr/include $(SYSROOT)/usr/lib
	cp -r include/* $(SYSROOT)/usr/include/
	# stddef and stdarg belong to the compiler; it ships better ones and its
	# own come first in the search order anyway.
	rm -f $(SYSROOT)/usr/include/stddef.h $(SYSROOT)/usr/include/stdarg.h
	cp libquark.a $(SYSROOT)/usr/lib/libc.a
	cp $(CRT0) $(SYSROOT)/usr/lib/crt0.o
	cp ../linker.ld $(SYSROOT)/usr/lib/quark.ld

clean:
	rm -f $(OBJS) $(CRT0) libquark.a

.PHONY: all clean install-sysroot
