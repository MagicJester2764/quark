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

- Preemptive scheduling in four bands, where a synchronous call hands the CPU
  straight to its callee, a task runs at the band of whoever is waiting on
  it, and within a band a program has the share its niceness gives it, by
  Linux's weights
- What each program has used, to the nanosecond, in it and in the kernel for
  it — and a limit on how long it may run
- A clock in nanoseconds — the processor's counter, where it can be trusted —
  and waits, timers and alarms that end when they are due rather than on the
  next of a hundred ticks a second
- Every processor the machine has, up to sixteen: programs run on all of
  them at once, and the kernel on one at a time
- Synchronous IPC — send, receive, call and reply with fixed-size messages —
  plus notifications, deadlines, and buffers lent with a call so that no server
  has to map a client's memory
- Object capabilities as the only authority: I/O ports, IRQs, physical ranges,
  endpoints, task management, a PCI device — held by a program, so that its
  threads hold them too. There is no UID 0 bypass
- Every PCI device found and sized at boot, and its configuration the
  kernel's: a driver holds its own device and reaches nothing else — its
  registers, its interrupt, whether it may copy memory — and a display
  device that draws from memory is given a screen that outlives its driver
- Where the machine has an IOMMU, a device that copies memory reaches what
  its driver was given and nothing else
- Address spaces, memory given its frames when first touched, shared memory,
  and memory objects whose pages a user-space pager provides — which is how a
  file is mapped, and how memory that is not being used is written out when
  there is not enough of it
- A descriptor table for each program: IPC endpoints, pipes and named pipes,
  connected streams that can carry descriptors and say who is at the other
  end, local sockets found by a name, poll sets that watch as epoll does,
  pseudo-terminals, timers, event counters, signals read as records, and
  a server's files, which the kernel counts and their server serves
- Tasks, threads, a `fork` that shares memory until one side writes it, and
  `exec` — of a program with threads too; futexes with deadlines, and a
  robust mutex let go when the task that held it dies; every task's own
  floating-point and vector registers, as wide as the processor has; and a
  first program whose stack is somewhere else each time, as the userland
  puts everything of every program
- Unix's signals: handlers the kernel runs wherever it finds a program, a
  mask for each thread, a signal for one thread, faults handed to a
  program's handler, waits a signal ends saying whether to make the call
  again, real-time signals that queue with what they carry, a program's
  own timers that raise them, and process groups, sessions and a
  terminal's job control
- A program's own `syscall` instructions, where it asks, raised as a
  signal for its C library to answer: how a program built for Linux's
  musl, linked to its shared C library, runs here unchanged
- IRQ delivery to user-space drivers, and page faults forwarded to a pager
- Random bytes (ChaCha20, seeded from RDSEED or RDRAND and the machine's
  timing), and the date, which a holder of the capability may set
- Turning the machine off and restarting it, the way its firmware says to,
  for a holder of the capability to

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
  scheduler.rs        Preemptive scheduler: four bands, and within one whoever
                      has run least by its weight
  usage.rs            What a program has used, how nice it is, how long it may run
  task.rs             Task struct
  fdtable.rs          A program's descriptors, and what it has said about signals
  served.rs           Descriptors a server serves: a file is one
  signal.rs           Signals: raised, held back, and handlers the kernel runs
  job.rs              Process groups, sessions, stopping and continuing
  context.rs          Task context switching
  fpu.rs              Floating-point and SSE state, one copy per task
  ipc.rs              Synchronous IPC and notifications
  lend.rs             Memory lent with a call
  cap.rs              Object capabilities
  paging.rs           Page tables, address spaces, memory on demand
  pmm.rs              Physical memory: a bitmap, given out from both ends
  heap.rs             Kernel heap
  memobj.rs           Memory objects: pages a pager provides
  reclaim.rs          Giving memory back when there is none to give
  shmem.rs            Shared memory regions
  futex.rs            Futex wait and wake
  pipe.rs             Pipes
  stream.rs           Connected pairs of byte streams
  pollset.rs          Waiting on more than one descriptor
  pty.rs              Pseudo-terminals and their line discipline
  local.rs            Local sockets: found by a name, and who is at the other end
  timerfd.rs          Timers as descriptors
  ptimer.rs           A program's own timers, which raise signals
  eventfd.rs          Counters as descriptors
  sigfd.rs            Signals read from a descriptor
  threads.rs          What a thread library keeps with the kernel: robust lists
  userspace.rs        Starting init, address space helpers
  elf.rs              ELF64 loader, for init
  idt.rs              Interrupt descriptor table and exceptions
  clock.rs            What time it is, and waking what is due when it is due
  pit.rs              The tick: a hundred interrupts a second
  rtc.rs              The date, from the battery-backed clock and back to it
  intc.rs             The interrupt controller devices come in through:
  ioapic.rs  pic.rs     the I/O APIC, or the 8259s
  acpi.rs             The firmware's tables: processors, interrupt controllers,
                      how to turn the machine off, where its IOMMUs are, and
                      where PCI configuration is
  power.rs            Turning it off, and starting it again
  pci.rs              Every PCI device, found once; its configuration
  devmem.rs           Device memory: the addresses that are not memory
  display.rs          A display device's screen: memory nobody owns
  iommu.rs            Where a device may copy memory: Intel's VT-d
  percpu.rs           What each processor has of its own
  klock.rs            The kernel lock: one processor in the kernel at a time
  lapic.rs            The local APIC: a tick, a timer, a word to another processor
  smp.rs  ap_boot.s   Starting the other processors
  tlb.rs              A mapping taken away, on every processor
  irq_dispatch.rs     IRQ delivery to user-space tasks
  cpu.rs              SMEP, SMAP and the FS base
  io.rs               Port I/O
  random.rs           Random bytes
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
docs/smp.md           How it runs on more than one processor
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
quarkutils makes 1057 checks from user space, and more where the machine has
more to ask about: 1094 on the machine ExplOSion tests on. A kernel change is verified by
booting an image and running it — `tools/boot-test.sh` in ExplOSion — on one
processor and on four.

## Disclaimer

This is primarily an AI-assisted experimental project, not a production kernel. It was built as a vehicle for exploring OS development concepts with AI tooling. Use at your own risk.
