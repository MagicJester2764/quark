# What the kernel is missing

The kernel's own list. What the *system* lacks above the ABI — a filesystem
that cannot shorten a file, a compositor with no drag and drop — is under
"Known gaps" in [quarkutils](https://github.com/MagicJester2764/quarkutils)'
`CLAUDE.md`.

This file was once a list of twenty-three things Quark needed, twenty-one of
them struck through as done. What follows is what is true now: what has been
asked for and is planned, and then the short list of what nobody has asked
for.

## Asked for, and planned

- **The kernel on every processor at once.** Programs run on every
  processor; the kernel runs on one at a time
  ([`docs/smp.md`](docs/smp.md)), so four programs making calls make no
  more calls than one. Taking the lock apart is the next kernel work, and
  with it a ready queue for each processor, a task kept where its cache is
  and one that can say where it runs, a task woken that runs at once on
  whichever processor runs the worst instead of at its band's next turn,
  and an idle processor that takes no ticks. Sixteen processors at most.
- **Threads in full.** More than sixty-four tasks; a child that is its
  program's, so that any thread may collect it, where it is the task's
  that made it; and the rest of the futex — requeue, priority inheritance,
  robust lists.
- **Signals queued**, as the C library will want for its real-time ones:
  one raised twice before it is run is run once, and a handler is told who
  raised it by process id and nothing more.
- **A device's own authority, and its registers wherever they are.** A
  driver that holds `DeviceMemory` may map any device's registers below
  four gigabytes, and none above; a claim (`SYS_DEVICE_CLAIM`) narrows
  what its device may reach, not what it may map. And a device is given
  one message (MSI), not several (MSI-X), as the drivers for faster
  devices will want.

## Not asked for

- **A program's code somewhere else each time.** Its stack, heap, threads
  and anonymous memory move each run; its code is where it was linked, as
  nothing is built to load anywhere (PIE), and so is the page its
  arguments are on. The kernel is where it was linked.
- **Choosing who is ended.** A machine with nothing left to give up ends
  whichever task touched the page it could not give, not the biggest.
- **Looking for memory ahead of time.** It is looked for when a frame is
  wanted and there is none, by whoever wanted it; nothing keeps a margin
  free in the background.
- **A call to a server that a signal cuts short.** A handler runs when the
  server answers: ending the call would be a word to the server that its
  caller has gone, and an answer that says so.
- **The rest of job control.** A session's terminal is given up only by its
  leader ending, and a terminal's master going hangs up on nobody.
- **Signals nothing raises.** No timer counts a program's own time down
  (`ITIMER_VIRTUAL`, `ITIMER_PROF`); a pipe with nobody reading it is found
  out by the writer's runtime, not raised.
- **Process ids that come round.** They only go up, and after two thousand
  million a C `pid_t` cannot hold one.
- **A wait list that names a task.** The lists name a task's id, which is
  reused, and a killed task is taken off them through what it held; a poll
  set's one waiter is not.
- **AMX**, and whatever comes after AVX-512 ([`docs/fpu.md`](docs/fpu.md)).
- **A clock that is kept right.** It runs at the rate it was measured at
  when the machine started, good to a few parts in ten thousand; there is
  no HPET, and a machine whose counter cannot be trusted keeps time by the
  tick.
- **The I/O APIC above the sixteen ISA interrupts**, where a newer
  machine wires a PCI device that cannot send messages: which line is in
  the firmware's bytecode, not its tables.
- **The rest of an IOMMU.** Interrupts are not remapped; a firmware that
  reserves memory for a device (an RMRR) leaves the machine unguarded; AMD's
  is not driven; and a device that addresses less than the machine has
  still needs memory from below what it can reach.
- **The rest of ACPI.** Three tables and one object of a fourth are read
  (`acpi.rs`); the firmware's own programs are not run, so there is no
  sleeping, no power button, no battery, lid or temperature.
- **Memory above 511 GiB**, as far as the kernel's own map goes.
- **A time zone.** The date is UTC; a C library keeps the zone.

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
