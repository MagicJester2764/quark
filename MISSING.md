# What the kernel is missing

The kernel's own list. What the *system* lacks above the ABI — a filesystem
that cannot shorten a file, a compositor with no drag and drop — is under
"Known gaps" in [quarkutils](https://github.com/MagicJester2764/quarkutils)'
`CLAUDE.md`.

This file was once a list of twenty-three things Quark needed, twenty-one of
them struck through as done. What follows is what is true now.

## Not there

- **A second CPU.** The kernel is uniprocessor, and several of its invariants
  lean on that: `IrqSpinLock` panics on contention precisely because, on one
  CPU, contention can only mean a lock taken twice. SMP invalidates that
  reasoning across the kernel and is its own project.
- **Paging anything out.** Anonymous memory gets its frames when first touched
  and keeps them; a machine that runs out ends whichever task touched the page
  it could not give, not the biggest one. There is no swap.
- **Copy-on-write.** `fork` copies every page the caller owns, eagerly. Sharing
  until written needs a reference count per frame, and frames here have an
  owner and nothing else.
- **POSIX signals.** The kernel has three signals of its own — interrupt,
  terminate and kill (`SYS_SIGNAL`). Kill ends a task at once; the other two
  arrive as bits in its notification word, abandon the call it is blocked in,
  and end it five seconds later if it is still there. That is enough for
  Ctrl-C at the console and for `kill`. What is missing is everything a C
  program means by the word: nothing runs a handler in the task, there are no
  masks and no process groups, and nothing is sent when a child exits or a
  terminal changes size. A pseudo-terminal knows its interrupt character — it
  is taken out of what is typed and the line thrown away — and tells nobody.
  A fault in
  ring 3 ends the task with the negated Linux signal number as its status,
  which is the only place those numbers appear.
- **AVX.** `CR4.OSXSAVE` is clear, so an AVX instruction faults. Turning it on
  means moving the per-task floating-point state from `FXSAVE` to `XSAVE`
  first — see [`docs/fpu.md`](docs/fpu.md).
- **A clock finer than the tick.** Time is the PIT at 100 Hz: a timer, a sleep
  and a futex deadline all round up to ten milliseconds. There is no HPET, no
  APIC timer and no use of the TSC.
- **An interrupt controller newer than the 8259.** Sixteen lines, no APIC, no
  MSI. A driver for a device that only speaks MSI has nothing to be given.
- **ACPI.** The kernel reads none of it. Powering off is a user program
  writing to a port QEMU happens to listen on.
- **Memory above 4 GiB.** The frame allocator's bitmap covers the first four
  gigabytes and the rest of what the firmware reports is left alone.
- **Setting the clock.** The date is read once, from the CMOS clock at boot,
  as UTC. There is no call to change it and no time zone.

## Fixed tables

Everything here is an array with a size, and each size is a limit somebody
will meet:

| | |
|---|---|
| Tasks | 64, threads included |
| Descriptors per program | 64, and one more for its working directory |
| Capability slots per task | 64 |
| Pipes | 96 in the machine, 8 made by any one program |
| Connected streams | 32 |
| Poll sets | 64, each watching 32 descriptors |
| Pseudo-terminals | 8 |
| Objects servers serve (open files) | 1024 |
| Timers, event counters | 16 of each |
| Shared memory regions | 256, of at most 4096 pages |
| Memory objects | 256, with 8192 cached pages between them |
| Futex waiters | 64 at once |
| Kernel heap | 1 GiB of address space |

## Half done

- **`exec` in a program with threads is refused.** POSIX has it end every
  other thread, and ending them means unwinding what they hold in a server.
- **A thread starts with a copy of its creator's capabilities**, not a share:
  what either is granted or gives up afterwards the other does not see.
  Descriptors are the program's and are shared.
- **A pty's window size is stored and nobody is told when it changes.** Linux
  sends `SIGWINCH`, and there are no signals.
- **The page cache never shrinks under pressure.** A mapped file's pages stay
  until nothing maps the file.
- **Five deprecated calls still answer** — the pre-capability grants, 86 to 90
  — and the two debug-console calls (160, 161) are marked for withdrawal once
  early output is handled another way. `docs/abi.md` says which.

## Left over

- **`fat32.drv`** is loaded at boot, announced on the console, and called by
  nothing: `init` reads the boot image itself. `vga.drv` is still the text
  console on a machine with no framebuffer. Both are from the months when
  drivers were flat binaries the kernel loaded; every driver that matters is
  an ordinary program now.
- **A driver ABI.** Drivers are user programs that speak the system call ABI
  like any other, which is the right answer; there is nothing separate to
  stabilise until somebody outside these repositories writes one.
