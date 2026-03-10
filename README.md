# Quark

A minimal x86-64 microkernel with user-space servers, IPC-backed stdio, and a growing set of OS primitives (virtual memory, futex/mutex, shared memory, networking).

## What it does

Quark boots via Multiboot2 (GRUB or the [Bang](https://github.com/MagicJester2764/bang) UEFI bootloader), transitions from 32-bit protected mode to 64-bit long mode, and provides a microkernel environment where most functionality runs in user space.

The kernel provides:

- Preemptive round-robin scheduling (100 Hz PIT timer)
- Synchronous IPC (send/recv/call/reply with fixed-size messages)
- Async notifications (seL4-style badge OR with multiplexed wait)
- Virtual address space creation and page mapping
- `sys_mmap` for user-space memory allocation (backs `Vec`, `String`, etc.)
- Per-task file descriptor table with IPC-backed read/write
- Capability-based access control (I/O ports, IRQs, physical memory mapping)
- Capability transfer over IPC (tasks can delegate caps without `CAP_TASK_MGMT`)
- IRQ delivery to user-space drivers
- Page fault forwarding to pager tasks (enables demand paging, COW, stack growth)
- Futex wait/wake for user-space synchronization
- Shared memory regions (create, grant, map across tasks)
- Per-task memory quotas
- Process wait (`sys_wait` for parent to collect child exit status)
- Program arguments (`argc`/`argv` via mapped argument page)
- Timed receive and tick counter for user-space timers/sleep

User space provides:

- **Init** (`user/init`) — Two-phase ELF loader: essential services from boot image, remaining programs from disk via GPT/FAT32. Passes program arguments, wires fds, grants capabilities, enforces sequential startup.
- **Nameserver** (`user/nameserver`) — service discovery via name registration/lookup
- **Console server** (`user/console`) — framebuffer text rendering via font8x8 with ANSI escape sequence support (cursor movement, colors, clear screen), serves write requests over IPC
- **Keyboard driver** (`user/keyboard`) — PS/2 scancode translation, IRQ 1 handling
- **Input server** (`user/input`) — line discipline (echo, backspace, newline) wrapping the keyboard driver
- **Disk driver** (`user/disk`) — ATA PIO disk driver (IRQ 14, read + write), registers as "disk" with nameserver
- **VFS** (`user/vfs`) — FAT32 filesystem service: read, write, create files/directories over IPC. Registers as "vfs" with nameserver.
- **Net** (`user/net`) — RTL8139 NIC driver with PCI enumeration, DMA ring buffers, Ethernet/ARP/IPv4/ICMP/UDP. Registers as "net" with nameserver. Client API in `libquark::net`.
- **Shell** (`user/shell`) — interactive command interpreter: reads input, parses commands, loads ELFs from `/usr/bin/` via VFS, spawns tasks with fd wiring, waits for exit
- **Echo** (`user/echo`) — prints arguments to stdout
- **Ls** (`user/ls`) — lists directory contents via VFS (default `/usr/bin`)
- **Cat** (`user/cat`) — reads and prints files via VFS
- **Hello** (`user/hello`) — test program that prints via `println!`
- **Disktest** (`user/disktest`) — reads sector 0 from disk and prints hex dump

## How it works

### Boot process

1. **GRUB or Bang** loads `kernel.bin`, `init.elf`, and `boot.img` as Multiboot2 modules
2. The 32-bit entry point (`boot.s`) sets up identity-mapped page tables (4 GiB via 2 MiB huge pages), enables long mode, and jumps to 64-bit Rust code
3. The kernel initializes the scheduler, IPC, IDT, PIT, syscalls, and physical memory manager
4. `spawn_init()` creates the init task with framebuffer info in a boot info page
5. **Phase 1** — Init mounts the FAT32 boot image and spawns essential services:
   - Pass 1: Nameserver (guarantees TID 2)
   - Pass 2: Console server (granted `CAP_MAP_PHYS`, receives framebuffer info via IPC)
   - Pass 3: Keyboard driver, disk driver (granted caps, fds wired to console)
   - Pass 4: Input server (fds wired, then stdin wired retroactively to prior tasks)
   - Boot image pages freed via `sys_phys_free`
6. **Phase 2** — Init loads remaining programs from disk:
   - Discovers the disk service via nameserver (with retry)
   - Parses GPT partition table to find the rootfs partition (falls back to MBR/raw FAT)
   - Navigates FAT32 directory tree to `/usr/bin/`
   - Spawns non-essential ELFs (hello, disktest, etc.) with caps and fd wiring
7. The kernel enters an idle HLT loop

### IPC

Fixed-size synchronous messages: sender TID, tag (u64), and 6 data words (48 bytes payload). `sys_call` combines send + receive-reply atomically using a `CallSendBlocked` state to prevent races between message pickup and reply delivery. `sys_recv_timeout` adds non-blocking poll and timed receive. `sys_notify` provides seL4-style async notifications: badge bits are OR'd into a per-task notification word and delivered as `TAG_NOTIFICATION` messages via `sys_recv`, enabling multiplexed wait over IPC + IRQs + notifications.

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
  ipc.rs              Synchronous IPC (send/recv/call/reply + async notifications)
  syscall.rs          Syscall dispatch (syscall/sysret via STAR/LSTAR)
  task.rs             Task struct, fd table, capabilities
  paging.rs           Page table management, huge page splitting
  pmm.rs              Physical memory manager (bitmap allocator)
  heap.rs             Kernel heap allocator
  sync.rs             IrqSpinLock<T> (interrupt-safe RAII spinlock)
  futex.rs            Futex wait/wake for user-space synchronization
  shmem.rs            Shared memory regions (create, grant, map)
  elf.rs              ELF binary parser
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
  libquark/           User-space library (syscalls, IPC, stdio, allocator, sync, vfs, net, args)
  init/               Init process (two-phase boot, GPT/FAT32 disk reader, service wiring)
  nameserver/         Service name registry
  console/            Framebuffer console server (font8x8)
  keyboard/           PS/2 keyboard driver (IRQ 1, scancode set 1)
  input/              Line-discipline input server
  disk/               ATA PIO disk driver (primary master, LBA28, read + write)
  vfs/                FAT32 filesystem service (read, write, create)
  net/                RTL8139 NIC driver (Ethernet, ARP, IPv4, ICMP, UDP)
  shell/              Interactive command interpreter
  echo/               Echo arguments to stdout
  ls/                 Directory listing via VFS
  cat/                File reader via VFS
  disktest/           Disk test program (reads and dumps sector 0)
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

To enable networking (RTL8139), add QEMU flags:

```bash
qemu-system-x86_64 -cdrom quark.iso -device rtl8139,netdev=n -netdev user,id=n
```

### With Bang bootloader (UEFI)

```bash
cd ../bang
make sync-quark   # Builds quark; copies essential ELFs to boot.img, others to rootfs/usr/bin/
make run          # Creates GPT disk image and boots via UEFI
```

## Disclaimer

This is primarily an AI-assisted experimental project, not a production kernel. It was built as a vehicle for exploring OS development concepts with AI tooling. Use at your own risk.
