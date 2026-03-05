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

# User-space programs (ELF binaries, not flat)
INIT_DIR := user/init
INIT_ELF := $(INIT_DIR)/target/$(TARGET)/release/init

HELLO_DIR := user/hello
HELLO_ELF := $(HELLO_DIR)/target/$(TARGET)/release/hello

NS_DIR := user/nameserver
NS_ELF := $(NS_DIR)/target/$(TARGET)/release/nameserver

KBD_DIR := user/keyboard
KBD_ELF := $(KBD_DIR)/target/$(TARGET)/release/keyboard

CON_DIR := user/console
CON_ELF := $(CON_DIR)/target/$(TARGET)/release/console

INP_DIR := user/input
INP_ELF := $(INP_DIR)/target/$(TARGET)/release/input

DISK_DIR := user/disk
DISK_ELF := $(DISK_DIR)/target/$(TARGET)/release/disk

DISKTEST_DIR := user/disktest
DISKTEST_ELF := $(DISKTEST_DIR)/target/$(TARGET)/release/disktest

.PHONY: all clean iso run run-uefi drivers user FORCE

all: $(KERNEL) drivers user

$(KERNEL): FORCE
	cargo build --release
	cp $(BINARY) $(KERNEL)

drivers: $(VGA_DRV_BIN) $(FAT32_DRV_BIN)

$(VGA_DRV_BIN): FORCE
	cd $(VGA_DRV_DIR) && cargo build --release
	objcopy -O binary $(VGA_DRV_ELF) $(VGA_DRV_BIN)

$(FAT32_DRV_BIN): FORCE
	cd $(FAT32_DRV_DIR) && cargo build --release
	objcopy -O binary $(FAT32_DRV_ELF) $(FAT32_DRV_BIN)

user: $(INIT_ELF) $(HELLO_ELF) $(NS_ELF) $(KBD_ELF) $(CON_ELF) $(INP_ELF) $(DISK_ELF) $(DISKTEST_ELF)

$(INIT_ELF): FORCE
	cd $(INIT_DIR) && cargo build --release

$(HELLO_ELF): FORCE
	cd $(HELLO_DIR) && cargo build --release

$(NS_ELF): FORCE
	cd $(NS_DIR) && cargo build --release

$(KBD_ELF): FORCE
	cd $(KBD_DIR) && cargo build --release

$(CON_ELF): FORCE
	cd $(CON_DIR) && cargo build --release

$(INP_ELF): FORCE
	cd $(INP_DIR) && cargo build --release

$(DISK_ELF): FORCE
	cd $(DISK_DIR) && cargo build --release

$(DISKTEST_ELF): FORCE
	cd $(DISKTEST_DIR) && cargo build --release

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

clean:
	cargo clean
	cd $(VGA_DRV_DIR) && cargo clean
	cd $(FAT32_DRV_DIR) && cargo clean
	cd $(INIT_DIR) && cargo clean
	cd $(HELLO_DIR) && cargo clean
	cd $(NS_DIR) && cargo clean
	cd $(KBD_DIR) && cargo clean
	cd $(CON_DIR) && cargo clean
	cd $(INP_DIR) && cargo clean
	cd $(DISK_DIR) && cargo clean
	cd $(DISKTEST_DIR) && cargo clean
	rm -rf $(KERNEL) $(VGA_DRV_BIN) $(FAT32_DRV_BIN) quark.iso isodir

FORCE:
