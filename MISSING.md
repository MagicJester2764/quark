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
- **Choosing who is ended.** A machine that has run out, with nothing left
  to give up and nothing on its way out, ends whichever task touched the
  page it could not give — not the biggest one, and not the newest.
- **Looking for memory ahead of time.** It is looked for when a frame is
  wanted and there is none, by whoever wanted it, who waits. Nothing keeps
  a margin free in the background, and nothing notices that a page was
  written out a moment before it was wanted again and was not worth
  writing.
- **A call to a server that a signal cuts short.** The kernel runs a
  handler wherever it finds a program (`docs/abi.md`, *Signals*), and every
  wait the kernel keeps is ended by one. A call to a server is not: woken
  with no reply it would fail, so the handler runs when the server answers
  — a read of a file, a wait for a lock. Ending one would be a word to the
  server that its caller has gone away, and an answer that says so.
- **Signals queued.** One raised twice before it is run is run once — the
  real-time signals too, which Linux queues with a value each. A handler is
  told who raised a signal by process id; not by user, and for SIGCHLD not
  with the child's status.
- **The rest of job control.** A session's terminal is given up only by its
  leader ending. And nothing is hung up on when a terminal's master goes:
  its readers see the end of the file.
- **Signals nothing raises.** A pipe with nobody reading it is found out by
  the writer's runtime, which asks what kind of thing the descriptor is.
  There is one alarm for a program, in real time. The time a program spends
  running is measured (`SYS_USAGE`) and limited (`SYS_CPU_LIMIT`, SIGXCPU),
  but no timer counts it down: `ITIMER_VIRTUAL` and `ITIMER_PROF`, and
  SIGVTALRM and SIGPROF with them, are not there.
- **Process ids that come round.** A process id is an endpoint number, and
  those only go up. A C `pid_t` holds two thousand million of them; Linux
  wraps and reuses, and here the task after that many has an id a C program
  cannot hold.
- **A wait list names a task by its id, and ids are reused.** A task killed
  while it is parked in a read or a write is taken off the list it was on
  (`pipe::forget_waiter`, reached through what it held), because a wake meant
  for it would otherwise reach whatever has its number next — and a call to a
  server that is woken with no reply fails. That covers pipes, streams,
  terminals, timers and counters. A poll set's one waiter is not covered, and
  the lists want to name the task rather than the number: the kernel already
  has a number per task that is never reused, for endpoints.
- **AMX**, and whatever comes after AVX-512. A program may use x87, SSE,
  AVX and AVX-512, where the processor has them, and each task's registers
  are its own; the tile registers are not turned on, being eight kilobytes
  a task for a use nothing here has. See [`docs/fpu.md`](docs/fpu.md).
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
  gigabytes — all of it: a driver that holds it may map any device's. It
  claims a device for its memory (`SYS_DEVICE_CLAIM`), and the claim does
  not yet narrow what it may map.
- **The rest of an IOMMU.** Where Intel's VT-d is, a device's DMA reaches
  its driver's memory and nothing else; its interrupts are not remapped,
  so a device can still send any message it likes. A firmware that
  reserves memory for a device (an RMRR — a USB controller's keyboard, a
  graphics card's frame) leaves the machine unguarded, as does AMD's
  IOMMU, which is not driven. And a device is given its driver's memory at
  the memory's own addresses: a device that addresses less than the
  machine has needs memory from below what it can reach, as before.
- **The rest of ACPI.** The kernel reads two tables and one object of a
  third (`acpi.rs`), and acts on all three: the processors, the interrupt
  controllers, and how to turn the machine off and restart it
  (`SYS_POWER`). It does not run the firmware's own programs — there is no
  interpreter for them — so there is no sleeping, no button that asks the
  machine to turn off, no battery or lid or temperature, and a control
  register that is memory rather than a port is not written.
- **Memory above 511 GiB**, which is as far as the kernel's own map of
  memory goes: the first entry of the top-level page table, less a gigabyte
  for the heap. And the table of frames — a bit and a byte for each — has
  to fit below four gigabytes, which it does for any machine that small.
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
| Capability slots per program | 64 |
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
| Memory | 511 GiB |

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
- **Memory is written out through the file server, a page a call**, to a
  file, on a disk driven a word at a time. It is correct and it is slow:
  about a hundred pages a second in a virtual machine. A partition of its
  own for it, pages written several at a time, and a disk driver that does
  not copy through a port are each of them faster and none is here.
- **Not every page can be taken.** A page a fork left in two programs
  stays while it is in both; a page of a file mapped to be written through
  stays while it is mapped; the cache that pages pass through on their way
  out holds thirty-two megabytes.
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
