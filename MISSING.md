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
- **Signals that interrupt.** A program is told of a signal it has a handler
  for, and its runtime runs the handler at the next system-call boundary
  (`docs/abi.md`, *Signals*). Nothing stops a program in the middle of
  computing: one that handles a signal and then makes no call is not
  interrupted by it. That would be a frame on the user stack and a way back
  from it, and saving everything in between — the floating-point state too.
- **Process groups, sessions and jobs.** Ctrl-C goes to every program holding
  the terminal, the signals that stop a program do nothing, and there is no
  foreground to hand a terminal to. A shell runs with job control off.
- **Signals nothing raises.** None when a child ends, none when a timer runs
  out — there is no `alarm` — and none when a terminal changes size. A pipe
  with nobody reading it is found out by the writer's runtime, which asks
  what kind of thing the descriptor is.
- **A mask the kernel knows.** A signal a program has blocked is held back by
  its runtime, which can only hold back what it would have run: a blocked
  signal with no handler does what it does at once.
- **A wait list names a task by its id, and ids are reused.** A task killed
  while it is parked in a read or a write is taken off the list it was on
  (`pipe::forget_waiter`, reached through what it held), because a wake meant
  for it would otherwise reach whatever has its number next — and a call to a
  server that is woken with no reply fails. That covers pipes, streams,
  terminals, timers and counters. A poll set's one waiter is not covered, and
  the lists want to name the task rather than the number: the kernel already
  has a number per task that is never reused, for endpoints.
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
  sends `SIGWINCH`.
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
