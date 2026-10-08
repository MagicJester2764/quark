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
- **A child that is its program's**, so that any thread may collect it,
  where it is the task's that made it. The rest of what threads lacked is
  done: 32,768 tasks where there were sixty-four, the futex's requeue and
  priority inheritance, how nice a thread is and the real-time classes, and
  a program's own FS and GS bases.
- **Several messages for a device.** A device is its own capability
  (`PciDevice`), with its registers wherever the firmware put them, and its
  driver has one message: the kernel aims it where the device has MSI, and
  the driver writes it into the first entry of its MSI-X table where it
  has only that. What is left is several — each of an NVMe disk's queues
  with its own, on a processor of its own — and a table only the kernel
  writes: a device's MSI-X table is in its own registers, where its driver
  aims it as it likes.

## Not asked for

- **The system's own programs somewhere else each time.** A program's
  stack, heap, threads and anonymous memory move each run, and so does a
  program linked to be put anywhere — a PIE, as a program built for Linux
  usually is. The system's own programs are not linked so: their code is
  where it was linked, and so is the page a program's arguments are on.
  The kernel is where it was linked.
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

Most of what this list was is made when a program makes it now (4.1):
pipes and named pipes, streams, local sockets and their queues, poll sets
and what they watch, pseudo-terminals, the objects servers serve, timers,
counters, descriptors read for signals, shared regions and futex waiters
are each a record a program's call makes, bounded by its descriptors and by
what memory the kernel can spare (`reclaim::may_make`). What is still an
array with a size, each a limit somebody will meet:

| | |
|---|---|
| Processors | 16 |
| Tasks | 32,768; 4,096 a program, its children included, without `TaskMgmt` |
| Interrupts of a device's own (MSI) | 32 |
| Entries of the firmware's memory map | 64, once neighbours are joined |
| Descriptors per program | 1,024 to start, and as many as 65,536 if it raises its limit |
| Capability slots per program | 65,536, room made 256 at a time |
| A shared memory region | 4,096 pages |
| A program's own timers | 32 |
| Real-time signals waiting | 64 a program and 16 a task, behind the first of each number |
| Groups a task is in besides its own | 16 |
| Memory objects | 2,047 — the slot is eleven bits of a page-table entry — and 1,024 a pager; a cache of their pages a quarter of memory, at least 8192 pages, at most a million |
| PCI functions | 128, of the first segment |
| Programs whose devices an IOMMU guards | 8, with 32 devices between them |
| A kernel stack | 28 KiB a task: twice the deepest an acceptance has measured |
| Kernel heap | 1 GiB of address space |
| Memory | 511 GiB |

## Half done

- **Memory is written out through the file server, a page a call**, to a
  file. It is correct and it is slow: `dtest pressure` — a program that
  wants more memory than the machine has, and then four threads of one —
  takes half a minute where the disk is IDE's, driven a word at a time,
  and six to twelve seconds where the device copies for itself (NVMe,
  AHCI, virtio), in a virtual machine. A partition of its own for it and
  pages written several at a time are each faster, and neither is here.
- **Not every page can be taken.** A page a fork left in two programs
  stays while it is in both; a page of a file mapped to be written through
  stays while it is mapped; and what is on its way out waits in the cache
  of objects' pages, which a machine taking pages faster than its pager
  writes them fills.
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
