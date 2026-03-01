# Quark

A minimal x86-64 microkernel with loadable driver modules.

## What it does

Quark boots via Multiboot2 (GRUB or the [Bang](https://github.com/MagicJester2764/bang) UEFI bootloader), transitions from 32-bit protected mode to 64-bit long mode, and initializes a console for output. It supports loadable driver modules that are passed in as Multiboot2 boot modules by the bootloader.

Currently implemented:

- 32-to-64-bit bootstrap with identity-mapped page tables (4 GiB)
- Multiboot2 info parsing (framebuffer, memory map, boot modules)
- Boot module registry with name-based lookup
- VGA text mode console via a loadable driver (`vga.drv`)
- Pixel framebuffer console (for UEFI GOP displays)
- FAT32 filesystem driver (`fat32.drv`) for reading FAT32 images in memory

## How it works

### Boot process

1. **GRUB or Bang** loads `kernel.bin` and driver modules into memory
2. The 32-bit assembly entry point (`boot.s`) sets up page tables for identity-mapping the first 4 GiB using 2 MiB huge pages, enables long mode, and jumps to 64-bit Rust code
3. `kernel_main` initializes the module registry from Multiboot2 tags, finds the VGA driver module, and sets up console output
4. Boot modules and driver status are printed to the console

### Loadable drivers

Drivers are position-independent flat binaries with an entry function at offset 0. The kernel calls this entry function, which fills a `#[repr(C)]` vtable struct with function pointers. All subsequent calls go through the vtable.

Drivers are compiled with `-C relocation-model=pic` and converted from ELF to flat binary with `objcopy -O binary`. Function pointers in the vtable are computed at runtime using RIP-relative addressing.

### Source layout

```
src/
  main.rs             Kernel entry, boot flow
  boot.s              32-to-64-bit bootstrap assembly
  multiboot2.rs       Multiboot2 tag parser
  modules.rs          Boot module registry
  fat32.rs            FAT32 driver interface
  console/
    mod.rs            Console abstraction (VGA / framebuffer)
    vga.rs            VGA text mode via driver vtable
    framebuffer.rs    Pixel framebuffer renderer

drivers/
  vga/                VGA text mode driver (346 bytes)
  fat32/              FAT32 filesystem driver (2249 bytes)
```

## Building

### Dependencies

- **Rust nightly** with `x86_64-unknown-none` target, `rust-src`, and `llvm-tools-preview`
- **objcopy** (from binutils) for converting driver ELFs to flat binaries
- **GRUB** (`grub-mkrescue` or `grub2-mkrescue`) for ISO creation (optional)
- **QEMU** for testing

### Build commands

```bash
make all         # Build kernel and all drivers
make drivers     # Build only drivers
make iso         # Create bootable GRUB ISO
make run         # Build ISO and run in QEMU (BIOS/legacy)
make run-uefi    # Build ISO and run in QEMU (UEFI, needs OVMF)
make clean       # Remove all build artifacts
```

## Running

### Standalone (GRUB)

```bash
make run
```

### With Bang bootloader (UEFI)

```bash
cd ../bang
make sync-quark   # Builds quark and copies kernel + drivers
make run          # Boots via UEFI
```

## Disclaimer

This is primarily an AI-assisted experimental project, not a production kernel. It was built as a vehicle for exploring OS development concepts with AI tooling. Use at your own risk.
