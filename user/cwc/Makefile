# A C program for Quark, built against libquark.
#
# The linker script is the same one every Quark program uses: the base
# address, the entry section and the thread-local layout are properties of the
# system, not of the language a program is written in.

CC     ?= gcc
LIBC   := ../libc
CFLAGS := -ffreestanding -nostdlib -fno-builtin -fno-stack-protector \
          -fno-pic -no-pie -mcmodel=large -mno-red-zone \
          -std=c11 -O2 -Wall -Wextra -I$(LIBC)/include
LDFLAGS := -nostdlib -no-pie -Wl,-T,../linker.ld -Wl,--build-id=none

cwc: main.c $(LIBC)/libquark.a
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ main.c $(LIBC)/libquark.a

clean:
	rm -f cwc

.PHONY: clean
