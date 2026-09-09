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
# The VFS client is shared with Quark's own C library rather than written
# twice: it is the same wire protocol either way, and two copies of a wire
# protocol drift.
OBJS    := src/syscall.o src/start.o src/files.o ../libc/src/quark.o

all: liblinux-abi.a

liblinux-abi.a: $(OBJS)
	rm -f $@
	$(AR) rcs $@ $(OBJS)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

SYSROOT ?= $(HOME)/opt/cross/x86_64-quark/sys-root

install-sysroot: all
	mkdir -p $(SYSROOT)/usr/lib
	cp liblinux-abi.a $(SYSROOT)/usr/lib/

clean:
	rm -f $(OBJS) liblinux-abi.a

.PHONY: all clean install-sysroot
