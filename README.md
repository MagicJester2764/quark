# Quark

A minimal x86-64 microkernel with user-space servers, IPC-backed stdio, multi-user login, and a growing set of OS primitives (virtual memory, pipes, futex/mutex, shared memory, networking).

## What it does

Quark boots via Multiboot2 (GRUB or the [Bang](https://github.com/MagicJester2764/bang) UEFI bootloader), transitions from 32-bit protected mode to 64-bit long mode, and provides a microkernel environment where most functionality runs in user space.

The kernel provides:

- Preemptive round-robin scheduling (100 Hz PIT timer)
- Synchronous IPC (send/recv/call/reply with fixed-size messages)
- Async notifications (seL4-style badge OR with multiplexed wait)
- Virtual address space creation and page mapping
- `sys_mmap` for user-space memory allocation (backs `Vec`, `String`, etc.)
- Per-task file descriptor table with IPC-backed read/write (blocking and non-blocking)
- Kernel pipes (anonymous byte-stream channels with ring buffer, blocking/non-blocking)
- File descriptor duplication (`sys_fd_dup`)
- Capability-based access control (I/O ports, IRQs, physical memory mapping, UID management)
- Capability transfer over IPC (tasks can delegate caps without `CAP_TASK_MGMT`)
- IRQ delivery to user-space drivers
- Page fault forwarding to pager tasks (enables demand paging, COW, stack growth)
- Futex wait/wake for user-space synchronization
- Shared memory regions (create, grant, map across tasks)
- Per-task memory quotas
- Per-task UID/GID with syscalls for get/set identity
- Task kill (`sys_task_kill`) and task info query (`sys_task_info`)
- Process wait (`sys_wait` for parent to collect child exit status)
- Program arguments (`argc`/`argv` via mapped argument page)
- Timed receive and tick counter for user-space timers/sleep

User space provides:

- **Init** (`user/init`) — Two-phase ELF loader: essential services from boot image, remaining programs from disk via GPT/FAT32. Passes program arguments, wires fds, grants capabilities, enforces sequential startup. Launches login (or shell as fallback).
- **Login** (`user/login`) — multi-user login program: prompts for username, reads `/etc/PASSWD`, sets UID/GID, spawns shell with user's home directory
- **Nameserver** (`user/nameserver`) — service discovery via name registration/lookup
- **Console server** (`user/console`) — framebuffer text rendering via font8x8 with ANSI escape sequence support (cursor movement, colors, clear screen), blinking underline cursor, pipe-based I/O transport
- **Keyboard driver** (`user/keyboard`) — PS/2 scancode translation, IRQ 1 handling
- **Input server** (`user/input`) — line discipline (echo, backspace, newline) wrapping the keyboard driver
- **Disk driver** (`user/disk`) — ATA PIO disk driver (read + write), multi-sector reads, registers as "disk" with nameserver
- **VFS** (`user/vfs`) — FAT32 filesystem service: read, write, create files/directories over IPC. Sector cache, trailing-slash validation. Registers as "vfs" with nameserver.
- **Net** (`user/net`) — RTL8139 NIC driver with PCI enumeration, DMA ring buffers, Ethernet/ARP/IPv4/ICMP/UDP. Registers as "net" with nameserver. Client API in `libquark::net`.
- **Shell** (`user/shell`) — interactive command interpreter with cwd tracking, `cd`/`pwd`/`kill` builtins, `~` home directory display in prompt, `.`/`..` path resolution, loads ELFs from `/usr/bin/` via VFS, spawns tasks with pipe-based fd wiring
- **Echo** (`user/echo`) — prints arguments to stdout
- **Ls** (`user/ls`) — lists directory contents or file info via VFS (defaults to cwd)
- **Cat** (`user/cat`) — reads and prints files via VFS
- **Ps** (`user/ps`) — lists running tasks with TID, state, UID, and parent TID
- **Ipcping** (`user/ipcping`) — measures IPC round-trip latency to named services via nameserver lookup
- **Hello** (`user/hello`) — test program that exercises heap allocation and sleep
- **Disktest** (`user/disktest`) — reads sector 0 from disk and prints hex dump

## How it works

### Boot process

1. **GRUB or Bang** loads `kernel.bin`, `init.elf`, and `boot.img` as Multiboot2 modules
2. The 32-bit entry point (`boot.s`) sets up identity-mapped page tables (4 GiB via 2 MiB huge pages), enables long mode, and jumps to 64-bit Rust code
3. The kernel initializes the scheduler, IPC, IDT, PIT, syscalls, and physical memory manager
4. `spawn_init()` creates the init task with framebuffer info in a boot info page
5. **Phase 1** — Init mounts the FAT32 boot image and spawns essential services:
   - Pass 1: Nameserver (guarantees TID 2)
   - Pass 2: Console server (granted `CAP_MAP_PHYS`, receives framebuffer info via IPC, pipe-based I/O)
   - Pass 3: Keyboard driver, disk driver (granted caps, stdout/stderr wired to console pipe)
   - Pass 4: Input server (stdin wired retroactively to prior tasks)
   - Pass 5: VFS (loaded but deferred start until disk is available)
   - Boot image pages freed via `sys_phys_free`
6. **Phase 2** — Init starts VFS and loads the login program:
   - Starts VFS (loaded in Phase 1), waits for it to register with nameserver
   - VFS discovers disk service and serves the FAT32 rootfs
   - Init loads LOGIN.ELF from `/usr/bin/` via VFS (falls back to SHELL.ELF)
   - Grants capabilities, wires fds, starts the login/shell task
7. The kernel enters an idle HLT loop

### IPC

Fixed-size synchronous messages: sender TID, tag (u64), and 6 data words (48 bytes payload). `sys_call` combines send + receive-reply atomically using a `CallSendBlocked` state to prevent races between message pickup and reply delivery. `sys_recv_timeout` adds non-blocking poll and timed receive. `sys_notify` provides seL4-style async notifications: badge bits are OR'd into a per-task notification word and delivered as `TAG_NOTIFICATION` messages via `sys_recv`, enabling multiplexed wait over IPC + IRQs + notifications.

### File descriptors

Each task has an 8-entry fd table. Fds can point to IPC targets or kernel pipes. `SYS_FD_WRITE` and `SYS_FD_READ` route through IPC or pipe read/write as appropriate. `SYS_FD_READ_NB` provides non-blocking reads (returns `WOULD_BLOCK` if no data available). `SYS_FD_DUP` duplicates descriptors. Init wires fds before starting tasks to prevent races. Console I/O uses pipe-based transport for efficient buffering.

### Stdio in user space

`libquark` provides `print!`/`println!` macros backed by a 256-byte buffer that flushes as a single `sys_fd_write` call. When fds are backed by pipes, writes go directly into the pipe ring buffer; when backed by IPC targets, the kernel chunks the data into 40-byte IPC messages. `read_line()` reads from fd 0 via `SYS_FD_READ`.

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
  pipe.rs             Kernel pipes (anonymous byte-stream ring buffers)
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
  libquark/           User-space library (syscalls, IPC, stdio, allocator, sync, vfs, net, passwd, args)
  init/               Init process (two-phase boot, GPT/FAT32 disk reader, service wiring)
  login/              Multi-user login program (passwd lookup, UID/GID, shell spawning)
  nameserver/         Service name registry
  console/            Framebuffer console server (font8x8, blinking cursor, pipe I/O)
  keyboard/           PS/2 keyboard driver (IRQ 1, scancode set 1)
  input/              Line-discipline input server
  disk/               ATA PIO disk driver (primary master, LBA28, read + write, multi-sector, yield-polling)
  vfs/                FAT32 filesystem service (read, write, create, sector cache)
  net/                RTL8139 NIC driver (Ethernet, ARP, IPv4, ICMP, UDP)
  shell/              Interactive command interpreter (cd, pwd, kill, path resolution)
  echo/               Echo arguments to stdout
  ls/                 Directory listing and file info via VFS
  cat/                File reader via VFS
  ps/                 Task list (TID, state, UID, parent)
  ipcping/            IPC latency measurement tool
  disktest/           Disk test program (reads and dumps sector 0)
  hello/              Test program (heap allocation, sleep)
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
