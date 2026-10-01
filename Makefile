# Quark — the kernel.
#
# This builds kernel.bin and the two flat modules the kernel loads itself, and
# installs them with the ABI: the system call numbers as a header and the
# document that says what they mean. What runs on the kernel is another
# repository (quarkutils), and nothing here knows where it is.

KERNEL := kernel.bin
TARGET := x86_64-unknown-none
BINARY := target/$(TARGET)/release/quark

GRUB_MKRESCUE := $(shell command -v grub-mkrescue 2>/dev/null || command -v grub2-mkrescue 2>/dev/null)

VGA_DRV_DIR := drivers/vga
VGA_DRV_ELF := $(VGA_DRV_DIR)/target/$(TARGET)/release/vga-driver
VGA_DRV_BIN := $(VGA_DRV_DIR)/vga.drv

FAT32_DRV_DIR := drivers/fat32
FAT32_DRV_ELF := $(FAT32_DRV_DIR)/target/$(TARGET)/release/fat32-driver
FAT32_DRV_BIN := $(FAT32_DRV_DIR)/fat32.drv

.PHONY: check-abi install all clean iso run run-uefi drivers FORCE

# `all` is not the first target in this file, so say which one is: plain `make`
# otherwise builds nothing but the ABI check, which passes and looks like a
# successful build of a tree that was never compiled.
.DEFAULT_GOAL := all

# One number per call, a row in docs/abi.md for every call, and a document
# that describes the version the kernel reports. First, because a kernel whose
# contract is wrong should not be built and handed to anybody.
check-abi:
	@./tools/check-abi.sh

all: check-abi $(KERNEL) drivers

$(KERNEL): FORCE
	cargo rustc --release -- -C link-arg=-Tlinker.ld
	cp $(BINARY) $(KERNEL)

drivers: $(VGA_DRV_BIN) $(FAT32_DRV_BIN)

$(VGA_DRV_BIN): FORCE
	cd $(VGA_DRV_DIR) && cargo build --release
	objcopy -O binary $(VGA_DRV_ELF) $(VGA_DRV_BIN)

$(FAT32_DRV_BIN): FORCE
	cd $(FAT32_DRV_DIR) && cargo build --release
	objcopy -O binary $(FAT32_DRV_ELF) $(FAT32_DRV_BIN)

# The kernel alone, under GRUB. There is no init on this image, so it boots as
# far as looking for one.
iso: $(KERNEL)
	@mkdir -p isodir/boot/grub
	@cp $(KERNEL) isodir/boot/kernel.bin
	@printf 'insmod all_video\nset timeout=0\nset default=0\n\nmenuentry "Quark" {\n\tmultiboot2 /boot/kernel.bin\n\tboot\n}\n' > isodir/boot/grub/grub.cfg
	$(GRUB_MKRESCUE) -o quark.iso isodir 2>/dev/null

# Boot via BIOS (legacy) GRUB
run: iso
	qemu-system-x86_64 -cdrom quark.iso

# Boot via UEFI GRUB (requires OVMF)
run-uefi: iso
	qemu-system-x86_64 -cdrom quark.iso \
		-drive if=pflash,format=raw,readonly=on,file=/usr/share/edk2/ovmf/OVMF_CODE.fd

# Stage build artifacts for whoever assembles an image out of them.
#
# Quark builds a kernel; it does not know what an image looks like, where one
# is mounted or what will run on it. `make install DESTDIR=<dir>` lays the
# artifacts out in the shape a distro consumes, and nothing here reaches into
# a sibling repo to put them somewhere.
#
#   $(DESTDIR)/kernel.bin
#   $(DESTDIR)/drivers/vga.drv, fat32.drv   loaded by the bootloader beside it
#   $(DESTDIR)/usr/include/quark/abi.h      the system call numbers
#   $(DESTDIR)/usr/share/doc/quark/abi.md   and what they mean
#
# The userland installs into the same directory from its own repository —
# init, the services, the programs — and the two do not overlap.
DESTDIR ?= dist

install: all
	@mkdir -p $(DESTDIR)/drivers
	@cp $(KERNEL) $(DESTDIR)/kernel.bin
	@cp $(VGA_DRV_BIN) $(FAT32_DRV_BIN) $(DESTDIR)/drivers/
	@# The ABI, which is what a kernel installs for the programs that will run
	@# on it: the numbers as a header generated from the source the dispatch is
	@# compiled from, and the document that says what each one means. A
	@# userland in another repository checks its own copy of the numbers
	@# against this, and has nothing else of the kernel's to look at.
	@mkdir -p $(DESTDIR)/usr/include/quark $(DESTDIR)/usr/share/doc/quark
	@./tools/gen-abi-header.sh > $(DESTDIR)/usr/include/quark/abi.h
	@cp docs/abi.md $(DESTDIR)/usr/share/doc/quark/abi.md
	@echo "installed to $(DESTDIR)"

clean:
	cargo clean
	cd $(VGA_DRV_DIR) && cargo clean
	cd $(FAT32_DRV_DIR) && cargo clean
	rm -rf $(KERNEL) $(VGA_DRV_BIN) $(FAT32_DRV_BIN) quark.iso isodir

FORCE:
