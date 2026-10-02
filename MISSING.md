# What the kernel is missing

The kernel's own list. What the *system* lacks above the ABI — a filesystem
that cannot shorten a file, a compositor with no drag and drop — is under
"Known gaps" in [quarkutils](https://github.com/MagicJester2764/quarkutils)'
`CLAUDE.md`.

This file was once a list of twenty-three things Quark needed, twenty-one of
them struck through as done. What follows is what is true now.

## Not there

- **A second processor in the kernel.** Programs run on every processor
  the machine has; the kernel runs on one at a time
  ([`docs/smp.md`](docs/smp.md)). A system call, a fault or an interrupt on
  a second processor waits for the first to leave, so four programs making
  calls make no more calls than one. Taking that lock apart is its own
  project, and with it go the rest of what using several processors well
  means: a ready queue for each rather than one for the machine, a task kept
  where its cache is, a task that can say where it runs, a better task
  waking that interrupts the processor running the worst rather than
  waiting for a tick, and an idle processor that takes no ticks. Sixteen
  processors at most.
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
- **The rest of job control.** There are process groups, sessions, a group
  in front of a terminal and programs that stop (`docs/abi.md`, *Jobs*).
  What a terminal does not do is stop a job for *writing* to it from behind
  (`TOSTOP`) or for changing its settings from there; only a read and a
  change of who is in front are checked. A session's terminal is given up
  only by its leader ending. And nothing is hung up on when a terminal's
  master goes: its readers see the end of the file.
- **Signals nothing raises.** None when a terminal changes size. A pipe
  with nobody reading it is found out by the writer's runtime, which asks
  what kind of thing the descriptor is. (An alarm and a child ending are
  raised: `SYS_SIG_ALARM`, and SIGCHLD.) There is one alarm for a program,
  in real time; nothing measures the time a program spends running, so there
  is nothing to raise for that.
- **Process ids that come round.** A process id is an endpoint number, and
  those only go up. A C `pid_t` holds two thousand million of them; Linux
  wraps and reuses, and here the task after that many has an id a C program
  cannot hold.
- **A mask the kernel knows.** A signal a program has blocked is held back by
  its runtime, which can only hold back what it would have run: a blocked
  signal with no handler does what it does at once — a blocked SIGTSTP
  stops. The one place job control leans on a mask, a shell taking its
  terminal back with SIGTTOU blocked, the runtime says so in the call.
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
- **A clock that is kept right.** The clock is the processor's counter, to
  the nanosecond, at the rate it was measured at when the machine started —
  good to a few parts in ten thousand, and nothing corrects it afterwards.
  A machine whose counter cannot be trusted keeps time by the tick. There is
  no HPET, and the local APIC's timer is not used in the mode that takes a
  time on the counter itself.
- **A waking that preempts.** A task whose wait ends on time runs at once
  if it is of a better band than what is running where the clock is, or if
  a processor is idle; one of the same band waits its turn.
- **An interrupt for every device, wherever it is.** Devices interrupt
  through the I/O APIC where there is one, on the sixteen ISA interrupts,
  wherever the firmware says each comes in; and a device that can send its
  interrupt as a message (MSI) is given a number of its own. What is not
  there: the I/O APIC's lines above the sixteen — where a PCI device that
  cannot send messages is wired on a newer machine, which the firmware's
  bytecode says and its tables do not; more than one message for a device
  (MSI-X); and anywhere to deliver one but the first processor.
- **A device's registers above four gigabytes**, and an authority for one
  device. A driver maps its device's registers by the `DeviceMemory`
  capability, which covers what the firmware's map leaves out below four
  gigabytes — all of it: a driver that holds it may map any device's.
- **The rest of ACPI.** The kernel reads two tables and one object of a
  third (`acpi.rs`), and acts on all three: the processors, the interrupt
  controllers, and how to turn the machine off and restart it
  (`SYS_POWER`). It does not run the firmware's own programs — there is no
  interpreter for them — so there is no sleeping, no button that asks the
  machine to turn off, no battery or lid or temperature, and a control
  register that is memory rather than a port is not written.
- **Memory above 4 GiB.** The frame allocator's bitmap covers the first four
  gigabytes and the rest of what the firmware reports is left alone.
- **A time zone.** The date is UTC. `SYS_CLOCK_SET` sets it, for a holder
  of `Clock`, and writes it to the CMOS clock.

## Fixed tables

Everything here is an array with a size, and each size is a limit somebody
will meet:

| | |
|---|---|
| Processors | 16 |
| Tasks | 64, threads included |
| Interrupts of a device's own (MSI) | 32 |
| Entries of the firmware's memory map | 64, once neighbours are joined |
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
- **A child is its creator task's to wait for, not its program's.** A thread
  cannot collect a child another thread of its program forked; POSIX lets
  any thread. The children of a thread that has ended are nobody's.
- **A pager's idle objects are told to it in a list thirty-two long.** More
  of a pager's objects than that going idle before it has looked — every
  mapped file of several programs ended together — and the rest are not
  told of: they stay until the pager lets them go some other way. Deaths of
  tasks and of programs are not lost; this still can be.
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
