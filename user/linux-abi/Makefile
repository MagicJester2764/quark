# The Linux system call surface, answered by Quark.
#
# Built freestanding and against nothing: this sits *under* a C library, so it
# cannot use one. The only headers are Quark's own, which describe the calls it
# translates into.

CC      ?= gcc
AR      ?= ar
CFLAGS  := -ffreestanding -nostdlib -fno-builtin -fno-stack-protector \
           -fno-pic -no-pie -mcmodel=large -mno-red-zone \
           -std=c11 -O2 -Wall -Wextra -I../libc/include
# For a debugging build. `EXTRA_CFLAGS=-DQUARK_ABI_TRACE` makes an
# unimplemented call name itself on stderr instead of reaching the program as a
# bare errno, which is how you find out which one a port is missing.
CFLAGS  += $(EXTRA_CFLAGS)
# The VFS client is shared with Quark's own C library rather than written
# twice: it is the same wire protocol either way, and two copies of a wire
# protocol drift. Compiled here into an object of our own, though — building it
# in place would have two Makefiles writing one path with their own flags, and
# whichever ran last would decide what the other one linked.
OBJS    := src/syscall.o src/start.o src/files.o src/vfs-client.o
# Linked for its section rather than its symbols, so it cannot live in the
# archive: nothing references it and it would never be pulled in.
MANIFEST := src/manifest.o

all: liblinux-abi.a $(MANIFEST)

liblinux-abi.a: $(OBJS)
	rm -f $@
	$(AR) rcs $@ $(OBJS)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

src/vfs-client.o: ../libc/src/quark.c
	$(CC) $(CFLAGS) -c -o $@ $<

SYSROOT ?= $(HOME)/opt/cross/x86_64-quark/sys-root

install-sysroot: all
	mkdir -p $(SYSROOT)/usr/lib
	cp liblinux-abi.a $(SYSROOT)/usr/lib/
	cp $(MANIFEST) $(SYSROOT)/usr/lib/quark-libc-manifest.o

clean:
	rm -f $(OBJS) $(MANIFEST) liblinux-abi.a

.PHONY: all clean install-sysroot
