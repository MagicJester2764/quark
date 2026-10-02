# Working on Quark

Quark is an x86-64 microkernel, and this repository is the kernel and nothing
else. It is one of seven that build together and must be checked out as
siblings:

```
repos/
  quark/       this repo — the kernel, its two loadable modules, and the ABI
  quarkutils/  everything that runs on it: the runtime, init, the drivers, the
               servers, the C library, the shell and the programs
  bang/        UEFI bootloader, and nothing else
  quark-toolchain/  the cross compilers: gcc, binutils and musl for Quark
  explosion/   a distro: stages the other three and assembles the image
  gnu-quark/   another: this kernel, the least of quarkutils that boots, and
               GNU's programs built unpatched on top
  rust/        fork of rust-lang/rust carrying the x86_64-unknown-quark std PAL
```

**Read `../quarkutils/CLAUDE.md` before touching anything outside this tree.**
The screen, the filesystem, the C library, the toolkits and every server's
rules are written down there. This file is about ring 0.

The dependency runs one way: a distro reaches down to the kernel, the userland
and the bootloader, and nothing reaches up. The kernel and the userland do not
reach sideways either — neither names the other's checkout, and neither builds
against the other's source. What crosses between them is what this repository
*installs*, and a distro's stage directory is the only place the two meet.

GNU/Quark is why a good deal of this kernel is as it is. Its rule is that
nothing in a GNU program is patched, so what bash and coreutils needed and
did not find, the kernel grew: descriptors a program keeps across `exec`,
signals, a terminal's line discipline, process ids, an alarm.

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
  `libc/include/quark/syscall.h` if C needs it — in *its* repository, with the
  version it says it was written for. The stage fails until the two agree,
  which is the point: at one version the two tables are the same table, so a
  call added without a new minor is refused by the userland's check, and a
  userland built for another major is refused by `init` at boot.
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
because that is the compiler it is tested with, and because equal pins are
one download. `../explosion` carries it too, for its own programs. Move them
together.

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
they are handed). A module is found by its name in either case
(`modules::find`): the name is a file's on a FAT partition, and a system
installed by its own tools has `INIT.ELF` where an image built on another
machine has `init.elf`. The drivers that matter — disk, keyboard, network —
are ordinary programs in `../quarkutils`.

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
program. `dtest` in `../quarkutils` makes 716 checks — capabilities, IPC,
memory, descriptors, signals, scheduling, users and terminals, `dtest calls`
with three million calls in three seconds, `dtest smp` for what a second
processor changes, `dtest clock` for what time it is and whether a wait ends
when it should, and seven more (`dtest msi`) on a machine with a device that
interrupts by message and its driver running — and `qfuzz` throws random
requests at every service.

So a kernel change is verified by booting an image:

```bash
cd ../explosion
make hd                                   # or hd-ext4
tools/boot-test.sh <keys-file> <shot.ppm> # type `dtest`, screenshot the result
tools/check-rootfs.sh hdimage.bin         # e2fsck on what the boot left
```

**On one processor and on several.** `SMP=4 tools/boot-test.sh …` gives the
machine four, and a kernel change is not verified until it has passed on
both: one processor is still a machine people have, and it is the only one
on which nothing runs at the same time as anything, which hides and shows
different mistakes. Under KVM the four are four real processors, and a race
is a real race.

A fault prints to serial: `[UPFAULT ...]` or `[UFAULT ...]` for ring 3, which
ends the program, and `[KFAULT ...]` for ring 0, which halts the machine. A
failed check prints to the screen. Look at both.

A kernel fault says three things: where (`rip`, `rsp`, the task and its kernel
stack), the registers, and `calls` — every word on the kernel stack that is an
address in the kernel's own code, innermost first, as an offset from `rsp`.
The kernel is built without frame pointers, so that list is the backtrace,
with the odd stale entry from a frame since left;
`nm -n target/x86_64-unknown-none/release/quark` says which function each is
in. It is what turned `rip=0x1029`, which names nothing, into "in
`timerfd::tick`, from the timer interrupt, with the direction flag set".

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
- **The kernel's own map of memory is made once, at boot, and is all of
  PML4[0] but its last gigabyte.** `boot.s` maps four gigabytes;
  `paging::map_all_memory` extends that over all the memory the firmware
  reports, before the heap exists and before any address space is made —
  because every address space is made with a *copy* of the table under
  PML4[0], and a gigabyte mapped afterwards is one that only the kernel's
  own table has. The heap is the last gigabyte of that entry (`heap.rs`):
  it used to sit at four gigabytes, just past a map that ended there, which
  is where a machine's fifth gigabyte of memory is. What the kernel can
  touch as itself ends at `paging::identity_end()`, not at a constant: a
  check against `1 << 32` is a signal that is silently not told on a
  machine with more memory than that.
- **Ordinary memory comes from the top, and memory a device is told the
  address of from below four gigabytes** (`pmm::alloc`, `pmm::alloc_low`).
  A network card's ring is named in a register thirty-two bits wide; given
  a frame above that, it is handed half an address and writes somewhere
  else. `SYS_PHYS_ALLOC` takes a flag for it, and a driver with a device
  that does DMA passes it. `dtest frames` holds the kernel to both ends;
  nothing holds a *driver* to asking — the network test fails on a machine
  with six gigabytes when it does not, and passes on every machine with
  four or fewer.
- **`paging::OWNED` (PTE bit 9) decides what may be freed.** Only frames the
  address space owns go back to the allocator. Device MMIO, shared memory and
  frames another task still holds are mapped *without* it. An owned frame is
  mapped in exactly one place — that is what makes freeing it on unmap safe —
  so a new mapping path either leaves the bit off or moves the page, as
  `sys_addrspace_give` does, rather than copying the mapping.
- **A scrap of memory between the firmware's own is not used**
  (`pmm::scrap`). A restart does not clear memory, and a firmware that
  reads a page it did not keep for itself finds what the last system left:
  OVMF reads such a page, in the megabyte it starts from, for what a
  confidential guest's loader would have said, and after `shutdown -r` took
  a program's text for a count of 116 processors and waited seventy-one
  minutes for them. Whether it did depended on what happened to be in the
  page — of the two installations in one acceptance run, the one on four
  processors restarted and the one on one did not — so a restart that
  "sometimes hangs in the firmware" is this, and the firmware's own log
  says so (`-debugcon file:fw.log -global isa-debugcon.iobase=0x402`).
  The allocator gave out the lowest free frame then, so what was low was
  used first and was always dirty; ordinary memory comes from the top now,
  and what is low is still what a driver's device and the kernel's own
  tables are given.
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
- **An object is kept for whoever was promised it.** A pager gives a program
  a capability and the program maps with it: two steps by two programs, and
  the object's last mapping can go between them. So `CTL_RELEASE` answers
  "later" while a living task of another program holds a capability for the
  object (`cap::memobject_held_elsewhere`), and the pager asks again. It
  released, and the capability named nothing: two threads mapping one file
  on two processors told each other there was no memory, a few times in a
  hundred. Capabilities are looked for there and not counted — they are
  plain words in a table, written from half a dozen places.
- **A reserved page is a non-present entry with `paging::MARKER` set**, in a
  page table or, for 2 MiB at once, a page directory. It is not empty: the
  walks that free tables and the checks that an address is free test for an
  all-zero entry, not a clear `PRESENT` bit, or they throw reservations away
  and free tables still in use. The kernel backs a reserved page before it
  touches it (`validate_user_range`, the futex path), and a page fault on one
  is served, so the first touch from anywhere gives it its frame.
- **A fault in ring 3 ends the program, never the machine.** Every task of
  the program that faulted exits with the negated Linux signal number
  (`idt.rs`), and only a fault taken in ring 0 halts. musl's `abort()` is a
  privileged `hlt`, so before this, one failed assert stopped everything. The
  program and not the task: a thread that faulted and went alone left the
  rest parked on whatever it held.
- **A negative exit status is the kernel's to give.** What a program ends
  itself with is kept as its low eight bits, by every call that ends one;
  a fault or a kill is the negated signal, and nothing a program passes can
  look like one. `SYS_EXIT_PROGRAM` took its argument whole for a long time
  — it is the call a C program's `exit` makes — so `return -1` from `main`
  was reported as a hangup, and a program that wanted to be believed killed
  had only to say so. A sweep that runs every program with arguments nobody
  would give it found it, in a terminal that could not find its display.
- **Who a task is, is said by a holder of `SetUid`, and about somebody else
  only while they are asking.** A task has a user, a group and up to sixteen
  groups besides; they are inherited, copied by `fork`, kept by `exec`, and
  the kernel acts on none of them except to let one user's task end
  another of the same user's, and to say whose a terminal nobody has
  claimed is. `SYS_IDENTIFY` is how a *server* says them: of
  a task in a call to it, or of a child that task has made and not started,
  with the kernel checking which at the moment it acts. A server that
  checked a TID's parent and then called `SYS_SET_UID` would be naming a
  number, and numbers are recycled. There is no setuid bit and there cannot
  be one: a program is loaded by whoever starts it.
- **A terminal's slave is for its session, not for whoever holds a
  descriptor for it.** `pty::slave_is_for` is asked at `SYS_PTY_OPEN`, at
  every read and write of a slave, and when a slave is used to change the
  terminal: a member of the session that has claimed it; the user who made
  the pair, while no session has; a holder of `TaskMgmt` for every task.
  A descriptor is inherited by everything a session starts, so a program
  that outlived its session — left running by somebody who logged out — went
  on holding the console's: the next person's keystrokes were its to read,
  their password among them, and a slave could be opened by its number by
  anybody at all. Unix takes the descriptor away at a hangup; here the
  question is asked each time, and the answer changes when the session
  ends. A new way to reach a slave asks it. It works only because each
  login is a session of its own (`login` in `../quarkutils` begins one, and
  ends with it): one session for as long as the machine is up is one that
  everybody who ever logged in is a member of.
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
- **The kernel runs with its own flags, whatever it was entered with.** The
  processor delivers an interrupt or a fault with RFLAGS as the interrupted
  code had them, and two of those bits are the kernel's business. *DF:*
  compiled code assumes string instructions run forwards, and a C library
  sets the flag for as long as a copy that must run backwards takes — musl's
  `memmove` is `std; rep movsb; cld` — so a tick or a page fault can arrive
  with it set. Entered that way, a tick's first `memset` ran down the stack
  over its own return address and the machine halted at `rip=0x1029`, about
  once in ten starts of a GTK program; a page fault cleared the frame *below*
  the one it was handing out, somebody else's, and handed the new one over
  with what its last owner left in it. *AC:* it suspends SMAP, and ring 3 can
  set it with `popfq`, so a program could have every interrupt and fault it
  took handled with the protection off. Both stubs in `idt.rs` clear both
  before anything compiled runs (`cld`, and `clac` where SMAP is on),
  `SFMASK` does it for `syscall`, and `_start` clears DF for the boot. A new
  way into the kernel does the same. `dtest flags` takes page faults and
  ticks with DF set; nothing outside the kernel can see AC being cleared, so
  that one is kept by reading the stub.
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
- **A descriptor is its program's, and every change to the table is one
  step.** Every task of a program uses one table (`fdtable.rs`), so a sibling
  can be preempted half way through anything. "Find a free number and fill
  it" is `fdtable::install`, with interrupts off across both halves; reading a
  descriptor and taking a reference for a copy of it is `get_retained`. A call
  that looks a descriptor up and then acts on it in two steps has a window in
  which a sibling closes it and somebody else is given the slot.
- **A task about to wait on what a descriptor names holds it** (`fdtable::hold`,
  given back by `unhold`, or by `task_gone` for a task killed where it
  waited). Without it a sibling's `close` frees the pipe under a parked read,
  and the read wakes up in whatever takes the slot next — another program's
  pipe.
- **A reference to what a descriptor names belongs to no task.**
  `pipe::retain_fd` and `release_fd` take no owner. Shared memory used to keep
  a count per task, which a table two threads share cannot have: the region
  went with whichever of them died first.
- **A program is its address space, not its task.** `SYS_TASK_SPACE` names the
  program a task belongs to with an id the kernel never reuses, and
  `SYS_SPACE_WATCH` says when its last task has gone. TIDs are recycled; space
  ids are not. Anything in the kernel that remembers a number should remember
  that one — see the budget rule under *What a process is*.
- **A program that has gone is said to have gone, however it went.**
  `ipc::notify_space_watchers` is called where a program's last task dies
  (`note_death`) and where its only task leaves it for another
  (`exec_into`). The second was not there. What a server kept for a program
  that then exec'd — a lock, a file a C library had open for the length of
  one call — it kept until the machine was turned off, and the kernel kept
  the watch: a hundred and twenty-eight commands filled the table, and from
  then on `SYS_SPACE_WATCH` failed for every program and no server heard of
  any of them ending. Nothing showed until a program was ended half way
  through a call — which a second processor made ordinary — and a removed
  directory it had been looking at stayed on the disk. A new way for a
  program to stop being one is a new place to say so.
- **What a watcher is owed is kept until it collects it.** Task deaths are a
  set, one bit a task (`ipc::DEATHS`); program deaths a list as long as
  there can be programs. Both were lists eight long, on the reasoning that
  a watcher eight behind is not doing its job — but one call ends more than
  eight with nobody having had a turn: a signal for a process group ends
  every member of a pipeline. The ninth was not told of. A notice the
  kernel owes somebody needs somewhere to wait that cannot be full, as a
  served descriptor's last close has (`served.rs`: a flag, and a call that
  collects); a pager's idle objects are still a list, thirty-two long.
- **A call from the kernel to a pager carries `PAGER_BIT` in its sender**, and
  nothing else can: the bit is set by `call_as` and by no syscall. The reply
  strips the bit and reaches the faulting task.
- **Only the kernel sends as sender 0.** A death notice (`TAG_TASK_DIED`, or
  the one for a program) is a message from sender 0, and no syscall can forge
  that; any program can send the *tag*. Servers depend on the difference.
- **`init` is started with the framebuffer and its boot modules, and nothing
  wider.** Every other `PhysRange` over memory is derived from those, so
  what the kernel hands the first task bounds what any task can map.
- **A `PhysRange` over a device's registers is minted from `DeviceMemory`,
  and only where there is no memory.** *Device memory* (`devmem.rs`) is
  every address below four gigabytes that the firmware's memory map does
  not list at all, less the first megabyte and the interrupt controllers'
  own pages — a program that could write to a local APIC could stop the
  clock. The capability is a kind of its own so that no task holds a
  `PhysRange` wider than what it maps: a driver reads where its device is
  and mints that (`cap::can_mint`). It is worked out from the *whole* map
  or not at all: a hole in what the kernel kept of the map is not a hole in
  the map, and RAM handed out as device memory is the kernel handed out.
  The map is kept whole by joining neighbours of one kind as it is read
  (`multiboot2::parse_memory_map`); kept one for one, a UEFI machine's
  hundred entries did not fit in sixty-four and the last of its memory was
  never seen. `init`'s capability goes in its *last* free slot: it names
  its low ones itself and counts on the rest of them being empty.

## Descriptors the kernel owns

A **pseudo-terminal** is a kernel descriptor, for the same reason a pipe is: a
terminal emulator waits on its master with `poll`, and readiness the kernel
cannot see is readiness `poll` cannot report. `/dev/ptmx` makes a pair and
answers with the master, `/dev/pts/N` opens its slave, and both paths are
caught in the C layer (`../quarkutils/linux-abi`) ahead of the VFS. The line
discipline is the part programs depend on and no more — echo, canonical input
and the characters a line is edited with, end of file, the newline
translations, and the interrupt character taken out of what is typed — and
the rest of a `termios` is stored and handed back unchanged. What is typed is
UTF-8 (`IUTF8`, set on a new terminal): erasing takes back a character, the
byte that begins it and every byte that continues it, and not the last byte
of one.
What a program prints waits for room when the terminal is full, all of it: a
write that came back short, or with nothing, is what a full disk looks like,
and `cat` said so. What a terminal emulator types does not wait, because its
echo comes back at it and a wait there is a wait on itself.
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

And **a file is one**, though the file is not the kernel's. A *served
descriptor* (`served.rs`) names an object in a server by a number the server
chose; the kernel counts who holds it and nothing more. That is what makes a
file an ordinary descriptor — copied by `fork`, kept by `exec`, put where a
program's standard output was — instead of a number each C library made up
and kept in memory that `exec` throws away. Three things hold it together:

- A server gives one only to a task that is calling it (`SYS_FD_SERVE`), and
  believes a request that names a cookie only after asking whether the caller
  holds it (`SYS_FD_HOLDS`). There is no other way to come by one.
- The last close is told to the server as a flag and collected with a call
  (`SYS_FD_REAP`), so a busy server loses none. A queue here would be a place
  to drop a file's last close.
- A read or a write through one is a call the kernel makes for the task
  (`ipc::served_call`), with the task's buffer lent. The task needs no
  capability for the server: the descriptor is the permission, as it is for a
  pipe.

A **named pipe** is the other thing a file server hands out, and it is not
a served descriptor: it is a pipe. The name, its owner and its mode are the
server's; what a program reads, writes and polls is the kernel's, the same
object `SYS_PIPE_CREATE` makes. `SYS_FD_SERVE_PIPE` joins them — a server
gives a task that is calling it an end of the pipe a key of the server's
names (`pipe::open_named`), for as long as anybody holds an end.

- **Finding the pipe, counting the end and looking at the other are one
  step.** The ends are opened one at a time by programs that have not met,
  and each usually waits for the other (`SYS_PIPE_PEER`). What it waits for
  is an *opening*, counted, since the moment it was given its end: a writer
  that opened, wrote and closed before the reader ran again has still been,
  and a wait for "a writer is there" would outlast it.
- **A copy of an end is not an opening of it.** `dup` and `fork` go through
  `add_ref`; only `open_named` moves the count.
- **A reader with no writer yet has not been hung up on** (`pipe::ended`).
  `poll` says a named pipe has ended only once a writer has been; said
  sooner, a program waiting for its first writer spins.

Descriptor 64 — one past the ordinary numbers — is the program's working
directory, a served descriptor like any other. It is in the table so that it
follows a program through `fork` and `exec` with no server being told.

Adding a kind means touching every place that enumerates them, and missing one
is quiet: `FdKind` in `task.rs`, read and write in both their blocking and
non-blocking forms in `syscall.rs`, `pipe::release_fd` and `pipe::retain_fd`
(a kind in one and not the other leaks or double-frees), `fdtable::hold`'s
list of what a task can be parked on, and `pollset::watchable` and
`readiness` — `poll` answered `POLLNVAL` for a terminal until the last of
those knew about it.

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
- **`fork` copies the descriptor table; `exec` keeps it.** The child gets a
  second descriptor for everything the parent has open, the working directory
  included; `exec` closes what was marked for it (`SYS_FD_FLAGS`) and nothing
  else. A thread does neither: it uses its program's table.
- **A program ends as a whole** (`SYS_EXIT_PROGRAM`). `SYS_EXIT_CODE` ends one
  task, which is what a thread wants and never what `exit` means: the other
  threads stayed parked on locks nobody would release, holding the program's
  descriptors, and a compositor never heard its client go.
- **And is ended as a whole.** A kill from outside a program
  (`scheduler::kill_program`: `SYS_TASK_KILL` of another program's task, the
  kill signal, the deadline a signal carries) and a fault both end every task
  in the address space. `kill_task` is the one-task form, for a program
  ending a thread of its own. The first was the only form for a long time,
  and a compositor that ended a toolkit client by its first task kept three
  of its threads.
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
- **And so is whoever is waiting on it.** `ipc::fail_waiters` runs where a
  task is marked dead (`exit_with`, `end_other`), beside the release of its
  descriptors and for the same reason: a dead task waits for its parent, and
  a parent in a call to it is not collecting anybody. `mount` starts a file
  server and calls it to see whether it found a filesystem; one that found
  none exits without answering, and the two waited on each other for ever —
  unless the server was already dead when the call was made, which fails at
  once. It was a race, and it was won in every test until a user's `mount`
  lost it. Reaping does it again for a task that died some other way.
- **A descriptor sent over a stream outlives the sender's end.** It is in the
  stream rather than in the sender, and the peer can still take it.
- **A thread joined through a word is nobody's child**
  (`scheduler::joined_by_word`): a task made by a task of its own program,
  with a word for the kernel to clear when it ends (`SYS_SET_CLEAR_TID`).
  That is every thread a C library makes, and the word is how it is joined.
  No wait answers with it or counts it, nothing is woken for it, and the
  kernel takes it apart itself (`UNWAITED`) — at the next door
  (`arrived`), not when a processor next has nothing to do, and the same
  for a dead task whose creator has gone. Left as its creator's child, an
  ended thread kept its place among sixty-four until its program ended, a
  `waitpid(-1)` could be answered with one, and a program whose only
  "children" were its threads was told to go on waiting. The creator gives
  the word, before the thread is started: one that gave its own was a
  child until it first ran. A thread with no word is still waited for,
  which is how this system's own runtime joins one.
- **A process is named by a number that is never used twice** (`SYS_PID`). A
  task id is a slot, and the next task made is given the lowest one free —
  usually the one just let go. Every Unix program that remembers a child
  assumes a number it was told a moment ago is not somebody else by now:
  bash decides whether to wait for a command by
  comparing its pid with the last background job's, and after `sleep 2 &`
  every command that was given that task id ran unwaited-for. A process id
  is the endpoint number of the task the program began as — assigned when
  the slot is filled and never given out again — kept per task, shared with
  the program's threads and left alone by `exec`. `SYS_WAIT_FOR` and
  `SYS_SIG_RAISE` take either and are told which by a flag; everything else
  in the ABI takes a task id, as before. The C layer's `getpid`, `fork`,
  `wait4` and `kill` speak process ids and its `gettid` a task id; `ps`
  shows both.

## Signals

A signal is said to a program (`signal.rs`), and what the program has said
about each one lives in its descriptor table's record — because `fork` copies
that and `exec` keeps what is ignored, which is how a shell starts a
background job that ignores what its terminal raises.

- **The kernel runs no handler.** Nothing is pushed on a user stack and
  nothing is returned from. A program that has said nothing is ended, here
  (`scheduler::end_program`, with the negated signal number, as a fault does).
  One with a handler is *told*: the signal is recorded as waiting, a word in
  the program's own memory is set through its page tables, and one wait is
  ended early. Its runtime takes what is waiting (`SYS_SIG_TAKE`) and calls
  the handler as a function. Making the kernel deliver one — a frame, a
  trampoline, a return — is a change of design, not a fix.
- **One signal ends one wait: the first to look.** `fdtable::sig_interrupted`
  is true once. Held as a level — "a signal is waiting, do not wait" — it
  turns every loop that sleeps and looks again into a spin for as long as the
  program has not taken the signal, and three of those loops are in the
  kernel: a poll sleeps by receiving from its own id.
- **Asking whether to wait and parking are one step**, with interrupts off
  (`pty::wait_readable`, `ipc::sys_recv_timeout`). A signal raised between
  the two finds nobody parked, and the wait outlasts it.
- **A signal ends only a wait that looks again when it is woken**: a read of
  a terminal, a poll, a sleep, an open of a named pipe. A call to a server is not one — woken with no
  reply, it fails — so `signal::wake` reaches for sleepers and terminal
  readers by what they are, and never for a task by its state.
- **Ctrl-C is for the group in front of the terminal**
  (`signal::from_terminal`), and so are Ctrl-\ and Ctrl-Z: see *Jobs*. A
  terminal no session has claimed has no group in front, and there the
  signal is for every program holding the slave. Either way a shell that
  does nothing about groups is in one group with what started it and what
  it starts: `login` and `qsh` both hear Ctrl-C — and `getty`, which keeps
  the terminal between sessions, hears it when no session has it — and each
  says what it does about signal 2. A new program that holds a session's
  terminal and is not what the session runs has to say so too, or Ctrl-C
  ends it.
- **Two signals are raised by the kernel of its own accord**, because nothing
  else can raise them: SIGALRM when a program's alarm is due
  (`SYS_SIG_ALARM`; the alarm is in the program's table, so `exec` keeps it
  and `fork` does not copy it), and SIGCHLD for a program when a task that is
  a child of it — its parent in another program, so not a thread — dies. A
  program that waits for a child *or* a time, whichever comes first, is
  woken by the one that came; before, GNU `timeout` sat in `sigsuspend` for
  ever. The alarm is raised by the clock (`clock::expire`, from the tick
  and from the timer between ticks), one program at a time, each alarm put
  away before its signal is raised: for a program that has said nothing the
  signal is the end of it, and if that is the program the clock interrupted,
  `signal::alarms` does not return. So it is the last thing the clock does,
  and what it does before — setting the timer for what is due next — counts
  the alarms as they will stand afterwards (`fdtable::alarm_after`).

## Jobs

A shell with job control puts each pipeline in a *process group*, says which
group is in front of the terminal, and is told when one stops. All of that
is here (`job.rs`), because none of it can be anywhere else: who a typed
character is for is decided where the terminal is, and a program that is
stopped is one the scheduler does not run.

- **A group and a session are named by process ids**, which are never used
  twice, and are kept by task beside the process id (`job::PGID`, `SID`) —
  a parent asks about a child after the child's program has gone. A task is
  born where its creator is; a thread is where its program is; `exec`
  changes nothing.
- **Stopped is not a state.** A task of a stopped program goes on being
  what it was — blocked in a call, asleep, ready — and has to be that when
  the program is continued. It is *held* (`scheduler::HELD`): never put on a
  ready queue and never switched to. `enqueue` and `donate_to` are the two
  places a task becomes runnable, and both ask. What would have woken it
  leaves it `Ready` and in no queue; continuing queues every task that is.
  A new way to make a task run has to ask too.
- **The running task can be the one stopped** — a program that stops
  itself, or reads a terminal from behind. It is held with the rest and
  stops at `scheduler::stop_here`, which whoever raised the signal calls
  once it has nothing left to finish. So a stop is raised for the caller's
  own program *last* (`job::raise_for_group`), like a signal that ends it.
- **A group with nobody to continue it is not stopped from a terminal.**
  That is the orphaned-group rule (`job::orphaned`), and it is what makes
  Ctrl-Z harmless at a shell that runs its commands in its own group: the
  shell, the command and the login that started them would otherwise all
  stop, with nobody left to type `fg`. SIGSTOP stops regardless.
- **A job left stopped by the death that orphans it is hung up on**: SIGHUP
  and SIGCONT. Not from inside the death — that is somebody in the middle
  of ending a program, and a hangup can end the caller's own — but from the
  next tick (`job::hang_up`), which is already where an alarm may end
  whatever was running. A stopped program nobody can start is a task slot
  gone for good, and there are sixty-four.
- **A terminal knows its session and who is in front** (`pty.rs`). Its
  signals go there. A read by any other group of the session stops the
  reader (SIGTTIN) and is asked again when it is continued — and looked at
  again every time round, because a job that was in front when it began to
  wait may have been stopped and put behind since. `job::resume` takes a
  continued task off a terminal's wait list for that reason.
- **A parent hears of a stop the way it hears of a death**: SIGCHLD, and
  `SYS_WAIT_FOR` if it asked. The answer for a stop is marked and collects
  nothing. A waiter is woken to *look again* (`WAIT_AGAIN`), not handed a
  child: the report is taken by whoever looks first.
- **The kernel cannot see a signal mask.** A shell takes its terminal back
  from behind with SIGTTOU blocked, which on Unix is what stops it being
  stopped for asking. Here the runtime says so in the call
  (`PTY_FRONT_QUIETLY`). Anything else that should treat a blocked signal
  differently needs telling the same way.

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

A system call runs with interrupts on, so anything the scheduler does in more
than one step is a tick away from being done in half. Three of these were
found by reading, and each was shown by holding its window open — a loop of
a few hundred million iterations where the tick had to land — before it was
closed:

- **Nothing comes between a task being marked dead and the switch away from
  it.** A dead task is never run again, so one preempted there left the rest
  of its exit undone for good: nobody was told it had gone, and a parent in
  `sys_wait` waited for ever. `exit_with` turns interrupts off first, as
  `end_other`'s callers always did.
- **A switch reads where it is going before it restores the flags.**
  `context_switch` used to `popfq` and then read the new RIP out of the
  task's context — one instruction with interrupts on. An interrupt taken
  there that rescheduled saved this same task over what was about to be
  read, and resumed, the task went to the resume label instead. For a task
  switched out from there that is where it was going anyway. For one that
  had never run it was not: it returned into the trampoline that ends a
  task, and a program just started exited with status 0 having run nothing.
  The RIP is pushed first now, so an interrupt in the window finds it on the
  stack.
- **The ready queue is touched only with interrupts off.** Putting a task on
  it is three writes; a tick between them queued the task it preempted in
  the same place, and the one being queued was ready and in no queue.
  `start_task` did that from a system call. Every caller of `unblock_task`
  holds interrupts off, or is an interrupt.

## Power

`power.rs`, and what `acpi.rs` reads for it. Two things that are quiet when
they are wrong:

- **The value that means "off" is found, not computed.** It is an object
  named `\_S5` in the machine's table of methods, which is a program, and
  the kernel has no interpreter: `acpi::s5` looks for the name being given
  to a package of small numbers, and takes the first two. A table that says
  it another way — the name built at run time, the numbers computed — is a
  machine `SYS_POWER` cannot turn off, and it answers so before anything is
  stopped. `shutdown` then writes the ports it always wrote.
- **Turning off stops the other processors first, and they stay stopped.**
  If the machine is still there afterwards the call comes back with a
  failure, on one processor, to a caller that was ending everything anyway.
  A new use of `power::off` that expects to carry on afterwards cannot.

Starting again never comes back: where the tables name no reset register —
OVMF's, for the machine QEMU pretends to be, name none — it is the keyboard
controller's reset line, and then a fault the processor cannot deliver.

## Time

`clock.rs` is the whole of it, and `docs/abi.md` says what a program sees.
The rules it leaves behind:

- **A time the kernel keeps is nanoseconds since boot, by `clock::now`.**
  Never a count of ticks: `pit::ticks` counts interrupts, for the scheduler
  and for a machine with no finer clock. 0 is "no deadline" wherever one is
  kept, and `clock::after` never answers it.
- **A span of time from a program goes through `clock::span`**: ticks, or
  with the top bit set nanoseconds. A new call that takes a time takes a
  span — that is why no call's number had to change, and why a program
  written for ticks still means what it meant.
- **Whoever writes a deadline down says so** (`clock::due`), after it has.
  Nothing else sets the timer between two ticks: a deadline nobody
  mentioned is seen to at the next tick, which is every test passing and
  every wait ten milliseconds long. `dtest clock` asks that most of twenty
  waits end on time, not that one does — a wait that ends on a tick is on
  time once in a while, by where in the tick it began.
- **Everything about time passing is `clock::expire`.** A new kind of
  deadline is looked at there and answers with its earliest still to come,
  so that the timer is set for it. Alarms last: raising one may not return.
- **A wait never ends early.** The timer is set a thousandth late on
  purpose (`lapic::one_shot`): an interrupt before the time finds nothing
  due and has to be taken again.
- **The timer interrupts no more often than every fifty microseconds**
  (`clock::MIN_GAP_NS`), whatever is asked. A timer that repeats every
  nanosecond is a program's to ask for and not the machine's to be stopped
  by; it counts the intervals that went by.
- **How many times a timer has fired is a matter of what time it is**, not
  of when the clock last looked: a read counts as of the read
  (`timerfd::catch_up`). Only a read — it takes what it counts, so nobody
  is owed a wake for it. Whether a timer is *readable* is the clock's to
  say, because saying it is what wakes whoever is waiting.
- **Setting the date moves no deadline.** Every wait is by the time since
  boot; the date is that plus a number (`clock::wall`), and
  `SYS_CLOCK_SET` changes the number and writes the battery-backed clock.
- **The counter is trusted where the processor says it is invariant, or
  under a hypervisor**, and its rate is measured once against the 8254's
  second channel, by reading both — no interrupt, so before the first tick,
  so that the clock and the count of ticks start together. The middle of
  three measurements: a guest's processor is sometimes somewhere else for
  one.

## More than one processor

`docs/smp.md` is the design. These are the rules it leaves behind, and
breaking any of them is quiet until it is a machine that stops.

- **One processor is in the kernel at a time** (`klock.rs`), and that is
  what keeps every other rule in this file true: "interrupts off" still
  means nothing else is in here. The lock is taken at the kernel's three
  doors — `syscall_dispatch`, `exception_handler`, `irq_handler` — and by
  nothing else; it is the *processor's*, carried across a switch; and it is
  given up on every way out to ring 3 and by the idle loop around its `hlt`.
  A handler remembers in its own frame whether it took the lock
  (`klock::enter`, `leave`) and gives back exactly that. A new way into the
  kernel takes it; a new way out — a new trampoline to ring 3 — gives it
  up. Either mistake panics rather than hangs: the lock knows who has it.
- **"Which processor is this" has an answer only with interrupts off.** A
  task in a system call is moved wherever it can be preempted. Nothing reads
  `percpu::index()` and acts on it later. `percpu::current()` is one
  instruction through GS, and is right whenever it is asked.
- **A task that is not the caller may be running.** In ring 3, on another
  processor, while the kernel ends it or stops it. So: it is marked, its
  processor is interrupted (`smp::interrupt`), and every task is looked at
  again at each door, with the lock held (`scheduler::arrived`) — one that
  was ended goes no further. A new door calls it.
- **A dead task's state does not say it has stopped running;
  `scheduler::ON_CPU` does.** Ended from another processor, a task is on its
  kernel stack and in its address space until its processor leaves it. Its
  parent is told only then (`UNANNOUNCED`, `announce`), `reap_one` will not
  take it apart before, and `space_in_use` — what is asked before an address
  space is thrown away — counts it. Anything new that frees what a task
  stands on asks `ON_CPU`, not `TaskState::Dead`. `SYS_ADDRSPACE_DESTROY`
  asked only whether a task was alive.
- **A mapping taken away is taken away on every processor** (`tlb.rs`),
  before its frame can be anybody else's (`pmm::alloc` settles first) and
  before the kernel lock is given up (`klock::release` does). The three
  functions that clear or replace a present entry — `map_page`,
  `unmap_page`, `clear_range` — say so (`tlb::stale`); anything new that
  does must too, or a thread on another processor goes on writing to a
  frame that is now another program's. Adding a mapping where there was
  none needs nothing.
- **What one processor asks of another without the lock is answered without
  it**, and from the wait for the lock as well as from an interrupt
  (`smp::while_waiting`): whoever asks is holding the lock, and whoever is
  asked may be waiting for it with interrupts off. Forgetting translations
  and halting are the only two. An interrupt that needs the kernel takes the
  lock like everything else.
- **A page fault the tables no longer agree with is not a fault**
  (`paging::permits`). Two threads touch a new page at once; one is given
  it; the other's fault is heard after. Only for a fault taken in ring 3: in
  ring 0 the same check would turn the kernel touching what it should not
  into a loop.
- **A processor says it is going to sleep before it gives up the lock**
  (`percpu::nap`), so that whoever next makes a task ready — which takes the
  lock — knows to wake it (`smp::wake_idle`), and stops saying so the moment
  it runs anything. `unblock_task` wakes one. `unblock_task_next`, which is
  a reply to a call, wakes nobody: the answerer is about to wait, and the
  caller runs in its place.
- **The clock and every device interrupt the first processor.** The others
  have a tick of their own from their local APIC, and it does one thing:
  `scheduler::timer_tick`. What is due — timeouts, timers, alarms — is seen
  to in one place, by the first processor (`clock::expire`): on its tick,
  and between ticks from its own local APIC's timer, which it has no other
  use for. A deadline written down on another processor that is due before
  the next tick is said to the first with an interrupt (`idt::VEC_CLOCK`),
  an ordinary one, which takes the lock.
- **Every processor's counter reads the same, or the counter is not the
  clock.** Time is one counter read on whichever processor is asked. Each
  processor's is compared with the first's as it is started (`smp::start`),
  and one that disagrees puts the whole machine back on the tick
  (`clock::distrust`) — before there is a task to have been told a time.
- **A device's interrupt is spoken of by its ISA number, and the controller
  is spoken to through `intc.rs`.** It is the I/O APIC where the firmware
  lists one and the 8259s where it does not, and the two differ in the one
  thing a driver in user space leans on: what keeps a device that holds its
  line from interrupting again before its driver has run. An 8259 that has
  not been told an interrupt is over delivers nothing of that importance
  or less. A local APIC does the same by vector, and every device's is in
  one class — so there the end of the interrupt is said at once
  (`intc::held`), and a line that is a level is masked until the driver
  answers (`SYS_IRQ_ACK`, `intc::ack`). An edge is never masked: the I/O
  APIC forgets one that arrives while it is, where the 8259 remembered.
  Which lines are levels is the firmware's to say (the MADT's overrides).
  A new place that takes an interrupt says one of `done`, `held` or
  `dropped` about it, and never writes to a controller itself.
- **Interrupts 16 to 47 are no controller's lines**: they are messages a
  device sends straight to a local APIC (MSI), given out one driver each
  (`SYS_MSI_ALLOC`, `irq_dispatch::allocate_message`) and taken back when
  the driver is gone. `intc` says nothing to any controller about one but
  that it was taken. Nothing in the kernel can stop a device sending — that
  is the device's switch, in configuration the kernel does not read — so a
  number given out again can still be sent to by whatever had it before,
  and the kernel tells the new driver: a stray, which a driver has to
  expect. `SYS_IRQ_REGISTER` is for the sixteen and refuses the rest, or a
  driver with the capability for any line could take another's.
- **The other processors are started before there is a task** (`smp::start`
  in `kernel_main`), with what the first processor has turned on: CR0, CR4,
  EFER, the `syscall` MSRs. Something turned on later on the first — a CR4
  bit, an MSR — has to be turned on on the others, or a task that moves
  finds it gone.

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
- **Signals are told to a program, not delivered to it** (see *Signals*). So a
  handler runs at a system-call boundary and nowhere else: a program that
  handles a signal and computes without making a call is not interrupted.
  Nothing is raised when a terminal changes size. A signal a
  program has blocked and has no handler for is not held back: the mask is its
  runtime's, and the kernel does what the signal does at once — a blocked
  SIGTSTP stops. A terminal stops a job that reads it from behind and one
  that puts itself in front, and not one that writes to it (`TOSTOP`) or
  changes its settings. A wait for a
  child is not one of the waits a signal ends. And the three *task* signals
  of `SYS_SIGNAL` — bits in one task's notification word, with a deadline —
  are still what a program written for this system is asked to stop with.
- A pty's window size is stored and nothing is told when it changes: Linux
  sends `SIGWINCH`. A program that draws itself to the terminal's size reads
  it once.
- The date is UTC, and there is no time zone: what keeps one is a C library.
  Nothing keeps the clock right once it is set — it runs at the rate the
  processor's counter was measured at when the machine started, which is good
  to a few parts in ten thousand.
- A machine whose processor has no counter that can be trusted — one that is
  not *invariant*, on hardware that is not a hypervisor's — keeps time by the
  tick, ten milliseconds wide, as every machine did. One with no local APIC
  has the fine clock and wakes on ticks.
- A task woken on time runs at once only if it is of a better band than what
  the first processor is running, or a processor is idle: one of the same
  band waits its turn, as any woken task does.
- The page cache for mapped files holds 8192 pages across 256 objects and
  nothing evicts them under pressure: a mapped file's pages stay until nothing
  maps the file any more.
- A thread starts with a copy of its creator's *capabilities*, not a share of
  them: what either is granted or gives up afterwards, the other does not see.
  (Descriptors are shared: they are the program's.)
- A child is the *task's* that made it, not the program's: a thread cannot
  wait for a child another thread of its program forked, which POSIX lets
  any thread do. And the children of a thread that has ended are nobody's:
  no wait is given them, and no SIGCHLD is raised for them.
- A poll set a task is parked on is not held the way a pipe is: a sibling
  closing the set while another thread waits on it leaves that thread to its
  timeout. A one-shot `SYS_POLL` makes a set of its own and is not affected.
- **The kernel is one processor's at a time.** Programs run on every
  processor; a system call, a fault or an interrupt waits for the kernel to
  be empty. So `IrqSpinLock` still panics on contention, and rightly: under
  the kernel lock, contention can still only mean re-entrancy. Taking the
  lock apart is its own project. With it go: a ready queue for each
  processor rather than one for the machine, a task kept where its cache
  is, a better task waking that interrupts whichever processor is running
  the worst rather than waiting for its tick, and an idle processor that
  takes no ticks.
- Every device interrupts the first processor, a message included. The
  I/O APIC's lines above the sixteen ISA interrupts are not used: which
  device is on which is in the firmware's bytecode, not its tables. A
  device that wants an interrupt of its own sends a message. One message
  each: nothing gives a device several (MSI-X, or MSI's multiple
  messages).
- `DeviceMemory` is one authority for all devices, as the I/O ports are: a
  driver that holds it may map any device's registers. Above four
  gigabytes there is none, so a device the firmware put there cannot be
  driven.
- Sixteen processors at most, and local APIC ids below 256 unless the
  firmware left the APICs in x2APIC mode.
