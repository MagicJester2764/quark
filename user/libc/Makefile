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
OBJS    := src/start.o src/string.o src/quark.o src/io.o src/stdio.o src/stdlib.o

all: libquark.a

libquark.a: $(OBJS)
	$(AR) rcs $@ $(OBJS)

%.o: %.c
	$(CC) $(CFLAGS) -c -o $@ $<

clean:
	rm -f $(OBJS) libquark.a

.PHONY: all clean
