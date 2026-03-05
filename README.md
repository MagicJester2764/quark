# Quark

A minimal x86-64 microkernel with user-space servers and IPC-backed stdio.

## What it does

Quark boots via Multiboot2 (GRUB or the [Bang](https://github.com/MagicJester2764/bang) UEFI bootloader), transitions from 32-bit protected mode to 64-bit long mode, and provides a microkernel environment where most functionality runs in user space.

The kernel provides:

- Preemptive round-robin scheduling (100 Hz PIT timer)
- Synchronous IPC (send/recv/call/reply with fixed-size messages)
- Virtual address space creation and page mapping
- Per-task file descriptor table with IPC-backed read/write
- Capability-based access control (I/O ports, IRQs, physical memory mapping)
- IRQ delivery to user-space drivers

User space provides:

- **Init** (`user/init`) — ELF loader, reads FAT32 rootfs, spawns and wires all services
- **Nameserver** (`user/nameserver`) — service discovery via name registration/lookup
- **Console server** (`user/console`) — framebuffer text rendering via font8x8, serves write requests over IPC
- **Keyboard driver** (`user/keyboard`) — PS/2 scancode translation, IRQ 1 handling
- **Input server** (`user/input`) — line discipline (echo, backspace, newline) wrapping the keyboard driver
- **Hello** (`user/hello`) — test program that prints via `println!`

## How it works

### Boot process

1. **GRUB or Bang** loads `kernel.bin`, `init.elf`, and `boot.img` as Multiboot2 modules
2. The 32-bit entry point (`boot.s`) sets up identity-mapped page tables (4 GiB via 2 MiB huge pages), enables long mode, and jumps to 64-bit Rust code
3. The kernel initializes the scheduler, IPC, IDT, PIT, syscalls, and physical memory manager
4. `spawn_init()` creates the init task with framebuffer info in a boot info page
5. Init mounts the FAT32 boot image and spawns services in order:
   - Pass 1: Nameserver (guarantees TID 2)
   - Pass 2: Console server (granted `CAP_MAP_PHYS`, receives framebuffer info via IPC)
   - Pass 3: Remaining programs (fds wired to console before starting)
   - Pass 4: Input server (fds wired, then stdin wired retroactively to prior tasks)
6. The kernel enters an idle HLT loop

### IPC

Fixed-size synchronous messages: sender TID, tag (u64), and 6 data words (48 bytes payload). `sys_call` combines send + receive-reply atomically using a `CallSendBlocked` state to prevent races between message pickup and reply delivery.

### File descriptors

Each task has an 8-entry fd table. `SYS_FD_WRITE` and `SYS_FD_READ` route through IPC to the target service (e.g., console server for stdout). Init wires fds before starting tasks to prevent races. The kernel falls back to its own console for fd 1/2 if not connected (disabled after the console server takes over).

### Stdio in user space

`libquark` provides `print!`/`println!` macros backed by a 256-byte buffer that flushes as a single `sys_fd_write` call, ensuring each print is an atomic IPC round-trip. `read_line()` reads from fd 0 via `SYS_FD_READ`.

### Source layout

```
src/
  main.rs             Kernel entry, boot flow
  boot.s              32-to-64-bit bootstrap assembly
  scheduler.rs        Round-robin preemptive scheduler
  ipc.rs              Synchronous IPC (send/recv/call/reply)
  syscall.rs          Syscall dispatch (syscall/sysret via STAR/LSTAR)
  task.rs             Task struct, fd table, capabilities
  paging.rs           Page table management, huge page splitting
  pmm.rs              Physical memory manager (bitmap allocator)
  heap.rs             Kernel heap allocator
  idt.rs              Interrupt descriptor table
  pit.rs              PIT timer (100 Hz)
  pic.rs              8259 PIC driver
  irq_dispatch.rs     Per-IRQ ring buffers for user-space delivery
  userspace.rs        spawn_init, address space helpers
  context.rs          Task context switching
  multiboot2.rs       Multiboot2 tag parser
  modules.rs          Boot module registry
  fat32.rs            FAT32 driver interface (legacy)
  io.rs               Port I/O helpers
  services.rs         Service registry (legacy)
  console/
    mod.rs            Console abstraction (VGA / framebuffer)
    vga.rs            VGA text mode via driver vtable
    framebuffer.rs    Pixel framebuffer renderer

drivers/
  vga/                VGA text mode driver (flat binary)
  fat32/              FAT32 filesystem driver (flat binary)

user/
  libquark/           User-space library (syscalls, IPC, stdio macros)
  init/               Init process (ELF loader, FAT32 reader, service wiring)
  nameserver/         Service name registry
  console/            Framebuffer console server (font8x8)
  keyboard/           PS/2 keyboard driver (IRQ 1, scancode set 1)
  input/              Line-discipline input server
  hello/              Test program
```

## Building

### Dependencies

- **Rust nightly** with `x86_64-unknown-none` target, `rust-src`, and `llvm-tools-preview`
- **objcopy** (from binutils) for converting driver ELFs to flat binaries
- **GRUB** (`grub-mkrescue` or `grub2-mkrescue`) for ISO creation (optional)
- **QEMU** for testing

### Build commands

```bash
make all         # Build kernel, drivers, and all user programs
make drivers     # Build only drivers
make user        # Build only user-space programs
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
make sync-quark   # Builds quark and copies kernel + drivers + user programs
make run          # Boots via UEFI
```

## Disclaimer

This is primarily an AI-assisted experimental project, not a production kernel. It was built as a vehicle for exploring OS development concepts with AI tooling. Use at your own risk.
