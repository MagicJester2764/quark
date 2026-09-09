KERNEL := kernel.bin
TARGET := x86_64-unknown-none
BINARY := target/$(TARGET)/release/quark

# Hosted target (requires std fork at QUARK_RUST_STD_PATH)
HOSTED_TARGET := x86_64-unknown-quark
QUARK_RUST_STD_PATH ?= $(CURDIR)/../rust/library

# Programs that need the std fork next door. Built when the fork is present and
# skipped when it is not, so this tree stands alone: a kernel should not require
# a patched rustc checkout to compile at all.
HOSTED_PROGRAMS := hello httpget
HAVE_STD_FORK := $(wildcard $(QUARK_RUST_STD_PATH)/std/Cargo.toml)
ifeq ($(HAVE_STD_FORK),)
HOSTED_ELFS :=
else
HOSTED_ELFS := $(foreach p,$(HOSTED_PROGRAMS),user/$(p)/target/$(HOSTED_TARGET)/release/$(p))
endif
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

NS_DIR := user/nameserver
NS_ELF := $(NS_DIR)/target/$(TARGET)/release/nameserver

KBD_DIR := user/keyboard
KBD_ELF := $(KBD_DIR)/target/$(TARGET)/release/keyboard

CON_DIR := user/console
FB_DIR := user/fb
WM_DIR := user/wm
WMDEMO_DIR := user/wmdemo
WMTYPE_DIR := user/wmtype
CON_ELF := $(CON_DIR)/target/$(TARGET)/release/console
FB_ELF := $(FB_DIR)/target/$(TARGET)/release/fb
WM_ELF := $(WM_DIR)/target/$(TARGET)/release/wm
WMDEMO_ELF := $(WMDEMO_DIR)/target/$(TARGET)/release/wmdemo
WMTYPE_ELF := $(WMTYPE_DIR)/target/$(TARGET)/release/wmtype

INP_DIR := user/input
INP_ELF := $(INP_DIR)/target/$(TARGET)/release/input

DISK_DIR := user/disk
DISK_ELF := $(DISK_DIR)/target/$(TARGET)/release/disk

DISKTEST_DIR := user/disktest
DISKTEST_ELF := $(DISKTEST_DIR)/target/$(TARGET)/release/disktest

VFS_DIR := user/vfs
VFS_ELF := $(VFS_DIR)/target/$(TARGET)/release/vfs

NET_DIR := user/net
NET_ELF := $(NET_DIR)/target/$(TARGET)/release/net

SHELL_DIR := user/shell
SHELL_ELF := $(SHELL_DIR)/target/$(TARGET)/release/shell

ECHO_DIR := user/echo
ECHO_ELF := $(ECHO_DIR)/target/$(TARGET)/release/echo

LS_DIR := user/ls
LS_ELF := $(LS_DIR)/target/$(TARGET)/release/ls

CAT_DIR := user/cat
CAPDEMO_DIR := user/capdemo
THREADTEST_DIR := user/threadtest
SOCKTEST_DIR := user/socktest
FSTEST_DIR := user/fstest

# C programs, built against the C library rather than quark-rt.
LIBC_DIR := user/libc
CWC_DIR := user/cwc
CAT_ELF := $(CAT_DIR)/target/$(TARGET)/release/cat
CAPDEMO_ELF := $(CAPDEMO_DIR)/target/$(TARGET)/release/capdemo
THREADTEST_ELF := $(THREADTEST_DIR)/target/$(TARGET)/release/threadtest
SOCKTEST_ELF := $(SOCKTEST_DIR)/target/$(TARGET)/release/socktest
FSTEST_ELF := $(FSTEST_DIR)/target/$(TARGET)/release/fstest
LIBC_A := $(LIBC_DIR)/libquark.a
CWC_ELF := $(CWC_DIR)/cwc

LOGIN_DIR := user/login
LOGIN_ELF := $(LOGIN_DIR)/target/$(TARGET)/release/login

PS_DIR := user/ps
PS_ELF := $(PS_DIR)/target/$(TARGET)/release/ps

IPCPING_DIR := user/ipcping
IPCPING_ELF := $(IPCPING_DIR)/target/$(TARGET)/release/ipcping

PING_DIR := user/ping
PING_ELF := $(PING_DIR)/target/$(TARGET)/release/ping

SHUTDOWN_DIR := user/shutdown
SHUTDOWN_ELF := $(SHUTDOWN_DIR)/target/$(TARGET)/release/shutdown

.PHONY: check-abi install all clean iso run run-uefi drivers user rootfs FORCE

# `all` is not the first target in this file, so say which one is: plain `make`
# otherwise builds nothing but the ABI check, which passes and looks like a
# successful build of a tree that was never compiled.
.DEFAULT_GOAL := all

check-abi:
	@./tools/check-abi.sh

all: check-abi $(KERNEL) drivers user rootfs
ifeq ($(HAVE_STD_FORK),)
	@echo "note: no std fork at $(QUARK_RUST_STD_PATH); skipped $(HOSTED_PROGRAMS)"
endif

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

user: $(INIT_ELF) $(HOSTED_ELFS) $(NS_ELF) $(KBD_ELF) $(CON_ELF) $(FB_ELF) $(WM_ELF) $(INP_ELF) $(DISK_ELF) $(DISKTEST_ELF) $(VFS_ELF) $(NET_ELF) $(SHELL_ELF) $(ECHO_ELF) $(LS_ELF) $(CAT_ELF) $(LOGIN_ELF) $(PS_ELF) $(IPCPING_ELF) $(PING_ELF) $(SHUTDOWN_ELF) $(CAPDEMO_ELF) $(THREADTEST_ELF) $(SOCKTEST_ELF) $(FSTEST_ELF) $(WMDEMO_ELF) $(WMTYPE_ELF) $(CWC_ELF)

$(INIT_ELF): FORCE
	cd $(INIT_DIR) && cargo build --release

# quark-rt reaches a hosted binary only through the fork's library/Cargo.toml
# patch, and `cargo -Z build-std` does not propagate that dependency into its
# fingerprints: editing quark-rt leaves the program linked against the previous
# copy, and cargo reports "Finished" without rebuilding. That silently produced
# a hello carrying the pre-Phase-0 syscall numbers while the kernel had moved to
# the new ones, which faulted as #UD out of the alloc error handler.
#
# Hash the quark-rt sources and clean the hosted build when they change. std
# genuinely has to be recompiled in that case — it links quark-rt — so the cost
# is inherent, not overhead. The stamp is written only after a successful
# build, so an interrupted one does not mark itself current.
# The whole of the fork's `sys` tree, not just its quark-named files. Listing
# those by hand missed sys/net/connection/mod.rs, which is where a platform is
# routed to its own module: adding Quark there changed nothing, cargo reported
# "Finished", and httpget went on linking std's `unsupported` socket stubs —
# compiling perfectly and failing at run time.
QUARK_RT_SRCS := $(wildcard user/quark-rt/src/*.rs) user/quark-rt/Cargo.toml \
                 $(shell find $(QUARK_RUST_STD_PATH)/std/src/sys -name '*.rs' 2>/dev/null | sort)

# One recipe, instantiated per hosted program. A pattern rule cannot do this:
# the program name appears twice in the path, and make allows a single % in a
# target. The program name is also its directory and its binary, so $(1) is the
# only thing that varies.
define HOSTED_BUILD_RULE
user/$(1)/target/$$(HOSTED_TARGET)/release/$(1): FORCE
	@new=`cat $$(QUARK_RT_SRCS) | md5sum | cut -d' ' -f1`; \
	 old=`cat user/$(1)/target/.quark-rt-stamp 2>/dev/null || echo none`; \
	 if [ "$$$$new" != "$$$$old" ]; then \
	   echo "  quark-rt changed since the last hosted build - cleaning std for $(1)"; \
	   (cd user/$(1) && cargo clean); \
	 fi
	cd user/$(1) && __CARGO_TESTS_ONLY_SRC_ROOT=$$(realpath $$(QUARK_RUST_STD_PATH)) cargo build --release --target ../../x86_64-unknown-quark.json -Z build-std=std,panic_abort -Z build-std-features=compiler-builtins-mem -Z json-target-spec
	@cat $$(QUARK_RT_SRCS) | md5sum | cut -d' ' -f1 > user/$(1)/target/.quark-rt-stamp
endef

$(foreach p,$(HOSTED_PROGRAMS),$(eval $(call HOSTED_BUILD_RULE,$(p))))

$(NS_ELF): FORCE
	cd $(NS_DIR) && cargo build --release

$(KBD_ELF): FORCE
	cd $(KBD_DIR) && cargo build --release

$(CON_ELF): FORCE
	cd $(CON_DIR) && cargo build --release

$(FB_ELF): FORCE
	cd $(FB_DIR) && cargo build --release

$(WM_ELF): FORCE
	cd $(WM_DIR) && cargo build --release

$(WMDEMO_ELF): FORCE
	cd $(WMDEMO_DIR) && cargo build --release

$(WMTYPE_ELF): FORCE
	cd $(WMTYPE_DIR) && cargo build --release

$(INP_ELF): FORCE
	cd $(INP_DIR) && cargo build --release

$(DISK_ELF): FORCE
	cd $(DISK_DIR) && cargo build --release

$(DISKTEST_ELF): FORCE
	cd $(DISKTEST_DIR) && cargo build --release

$(VFS_ELF): FORCE
	cd $(VFS_DIR) && cargo build --release

$(NET_ELF): FORCE
	cd $(NET_DIR) && cargo build --release

$(SHELL_ELF): FORCE
	cd $(SHELL_DIR) && cargo build --release

$(ECHO_ELF): FORCE
	cd $(ECHO_DIR) && cargo build --release

$(LS_ELF): FORCE
	cd $(LS_DIR) && cargo build --release

$(CAT_ELF): FORCE
	cd $(CAT_DIR) && cargo build --release

$(CAPDEMO_ELF): FORCE
	cd $(CAPDEMO_DIR) && cargo build --release

$(THREADTEST_ELF): FORCE
	cd $(THREADTEST_DIR) && cargo build --release

$(SOCKTEST_ELF): FORCE
	cd $(SOCKTEST_DIR) && cargo build --release

$(FSTEST_ELF): FORCE
	cd $(FSTEST_DIR) && cargo build --release

# The C library, and a C program built against it. A libc is a consumer of the
# Quark ABI exactly as the Rust runtime is; neither is privileged over the
# other, and both are built here.
$(LIBC_A): FORCE
	$(MAKE) -C $(LIBC_DIR)

$(CWC_ELF): $(LIBC_A) FORCE
	$(MAKE) -C $(CWC_DIR)

$(LOGIN_ELF): FORCE
	cd $(LOGIN_DIR) && cargo build --release

$(PS_ELF): FORCE
	cd $(PS_DIR) && cargo build --release

$(IPCPING_ELF): FORCE
	cd $(IPCPING_DIR) && cargo build --release

$(PING_ELF): FORCE
	cd $(PING_DIR) && cargo build --release

$(SHUTDOWN_ELF): FORCE
	cd $(SHUTDOWN_DIR) && cargo build --release

rootfs:
	@mkdir -p rootfs/etc
	@echo 'root:0:0:/home/root:/usr/bin/SHELL.ELF' > rootfs/etc/passwd

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
# Quark builds a kernel and the programs that run on it; it does not know what
# an image looks like or where one is mounted. `make install DESTDIR=<dir>`
# lays the artifacts out in the shape a distro consumes, and nothing here
# reaches into a sibling repo to put them somewhere.
#
#   $(DESTDIR)/kernel.bin
#   $(DESTDIR)/drivers/      loaded by the bootloader from the ESP
#   $(DESTDIR)/boot/         essential services, staged into boot.img
#   $(DESTDIR)/usr/bin/      everything else, staged into the root filesystem
#   $(DESTDIR)/etc/
DESTDIR ?= dist

BOOT_SERVICES := nameserver:NAMESRVR keyboard:KEYBOARD console:CONSOLE \
                 input:INPUT disk:DISK vfs:VFS net:NET fb:FB
USR_PROGRAMS  := disktest:DISKTEST shell:SHELL echo:ECHO ls:LS cat:CAT \
                 login:LOGIN ps:PS ipcping:IPCPING ping:PING \
                 shutdown:SHUTDOWN capdemo:CAPDEMO threadtest:THREADTEST socktest:SOCKTEST fstest:FSTEST wm:WM wmdemo:WMDEMO wmtype:WMTYPE

# Programs written in C, built against user/libc.
C_PROGRAMS    := cwc:CWC

install: all
	@mkdir -p $(DESTDIR)/drivers $(DESTDIR)/boot $(DESTDIR)/usr/bin $(DESTDIR)/etc
	@cp $(KERNEL) $(DESTDIR)/kernel.bin
	@cp $(VGA_DRV_BIN) $(FAT32_DRV_BIN) $(DESTDIR)/drivers/
	@cp $(INIT_ELF) $(DESTDIR)/drivers/init.elf
	@for p in $(BOOT_SERVICES); do \
		src=$${p%%:*}; dst=$${p##*:}; \
		cp user/$$src/target/$(TARGET)/release/$$src $(DESTDIR)/boot/$$dst.ELF; \
	done
	@for p in $(USR_PROGRAMS); do \
		src=$${p%%:*}; dst=$${p##*:}; \
		cp user/$$src/target/$(TARGET)/release/$$src $(DESTDIR)/usr/bin/$$dst.ELF; \
	done
	@# C programs are not cargo crates, so their binaries sit beside their
	@# sources rather than under a target directory.
	@for p in $(C_PROGRAMS); do \
		src=$${p%%:*}; dst=$${p##*:}; \
		cp user/$$src/$$src $(DESTDIR)/usr/bin/$$dst.ELF; \
	done
	@# Gate on the fork, not on the files: a hosted binary left over from an
	@# earlier build cannot be shown to match the current tree, and shipping a
	@# stale one is how hello ended up calling pre-Phase-0 syscall numbers.
ifeq ($(HAVE_STD_FORK),)
	@echo "  (no std fork - $(HOSTED_PROGRAMS) omitted rather than shipped stale)"
else
	@for p in $(HOSTED_PROGRAMS); do \
	   cp user/$$p/target/$(HOSTED_TARGET)/release/$$p \
	      $(DESTDIR)/usr/bin/`echo $$p | tr a-z A-Z`.ELF; \
	 done
endif
	@cp rootfs/etc/passwd $(DESTDIR)/etc/PASSWD
	@echo "installed to $(DESTDIR)"

clean:
	cargo clean
	cd $(VGA_DRV_DIR) && cargo clean
	cd $(FAT32_DRV_DIR) && cargo clean
	cd $(INIT_DIR) && cargo clean
	cd $(NS_DIR) && cargo clean
	cd $(KBD_DIR) && cargo clean
	cd $(CON_DIR) && cargo clean
	cd $(INP_DIR) && cargo clean
	cd $(DISK_DIR) && cargo clean
	cd $(DISKTEST_DIR) && cargo clean
	cd $(VFS_DIR) && cargo clean
	cd $(NET_DIR) && cargo clean
	cd $(SHELL_DIR) && cargo clean
	cd $(ECHO_DIR) && cargo clean
	cd $(LS_DIR) && cargo clean
	cd $(CAT_DIR) && cargo clean
	cd $(LOGIN_DIR) && cargo clean
	cd $(PS_DIR) && cargo clean
	cd $(IPCPING_DIR) && cargo clean
	cd $(PING_DIR) && cargo clean
	cd $(SHUTDOWN_DIR) && cargo clean
	@for p in $(HOSTED_PROGRAMS); do (cd user/$$p && cargo clean); done
	$(MAKE) -C $(LIBC_DIR) clean
	$(MAKE) -C $(CWC_DIR) clean
	rm -rf $(KERNEL) $(VGA_DRV_BIN) $(FAT32_DRV_BIN) quark.iso isodir

FORCE:
