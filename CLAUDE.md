# Working on Quark

Quark is an x86-64 microkernel, and this repository is the kernel and nothing
else. It is one of five that build together and must be checked out as
siblings:

```
repos/
  quark/       this repo — the kernel, its two loadable modules, and the ABI
  quarkutils/  everything that runs on it: the runtime, init, the drivers, the
               servers, the C library, the shell and the programs
  bang/        UEFI bootloader, and nothing else
  explosion/   the distro: stages the other three and assembles the image
  rust/        fork of rust-lang/rust carrying the x86_64-unknown-quark std PAL
```

**Read `../quarkutils/CLAUDE.md` before touching anything outside this tree.**
The screen, the filesystem, the C library, the toolkits and every server's
rules are written down there. This file is about ring 0.

The dependency runs one way: ExplOSion reaches down to the kernel, the userland
and the bootloader, and nothing reaches up. The kernel and the userland do not
reach sideways either — neither names the other's checkout, and neither builds
against the other's source. What crosses between them is what this repository
*installs*, and `../explosion`'s stage directory is the only place the two
meet.

## The ABI is the interface

`docs/abi.md` is the contract, and since the userland left this tree it is all
the userland has. A call the kernel answers and the document does not describe
is a call nobody outside this tree can use correctly.

- **A number is written in one place.** `src/syscall.rs` holds the constants
  the dispatch is compiled from. `make install` derives
  `usr/include/quark/abi.h` from them (`tools/gen-abi-header.sh`) and installs
  it beside `usr/share/doc/quark/abi.md`; those two files are what a userland
  may know about this repository.
- **`tools/check-abi.sh` is the kernel's half of the check**, and `make` runs
  it first: no two calls share a number, every call has a reference row in the
  document with its number, no row names a call that is not there, and the
  version the document says it describes is the one the kernel reports.
  `../quarkutils/tools/check-abi.sh` is the other half — its own copy of the
  numbers against the installed header — and ExplOSion's stage is where that
  one is not allowed to be skipped.
- **Two names on one number is silent.** The dispatch is a `match` and takes
  the first arm that matches, so the second call never runs and every use of
  it does something else: `SYS_ADDRSPACE_DESTROY` and `SYS_MAP_PHYS` were both
  38 for a phase, and every address space a failed spawn left behind stayed
  behind. rustc says "unreachable pattern" about it, in a build that prints
  other warnings.
- **Changing it.** A new call is a constant, a dispatch arm, a row in
  `docs/abi.md` and a line in its version history; the minor version goes up
  for an addition and the major when a number moves or a call changes meaning.
  Then the userland's copies — `quark-rt/src/syscall.rs`, and
  `libc/include/quark/syscall.h` if C needs it — in *its* repository. The
  stage fails until the two agree, which is the point.
- **The document goes stale quietly.** Its rows said `TaskMgmt` for five calls
  that had asked for less since 2.9, and "There is no fork or exec" two lines
  under the rows for both. Nothing checks the prose; read the neighbours when
  you add a row.

## Toolchain

Pinned to `nightly-2026-03-01` in `rust-toolchain.toml`. It is the same pin
`../quarkutils` and `../bang` carry, and of the three this tree has the least
reason for the exact date: `../quarkutils` must match the commit the std fork
is based on (its `CLAUDE.md` says how to check), and `../bang` pins because
newer toolchains rewrite the uefi crate's UCS-2 loops into a `wcslen` libcall
it has to supply. The kernel depends on neither; it pins the same nightly
because that is the compiler it is tested with, and because three equal pins
are one download. Move them together.

A floating `nightly` channel is what to avoid: it drifts forward and leaves
the other two behind.

Fresh machine:

```bash
rustup toolchain install nightly-2026-03-01
rustup component add rust-src llvm-tools-preview --toolchain nightly-2026-03-01
rustup target add x86_64-unknown-none --toolchain nightly-2026-03-01
```

## Build and run

```bash
make            # kernel.bin and the two modules under drivers/
make install DESTDIR=<dir>   # stage them, and the ABI, for a distro to consume

cd ../explosion
make run        # stage all three trees, assemble the image, boot it in QEMU
```

Quark builds a kernel. It does not know what an image looks like or what will
run on it, and nothing here reaches into a sibling repo — `make install` lays
artifacts out and ExplOSion collects them:

```
$(DESTDIR)/kernel.bin
$(DESTDIR)/drivers/vga.drv, fat32.drv    flat modules the kernel loads itself
$(DESTDIR)/usr/include/quark/abi.h       the system call numbers
$(DESTDIR)/usr/share/doc/quark/abi.md    and what they mean
```

`drivers/` is the kernel's: flat binaries with no access to kernel symbols,
loaded from boot modules and called through a table (`services.rs` is what
they are handed). The drivers that matter — disk, keyboard, network — are
ordinary programs in `../quarkutils`.

`make iso` and `make run` boot the kernel alone under GRUB, with nothing to
run: useful for the first hundred lines of boot and for nothing after.

ExplOSion's QEMU targets pass `-cpu max` deliberately. Default CPU models expose
neither SMEP nor SMAP, so the kernel's supervisor-mode protections are silently
inactive without it — a boot test on the default CPU proves nothing about them.
The kernel prints which it enabled to serial at boot.

Only serial reaches stdout; `console::puts` goes to the framebuffer. To see
user-space output headlessly, screendump over QMP rather than assuming the
system hung.

## Testing

There are no tests in this tree, and that is the shape of a microkernel rather
than an omission: the kernel is tested from outside, through the ABI, by a
program. `dtest` in `../quarkutils` makes 267 checks — capabilities, IPC,
memory, descriptors, scheduling, `dtest calls` with three million calls in
three seconds — and `qfuzz` throws random requests at every service.

So a kernel change is verified by booting an image:

```bash
cd ../explosion
make hd                                   # or hd-ext4
tools/boot-test.sh <keys-file> <shot.ppm> # type `dtest`, screenshot the result
tools/check-rootfs.sh hdimage.bin         # e2fsck on what the boot left
```

A kernel fault prints to serial (`[UPFAULT ...]` for ring 3, a halt for ring
0); a failed check prints to the screen. Look at both.

## Invariants that must not regress

These were established deliberately. Breaking one silently re-opens a hole.
The ones that are rules for *programs* — what a spawner must do, what a server
may keep — are in `../quarkutils/CLAUDE.md`; these are the kernel's.

- **User mappings live at or above `paging::USER_MIN_ADDR` (PML4[1]).**
  `create_address_space` deep-copies only PML4[0]'s PDPT and *shares* the page
  directories beneath it, so a mapping below that writes into tables every
  address space shares and promotes them to USER everywhere. It is also what
  makes SMAP safe: no USER bit exists anywhere in the kernel's identity map.
  Validate with `paging::user_range_ok`.
- **`paging::OWNED` (PTE bit 9) decides what may be freed.** Only frames the
  address space owns go back to the allocator. Device MMIO, shared memory and
  frames another task still holds are mapped *without* it. An owned frame is
  mapped in exactly one place — that is what makes freeing it on unmap safe —
  so a new mapping path either leaves the bit off or moves the page, as
  `sys_addrspace_give` does, rather than copying the mapping.
- **A dead task holds all its memory until it is reaped.** `sys_wait` reaps the
  child it returns; the idle loop reaps the rest. Anything that collects a
  child some other way must reap it too, or a parent running programs back to
  back — which never lets the machine idle — runs out of memory.
- **Every task has its own floating-point state**, saved and restored on every
  switch (`fpu.rs`). The kernel is soft-float and never touches the registers,
  so this is the whole of it. FXSAVE is enough only while CR4.OSXSAVE is clear:
  enabling AVX without moving to XSAVE hands one task another's YMM registers.
- **Page-table entries that refer to a memory object carry its slot** in bits
  52–62, present or not, and every path that clears or replaces an entry —
  `clear_range`, `unmap_page`, `map_page`, `free_pt_leaves` — hands the
  reference back (`memobj::unmap_ref`). Miss one and the object is never
  released. Protection keys would give bits 59–62 a meaning, so CR4.PKE stays
  clear.
- **A reserved page is a non-present entry with `paging::MARKER` set**, in a
  page table or, for 2 MiB at once, a page directory. It is not empty: the
  walks that free tables and the checks that an address is free test for an
  all-zero entry, not a clear `PRESENT` bit, or they throw reservations away
  and free tables still in use. The kernel backs a reserved page before it
  touches it (`validate_user_range`, the futex path), and a page fault on one
  is served, so the first touch from anywhere gives it its frame.
- **A fault in ring 3 ends the task, never the machine.** The task exits with
  the negated Linux signal number (`idt.rs`), and only a fault taken in ring 0
  halts. musl's `abort()` is a privileged `hlt`, so before this, one failed
  assert stopped everything.
- **Capabilities are the authority.** There is no UID 0 bypass; `uid == 0` no
  longer short-circuits `cap::task_has_*`. A service that cannot do something
  is missing a capability, not a privilege level.
- **Every descriptor has a form that answers instead of waiting.**
  `SYS_FD_READ_NB` and `SYS_FD_WRITE_NB` return "would block" where
  `SYS_FD_READ` and `SYS_FD_WRITE` park, and `SYS_FUTEX_WAIT_TIMEOUT` gives up
  where `SYS_FUTEX_WAIT` does not. A new kind of descriptor needs both halves:
  a program that marked one non-blocking and was parked anyway does not fail,
  it stops — glib's main loop did, holding its context lock.
- **`UserAccess` guards must not span a block or yield.** RFLAGS.AC travels
  with the task's saved flags, so a guard held across a reschedule leaves the
  SMAP window open in whatever runs next. Copy into a kernel buffer first — see
  `fd_write_ipc`.
- **Validate user pointers with `validate_user_ptr{,_mut}`, not a range check.**
  The kernel runs on the caller's CR3; an in-range but unmapped address faults
  *inside* the kernel, sometimes with a lock held and interrupts off.
- **Mapping authority is ownership first, `PhysRange` second.** `sys_map_phys`
  and the deprecated `sys_addrspace_map` accept frames the caller owns
  (`pmm::owns_range`), so a task that allocated a frame may map it holding no
  capability at all. That is what almost every mapper does. A `PhysRange` grant
  is for memory the allocator never owned — the framebuffer, device MMIO, a
  boot module — and covers exactly that. The legacy `CAP_MAP_PHYS` bit confers
  nothing: it used to expand into a range over all of memory, which a bit
  passed with `SYS_GRANT_CAP` or `SYS_CAP_TRANSFER` could hand anybody.
- **IPC needs an Endpoint capability, and an Endpoint names a task, not a
  TID.** `sys_send`/`sys_call`/`sys_notify` look for an `Endpoint` recording
  the destination's endpoint number, which the kernel assigns when a task slot
  is filled and never gives out again. TIDs are recycled; numbers are not, so a
  capability to a dead task names nothing and nothing has to be swept at reap
  time. Only the task itself, its creator or a holder may mint one. Everybody
  else is handed one: every program gets the nameserver's from its spawner, and
  a lookup grants the one for the name. IPC the kernel performs through an
  installed fd bypasses this on purpose: the fd is the authorisation, and only
  a CAP_TASK_MGMT holder can install one.
- **A server calls a client back only with a capability the client offered.**
  `sys_call_offer` puts one on a call and `sys_cap_take` accepts it; nothing
  else can fill a server's CSpace, and a claim or registration made without
  one is refused.
- **A program is its address space, not its task.** `SYS_TASK_SPACE` names the
  program a task belongs to with an id the kernel never reuses, and
  `SYS_SPACE_WATCH` says when its last task has gone. TIDs are recycled; space
  ids are not. Anything in the kernel that remembers a number should remember
  that one — see the budget rule under *What a process is*.
- **A call from the kernel to a pager carries `PAGER_BIT` in its sender**, and
  nothing else can: the bit is set by `call_as` and by no syscall. The reply
  strips the bit and reaches the faulting task.
- **Only the kernel sends as sender 0.** A death notice (`TAG_TASK_DIED`, or
  the one for a program) is a message from sender 0, and no syscall can forge
  that; any program can send the *tag*. Servers depend on the difference.
- **`init` is started with the framebuffer and its boot modules, and nothing
  wider.** Every other `PhysRange` in the machine is derived from those, so
  what the kernel hands the first task bounds what any task can map.

## Descriptors the kernel owns

A **pseudo-terminal** is a kernel descriptor, for the same reason a pipe is: a
terminal emulator waits on its master with `poll`, and readiness the kernel
cannot see is readiness `poll` cannot report. `/dev/ptmx` makes a pair and
answers with the master, `/dev/pts/N` opens its slave, and both paths are
caught in the C layer (`../quarkutils/linux-abi`) ahead of the VFS. The line
discipline is the part programs depend on and no more — echo, canonical input,
and the newline translations — and the rest of a `termios` is stored and
handed back unchanged.
Between the master being opened and the slave being opened the master's read
waits rather than reporting an end of file: the program that will hold the
slave has not been started yet. Afterwards, the last slave closing *is* the end
of file, which is how a terminal learns its shell has exited.

A **timer is a descriptor too** (`SYS_TIMER_CREATE`), because a program's event
loop already waits on descriptors and a cursor that blinks needs the same wait
to end at a time rather than at an event.

And **a counter is one** (`SYS_EVENT_CREATE`, `eventfd`): one task adds to it,
another waits until it is not zero and takes what is there. It is what glib,
libwayland and GTK reach for first to wake a sleeping loop; a pipe is only ever
their fallback.

Adding a kind means touching every place that enumerates them, and missing one
is quiet: `FdKind` in `task.rs`, read and write in both their blocking and
non-blocking forms in `syscall.rs`, `pipe::release_fd` and `pipe::retain_fd`
(a kind in one and not the other leaks or double-frees), and
`pollset::watchable` and `readiness` — `poll` answered `POLLNVAL` for a
terminal until the last of those knew about it.

## What a process is

A program starts one of two ways, and the kernel's part in each is small.

A **spawner** builds one, in user space (`quark_rt::spawn` in
`../quarkutils`): it makes an address space, moves pages into it, wires the
descriptors, hands over the capabilities and starts a task there. The kernel's
rule is `may_prepare` — a task the caller created and has not started is its
own to fill, because nothing else can name it, it holds nothing and it cannot
run — so none of that needs authority over anybody. `TaskMgmt` buys the
unbounded form; without it a program may have sixteen children at once, which
is also how many threads it may have.

Or a program **forks** and **execs**, which is what a C program does and what
every Unix program assumes:

- **`fork` copies eagerly.** The child is a task in a copy of the caller's
  address space that returns 0 from the same system call — which works because
  the syscall stub's eleven pushes always land at `kernel_stack_top - 88`, so a
  task inside a call has its whole register frame at a known place. A page the
  parent owns becomes a page of the child's own; a page it does not own —
  shared memory, a device, a file's page — is shared, because `OWNED` is what
  decides who may free a frame. Copy-on-write would save all of the copying and
  none of the correctness, and it needs reference counts frames here have not
  got.
- **`exec` keeps the task and changes the program.** The ELF is loaded in user
  space, into an address space the caller made, and `SYS_EXEC_SPACE` swaps the
  task into it: same id, same descriptors, same capabilities, same parent, new
  address space — and therefore a new program as far as every server is
  concerned. The thread pointer is cleared with it, or the new program's first
  thread-local reads through an address the old one had.
- **A kernel budget is per program, not per TID.** A pipe outlives its
  creator — its ends are descriptors other tasks hold — so counting the
  per-task cap by TID gave a fresh task the budget of whatever had its number
  before, and a program that had spent its eight left the next task to take
  that number unable to make any. Space ids are never reused; TIDs are. The
  same trap is written down for servers in `../quarkutils/CLAUDE.md`, and it
  is worth looking for anywhere in the kernel that remembers a number.
- **Descriptors are released when a task dies, not when it is reaped.** Its
  memory waits for a parent to collect it, which is where the exit status
  lives; a descriptor is something another task can be *waiting on*, and the
  oldest idiom in Unix — a child writes down a pipe and exits, a parent reads
  to the end and then waits — had each half waiting for the other.
- **A descriptor sent over a stream outlives the sender's end.** It is in the
  stream rather than in the sender, and the peer can still take it.

## Scheduling

Four bands, best first: drivers, servers, ordinary programs, idle. A task runs
only when nothing better is waiting and takes turns within its own band. A
program asks for a band in its `manifest!` block alongside its capabilities,
and a spawner applies it under the same narrowing rule — it can never grant a
better band than it is in, so only `init` can put a driver in the driver band.

Three things follow from that, and breaking any of them is quiet:

- **Waiting means blocking.** A task in a better band that spins on
  `sys_yield` is immediately runnable again, so nothing below it ever runs.
  This is fatal rather than merely wasteful now: the runtime's
  `nameserver::lookup_retry` yielded a hundred times between tries and starved
  the VFS out of ever registering. A program sleeps with `SYS_RECV_TIMEOUT` on
  its own TID, which nobody can send to.
- **A synchronous call hands over the CPU.** The caller has blocked and has
  nothing to contribute until the reply, so the callee is switched to directly
  and runs on what is left of the caller's slice rather than a fresh one. A
  server does not earn a quantum every time it is called.
- **A hand-over keeps interrupts off from waking the callee to switching to
  it.** `make_ready` leaves the callee runnable but in no queue, since it is
  about to run, so `donate_to` takes the flags `call_inner` saved instead of
  saving its own. With a gap between the two, a tick preempted the caller,
  already blocked, and nothing ever ran either task again: fontconfig hung
  about once a minute scanning fonts. `dtest calls` makes three million calls
  in three seconds and caught it on its first run.
- **A task runs at the band of whoever is waiting on it**, for as long as that
  is true. Without it a server called by something urgent is preempted by
  anything in between. It is also what lets the direct switch stay safe: the
  callee already carries the caller's band when the scheduler decides whether
  handing straight over would run something ahead of its betters.

## Known gaps

- Nothing is ever paged out: anonymous memory is given its frames when first
  touched (`SYS_MAP_ANON`, which the C library's `mmap` uses) and keeps them.
  A machine that runs out ends whichever task touched the page it could not
  give (SIGBUS), not the biggest. Reading an untouched page gives it a frame
  of its own, where Linux maps one shared page of zeroes. Rust programs' heaps
  still come from `SYS_MMAP`, backed at once.
- `fork` copies every page the caller owns, eagerly, and a threaded program
  cannot `exec`: POSIX has it end every other thread, and ending them means
  unwinding what they hold in a server, so it is refused rather than half done.
- There are **no signals**. A terminal's Ctrl-C reaches the program in it as a
  byte rather than as a signal, there are no process groups for one to go to,
  and a task that faults ends with the negated Linux signal number as its exit
  status because that is the only place a signal number means anything here.
- A pty's window size is stored and nothing is told when it changes: Linux
  sends `SIGWINCH`, and there are no signals. A program that draws itself to
  the terminal's size reads it once.
- The clock is read once, from the CMOS clock at boot, as UTC. Nothing sets it,
  and there is no time zone.
- The page cache for mapped files holds 8192 pages across 256 objects and
  nothing evicts them under pressure: a mapped file's pages stay until nothing
  maps the file any more.
- A thread starts with a copy of what its creator holds — capabilities and
  descriptors, poll sets and sockets excepted — not a share of it: what either
  is given or closes afterwards, the other does not see. A pipe end that was
  open when a thread started stays open until that thread closes it or exits.
- **SMP.** The kernel is uniprocessor and several invariants depend on it —
  `IrqSpinLock` panics on contention precisely because on one CPU contention
  can only mean lock re-entrancy.
