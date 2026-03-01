KERNEL := kernel.bin
TARGET := x86_64-unknown-none
BINARY := target/$(TARGET)/release/quark
GRUB_MKRESCUE := $(shell command -v grub-mkrescue 2>/dev/null || command -v grub2-mkrescue 2>/dev/null)

VGA_DRV_DIR := drivers/vga
VGA_DRV_ELF := $(VGA_DRV_DIR)/target/$(TARGET)/release/vga-driver
VGA_DRV_BIN := $(VGA_DRV_DIR)/vga.drv

.PHONY: all clean iso run run-uefi drivers FORCE

all: $(KERNEL) drivers

$(KERNEL): FORCE
	cargo build --release
	cp $(BINARY) $(KERNEL)

drivers: $(VGA_DRV_BIN)

$(VGA_DRV_BIN): FORCE
	cd $(VGA_DRV_DIR) && cargo build --release
	objcopy -O binary $(VGA_DRV_ELF) $(VGA_DRV_BIN)

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
	rm -rf $(KERNEL) $(VGA_DRV_BIN) quark.iso isodir

FORCE:
