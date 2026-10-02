# Quark

A minimal x86-64 microkernel. This repository is the kernel and nothing else:
what runs on it is [quarkutils](https://github.com/MagicJester2764/quarkutils),
what boots it is [Bang](https://github.com/MagicJester2764/bang), and what
assembles the three into an image is
[ExplOSion](https://github.com/MagicJester2764/explosion) — or
[GNU/Quark](https://github.com/MagicJester2764/gnu-quark), which puts GNU's
shell and programs on this kernel in place of Quark's own.

## What it does

Quark boots via Multiboot2 (GRUB or the Bang UEFI bootloader), moves from
32-bit protected mode to 64-bit long mode, starts one program — `init`, handed
to it as a boot module — and after that provides a small set of primitives.
Everything else is a program: the drivers, the filesystems, the network stack,
the display, the shell.

The kernel provides:

- Preemptive scheduling in four bands (100 Hz PIT), where a synchronous call
  hands the CPU straight to its callee and a task runs at the band of whoever
  is waiting on it
- Synchronous IPC — send, receive, call and reply with fixed-size messages —
  plus notifications, deadlines, and buffers lent with a call so that no server
  has to map a client's memory
- Object capabilities as the only authority: I/O ports, IRQs, physical ranges,
  endpoints, task management. There is no UID 0 bypass
- Address spaces, memory given its frames when first touched, shared memory,
  and memory objects whose pages a user-space pager provides — which is how a
  file is mapped
- A per-task descriptor table: IPC endpoints, pipes, connected streams that
  can carry descriptors, poll sets, pseudo-terminals, timers and event counters
- Tasks, threads, `fork` and `exec`; futexes with deadlines
- IRQ delivery to user-space drivers, and page faults forwarded to a pager
- Random bytes (ChaCha20, seeded from RDSEED or RDRAND and the machine's
  timing) and the date

## The ABI

[`docs/abi.md`](docs/abi.md) is the contract: every system call, what it takes,
what it returns and what it asks for. Numbers are assigned in blocks of
sixteen, one subsystem to a block, and the whole is versioned — a running
kernel answers `SYS_ABI_VERSION` (240) with `(major << 16) | minor`.

The numbers are written in one place, `src/syscall.rs`, which is what the
dispatch is compiled from. `make install` derives a header from them and
installs it with the document:

```
usr/include/quark/abi.h       the numbers, and the version
usr/share/doc/quark/abi.md    what they mean
```

Those two files are what a userland in another repository builds against.
`tools/check-abi.sh` runs first in every build and fails unless every call has
a row in the document, no two calls share a number, and the version the
document describes is the one the kernel reports.

## Source layout

```
src/
  main.rs             Kernel entry, boot flow
  boot.s              32-to-64-bit bootstrap assembly
  syscall.rs          System call numbers and dispatch (syscall/sysret)
  scheduler.rs        Preemptive scheduler: four bands, round-robin within one
  task.rs             Task struct, descriptor table, capability space
  context.rs          Task context switching
  fpu.rs              Floating-point and SSE state, one copy per task
  ipc.rs              Synchronous IPC and notifications
  lend.rs             Memory lent with a call
  cap.rs              Object capabilities
  paging.rs           Page tables, address spaces, memory on demand
  pmm.rs              Physical memory manager (bitmap allocator)
  heap.rs             Kernel heap
  memobj.rs           Memory objects: pages a pager provides
  shmem.rs            Shared memory regions
  futex.rs            Futex wait and wake
  pipe.rs             Pipes
  stream.rs           Connected pairs of byte streams
  pollset.rs          Waiting on more than one descriptor
  pty.rs              Pseudo-terminals and their line discipline
  timerfd.rs          Timers as descriptors
  eventfd.rs          Counters as descriptors
  userspace.rs        Starting init, address space helpers
  elf.rs              ELF64 loader, for init
  idt.rs              Interrupt descriptor table and exceptions
  pit.rs  pic.rs      Timer and interrupt controller
  irq_dispatch.rs     IRQ delivery to user-space tasks
  cpu.rs              SMEP, SMAP and the FS base
  random.rs  rtc.rs   Random bytes; the date
  serial.rs           COM1 debug output
  sync.rs             IrqSpinLock<T>
  multiboot2.rs       Multiboot2 tag parser
  modules.rs          Boot module registry
  services.rs         What a loadable module is handed
  fat32.rs            Interface to the fat32 module
  console/            Kernel console (VGA text or framebuffer)

drivers/
  vga/  fat32/        Flat modules the kernel loads itself, in ring 0

docs/abi.md           The system call ABI
docs/fpu.md           How floating-point state is kept, one copy per task
MISSING.md            What the kernel has not got
tools/                The ABI check and the header generator
```

## Building

Dependencies:

- **Rust nightly**, the one `rust-toolchain.toml` pins, with the
  `x86_64-unknown-none` target, `rust-src` and `llvm-tools-preview`
- **objcopy** (binutils), to turn the two modules into flat binaries
- **GRUB** (`grub-mkrescue` or `grub2-mkrescue`) and **QEMU**, only for
  `make iso` and `make run`

```bash
make                         # kernel.bin and the two modules
make install DESTDIR=<dir>   # stage them, and the ABI, for a distro
make iso                     # a GRUB image with the kernel alone on it
make run                     # boot that in QEMU (BIOS)
make run-uefi                # the same under OVMF
make clean
```

## Running

The kernel alone boots to nothing: it looks for an `init` module and there is
none on the GRUB image. To run a system, build the userland beside it and let
ExplOSion assemble the image:

```bash
git clone https://github.com/MagicJester2764/quarkutils ../quarkutils
git clone https://github.com/MagicJester2764/bang ../bang
git clone https://github.com/MagicJester2764/explosion ../explosion
cd ../explosion
make run
```

## Testing

The kernel is tested from outside, through the ABI, by a program: `dtest` in
quarkutils makes 566 checks from user space. A kernel change is verified by
booting an image and running it — `tools/boot-test.sh` in ExplOSion.

## Disclaimer

This is primarily an AI-assisted experimental project, not a production kernel. It was built as a vehicle for exploring OS development concepts with AI tooling. Use at your own risk.
