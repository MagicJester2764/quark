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
program. `dtest` in `../quarkutils` makes 1073 checks — capabilities, IPC,
memory, descriptors, signals, scheduling, users and terminals, `dtest calls`
with three million calls in three seconds, `dtest smp` for what a second
processor changes, `dtest turns` for whose turn a call runs on, `dtest idle`
for a machine with nothing to do, `dtest clock` for what time it is and
whether a wait ends when it should, `dtest fork` for what a fork shares and
who a write is seen by, `dtest handlers` for a handler the kernel runs,
`dtest usage` for what a program has used and its share of the processor,
`dtest devices` for who holds which device; twenty more on four
processors (`dtest placement` for where a task is put to wait, `dtest
offline` for a processor taken away and brought back, `dtest scale` for
four pairs of tasks calling at once); twenty-five more
(`dtest pressure`) on a machine with somewhere to write memory out to,
twelve (`dtest msi`, `dtest devices`) with a device that interrupts by
message and its driver running, eight (`dtest iommu`) where an IOMMU stands
between that device and memory, eleven (`dtest usb`) with a keyboard, a
mouse and a disk on USB, three (`dtest display`) where the display can be
had another size, and seventeen (`dtest sound`) with a sound card: 1110 on
the machine ExplOSion tests on, and 1130 on it with four processors — and
`qfuzz` throws random requests at every service. `callbench` says how many
calls a second pairs of threads make, a pair to a processor, and `kstress`
has every processor making calls, taking faults, waking and writing at
once, and checks each.

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

**With an IOMMU and without.** `IOMMU=1` gives the machine Intel's
(`-device intel-iommu`), which wants QEMU's q35 chipset, whose disks are on
AHCI: it boots a disk image there as anywhere, or the live ISO
(`ISO=explosion.iso`), which runs from memory. A change to how memory
reaches a device, or to who owns a frame, is verified there too.

A fault prints to serial: `[UPFAULT ...]` or `[UFAULT ...]` for ring 3, which
ends the program, and `[KFAULT ...]` for ring 0, which halts the machine. A
program's page fault says where it was (`rip`, `rsp`), what the processor
said of the access (`err`) and what the page's entries hold (`pde`, `pte`;
a reservation is a non-present entry with bit 10, `MARKER`, and bit 11 too
where it names an object's page): a stack run past its end, a reservation
lost, or a pointer to nothing. Either kind ends with two words of the code
at `rip`, twelve from the top of the stack and what the program was started
as (`code=`, `stack=`, `name=`): the code's bytes, looked for in the
libraries the program has, say which and where (musl's `a_crash` is a `hlt`,
so a failed assert in its malloc is a `[UFAULT vec=13 ...]`). A
failed check prints to the screen. Look at both. `[KSTUCK ...]` is the third
thing serial can say, and is read as a kernel fault is: one processor has had
the kernel for half a minute, the rest have been waiting at its door, and
this is where the one that had it was.

A kernel fault says three things: where (`rip`, `rsp`, the task and its kernel
stack), the registers, and `calls` — every word on the kernel stack that is an
address in the kernel's own code, innermost first, as an offset from `rsp`.
The kernel is built without frame pointers, so that list is the backtrace,
with the odd stale entry from a frame since left;
`nm -n target/x86_64-unknown-none/release/quark` says which function each is
in. It is what turned `rip=0x1029`, which names nothing, into "in
`timerfd::tick`, from the timer interrupt, with the direction flag set".
It says how much of the stack the task had used (`used=`), and a stack that
ran out says so — `kernel stack overflow, task T name=...` — with its
`calls` read from the stack's bottom, where the calls that did it are.
`[kstack] deepest N of M bytes` on serial is the deepest any kernel stack has
gone, said each time it is passed and at shutdown.

## Invariants that must not regress

These were established deliberately. Breaking one silently re-opens a hole.
The ones that are rules for *programs* — what a spawner must do, what a server
may keep — are in `../quarkutils/CLAUDE.md`; these are the kernel's.

- **Where a program's things go is chosen at random, by whoever chooses.**
  A program names every address it maps, so the choosing is in user space
  — `quark_rt::layout` and the C layer in `../quarkutils`, from
  `SYS_GETRANDOM`: a stack, a heap, threads' stacks, an `mmap` arena, each
  a random number of pages into a window of its own. The kernel chooses
  one address, the first program's stack (`setup_user_stack`), and chooses
  it the same way. A new place the kernel chooses is chosen so too; what
  does not move is a program's code, linked where it runs — but a program
  linked to be put anywhere (a PIE, as Linux's are), which its loader puts
  a random number of pages into a window of its own — and the page its
  arguments are on.
- **User mappings live at or above `paging::USER_MIN_ADDR` (PML4[1]).**
  `create_address_space` deep-copies only PML4[0]'s PDPT and *shares* the page
  directories beneath it, so a mapping below that writes into tables every
  address space shares and promotes them to USER everywhere. It is also what
  makes SMAP safe: no USER bit exists anywhere in the kernel's identity map.
  Validate with `paging::user_range_ok`. **And they end below
  `paging::USER_ADDR_LIMIT`, one page short of 2^47**: a `syscall` in the
  last two bytes of that page goes back to 2^47, which is no address, and
  Intel's `sysret` faults on it in ring 0 with the program's stack pointer
  loaded (CVE-2012-0217) — the page could be mapped until 4.5.
- **The kernel's own map of memory is made once, at boot, and is all of
  PML4[0] but its last two gigabytes.** `boot.s` maps four gigabytes;
  `paging::map_all_memory` extends that over all the memory the firmware
  reports, before the heap exists and before any address space is made —
  because every address space is made with a *copy* of the table under
  PML4[0], and a gigabyte mapped afterwards is one that only the kernel's
  own table has. The heap is the last gigabyte of that entry (`heap.rs`):
  it used to sit at four gigabytes, just past a map that ended there, which
  is where a machine's fifth gigabyte of memory is. Kernel stacks are the
  gigabyte below it (`kstack.rs`), whose page directory is made before
  there is an address space for the same reason. What the kernel can
  touch as itself ends at `paging::identity_end()`, not at a constant: a
  check against `1 << 32` is a signal that is silently not told on a
  machine with more memory than that.
- **A kernel stack has a page below it that faults** (`kstack.rs`): every
  task's, every processor's idle stack and every stack a double fault is
  taken on, in their region with 4 KiB pages, and the first processor's
  boot stack, whose page is split out of the image's two-megabyte pages.
  A stack that runs out faults there, and the report says so. It ran into
  whatever the heap had below it, for as long as there was heap: down
  through other tasks' records, to a double fault at the heap's bottom
  that named a stack that was not the task's. A stack given back is
  unmapped, and every processor told before its frames are anybody else's
  (`tlb::stale_everywhere`: every address space has the region).
  **What a stack holds is kept small**: a record kilobytes long is made in
  its room (`Table::fill_from`, from a template), not built in a frame and
  moved — making a task built its record, its holding and its program's
  record so, and took thirty kilobytes of its caller's stack. A new path
  that builds something that size on the stack is read for that; the
  stacks are sized at twice the deepest the acceptance measures.
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
  mapped in exactly one place, or it is counted: a `fork` leaves a page in
  two address spaces, and the frame keeps how many besides one have it as
  their own (`pmm::share`). `pmm::free` is the one way back for a frame and
  gives it to the allocator only when the last of them lets go — that is
  what makes freeing it on unmap safe. So a new mapping path leaves the bit
  off, or moves the page, as `sys_addrspace_give` does, or counts it;
  it never copies the mapping and says nothing.
- **A page shared since a `fork` is the writer's own before it is written,
  whoever writes it and however it is reached.** What could be written is
  marked (`paging::COPY_ON_WRITE`, bit 10 of a present entry — `MARKER`'s,
  when the entry is not) and is not `WRITABLE` in either address space; the
  write is a fault, and `paging::own` gives the writer a copy, or the page
  back if nobody shares it any more. Nothing a program can see says any of
  this happened. What has to be kept is every way to a page that is not a
  write by its program through its own tables:
  - *The kernel writing for a program.* It owns the page first where it can
    say so first (`paging::back_range`, which is what `validate_user_ptr_mut`
    comes to), and **ring 0 honours write protection** (CR0.WP, set in
    `boot.s`), so a write that was checked, waited, and found the page shared
    again — another thread forked meanwhile — is a fault that is served and
    not a write to the child's page as well. Without WP the kernel writes
    through a read-only entry and nothing says so.
  - *A page reached by its frame.* `signal::tell` sets a word in another
    program's memory, `lend::copy` writes what a server was lent, and
    `SYS_ADDRSPACE_GIVE` moves a page out: each makes the page its address
    space's own first (`own`, and `unshare` for one that is going away).
    Anything new that finds a frame through somebody's tables and writes it
    does the same.
  - *A name for a page.* A futex word is named by its address space and
    address where the program owns the page, and by its frame only where it
    does not (`futex::Key`). By the frame alone, a child's wake reached its
    parent's thread; and the program that then wrote the word — which is
    what a wake follows — had it in another frame, and woke nobody: a
    semaphore two threads shared stopped working the moment a third forked.
  - *One step.* Looking at an entry and changing it are done with the
    space's lock held (`back`, `own`): a system call is preempted wherever a
    tick finds it, and a thread of the same program doing the same to the
    same page in between has the frame given back twice — the second time
    from under whoever still shares it. `fork` itself walks and changes
    the parent's tables in one step for the same reason, and tells every
    processor that has the parent loaded before it does anything else
    (`tlb::sync`): a thread of the parent on another processor would
    otherwise go on writing, through what it remembered, pages the child
    now has too. `dtest fork` has a check for each of these, and each was
    seen to fail with its line taken out.
  - *Break before make.* A copy is put in where the old frame was only once
    the entry has gone and every processor with the address space loaded
    has forgotten it (`own`: cleared, `tlb::sync`, then the copy). A
    processor goes on using a translation it remembers until it is told to
    forget it, whatever the table says by then; with the copy put straight
    in, another thread's processor that remembered the old, read-only
    translation found the new one for a store and answered its next load
    from the old frame — the child's — and the thread read back an older
    value than it had just written. Flushing sooner does not close that,
    only a missing entry does. cargo's malloc met it every few builds on
    four processors; `cowstress` in `../quarkutils` — threads storing to
    the same pages and reading straight back while one of them forks —
    meets it in seconds. And a fault is served by what the entry is when it
    is served, not by what the processor said when it was taken: a write
    that found no entry in that window, and waited at the door while a fork
    shared the page again, is a write to a page shared since a fork (`own`
    takes every write fault). Sent by the fault's word to `back`, which has
    nothing to give a page that is there, it ended its program.
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
  so this is the whole of it. How much of it there is, is what the kernel
  has told the processor programs may use (XCR0): x87 and SSE everywhere,
  AVX and AVX-512 where the processor has them, and all of that is saved,
  with XSAVE. **What is turned on and what is saved are one decision**
  (`fpu::enable`): a component turned on and not saved hands one task the
  upper halves of another's registers, and nothing faults to say so — the
  checks in `dtest fpu` fail, and that is all. A new component (AMX is
  eight kilobytes a task) is a bit in XCR0, room in `fpu::AREA_SIZE`, and
  the same on every processor: XCR0 is each processor's own
  (`fpu::init_processor`).
- **Page-table entries that refer to a memory object carry its slot** in bits
  52–62, present or not, and every path that clears or replaces an entry —
  `clear_range`, `unmap_page`, `map_page`, `free_pt_leaves` — hands the
  reference back (`memobj::unmap_ref`). Miss one and the object is never
  released. And every path that copies an entry takes one (`copy_pt`, for a
  fork): a page of a file mapped privately and since touched is the
  program's own copy and *still names the file*, and a child's entry for it
  was not counted — and was given back all the same when the child went.
  Two children later the file's server was told nothing mapped it, let it
  go, and the parent's next touch of a page it had not yet read was a bus
  error. Protection keys would give bits 59–62 a meaning, so CR4.PKE stays
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
- **A walk of a program's tables is one step** (`paging::OneStep`): from
  the first entry it reads to the last it writes, with the address space's
  lock held (`paging::space_lock`, `sync::RANK_SPACE`: one of 64, the one
  its root hashes to, the same every time; a step inside another of the
  same space's takes nothing more). The kernel's own tables, changed only
  as the heap grows under the heap's lock, are a step with interrupts off
  alone. An
  unmap gives back a table it leaves empty, and the directory above it if
  that is empty too (`reclaim_empty_tables`), and a system call is
  preempted wherever a tick finds it — so a walk that had read its way down
  and was preempted went on in a table another thread of the program had
  since given back. A map filled in a directory that was nobody's: the page
  was in no table the program could reach, and the program faulted on the
  page it had just been given; and the entry was written into a frame that
  may by then have been somebody else's. `kstress mix` met it within
  seconds on eight processors, and `dtest tables` — two threads kept to one
  processor, each mapping and unmapping a page under one directory — meets
  it every time on one. A new walk takes a `OneStep`, or is only called
  from one that has (`back`, `own` and `unshare` take one; the fork's copy
  holds both spaces' locks, `paging::lock_two_spaces`, and reclaim's takes
  the space's); one that has to wait in the middle gives its step up and
  walks again afterwards, as `back_object` does for a pager — nothing is
  held across a wait.
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
- **A capability space grows, and a slot's number never moves** (`cap::CSpace`):
  room for 256 when its first slot is written, doubled when a slot past it
  is wanted or every slot is full, to 65,536; a slot past the room is
  empty, and `SYS_CAP_READ` says how much room there is — a walk of every
  slot goes that far, or it misses what is past where it stopped (the C
  layer's `setuid` gave up `SetUid` in the first sixty-four slots of 256).
  **And a capability is made only where it can be revoked**: a slot's count
  of revocations is made when one is first minted, granted or taken from it
  (`cap::generation_for`), and that is refused with no memory for it. A
  revoke cannot fail for want of memory, and does nothing to a slot with no
  count: nothing was derived from it. **A space is read and changed with
  its lock held** (`cap::locked`, one of 64 by the space's number,
  `sync::RANK_CSPACE`), a task's number read again once it is — whoever
  moves a task between spaces holds both — and **a count is read with no
  lock at all**: a call's check of an endpoint reads one on every call, so
  counts are words that only go on, in chunks that never move
  (`cap::Counts`), made under the table's lock (`RANK_CAP_TABLE`). A new
  look at a space goes through `locked`, `with_cspace` or `holds`.
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
  that one is kept by reading the stub. *TF and NT* are the program's too,
  and `SFMASK` clears both on `syscall` (`kernelentry` in `../quarkutils`
  has each): with TF set the call's trap came at `syscall_entry`'s first
  instruction, before `swapgs`, on the program's stack — a double fault
  under SMAP, and without it a handler running on a stack the program
  chose; with NT set the `iretq` a signal handler's return goes out by
  faulted in ring 0. Each halted the machine.
- **Every way back to ring 3 goes to an address that is the program's, and
  nothing that can run on the program's GS uses it.** A `sysret` or an
  `iretq` to an address that is not canonical faults in ring 0, after
  `swapgs` — on the program's GS, and for `sysret` on Intel on the
  program's stack. So what a task goes back to is checked where it is set:
  `exec_into`'s entry and stack (a program started at 2^47 halted the
  machine, on AMD as on Intel), `signal::ret`'s record, the handler a
  program names, `SYS_TASK_START`'s entry; and the end of a call checks the
  frame's RIP again before the `sysretq`, for whatever new way forgets.
  Where FSGSBASE is on (`cpu::fsgsbase`, from 4.5) a program sets its GS
  base itself, so a fault taken there would run the kernel on a GS base the
  program chose. An NMI or a machine check can arrive between `syscall` and
  `swapgs`, or `swapgs` and `sysretq`: neither touches GS (`nmi_stub`, which
  lets an NMI pass — the kernel sends none — and `machine_check_stub`, which
  writes to the serial port with no lock and stops the processor). A double
  fault can only happen after `swapgs`, once the above holds, and uses GS.
- **A task's FS and GS bases are its own** (`Task::fs_base`, `gs_base`).
  Where FSGSBASE is on a program changes them without a call, so a switch
  reads the outgoing task's back from the processor — the FS base with
  `rdfsbase`, the GS base from `IA32_KERNEL_GS_BASE`, where `swapgs` keeps
  it while the kernel runs — before it gives the incoming its own (`switch_to`).
  `fork` copies the parent's as the processor has them, not as last
  recorded; `exec` clears both; `SYS_SET_FS_BASE` still sets the first. A
  program is told it may (`AT_HWCAP2` bit 1) by whoever builds its
  auxiliary vector, for a kernel of 4.5 or later: the kernel builds none.
- **Validate user pointers with `validate_user_ptr{,_mut}`, not a range check.**
  The kernel runs on the caller's CR3; an in-range but unmapped address faults
  *inside* the kernel, sometimes with a lock held and interrupts off.
- **What a system call has checked stays in memory until it returns**
  (`scheduler::pin`, which `validate_user_range` does; `unpin` at the end of
  `syscall_dispatch`). A call checks a buffer, waits — for a pipe to have
  something in it — and then copies with a lock held, and a page that had
  been written out in the meantime (`reclaim.rs`) could not be waited for
  there: it would be a fault in the kernel with interrupts off, and the
  machine. So what is taken from a program is never a page a call some task
  of it is in has checked. Eight ranges a call are remembered, and a call
  that checks more holds all of its program's memory. A new place the
  kernel touches a program's memory with interrupts off touches only what
  the call validated — or pins it itself, as the futex wait does its word.
  A fault in the kernel where interrupts were *on* may wait like one from
  ring 3 (`may_wait` in `idt.rs`): it could have been preempted there.
  **A page a call checks that has no memory yet is given it under the one
  lock**, which a call made without it takes for that
  (`validate_user_range`, a futex's word): the memory may be a frame taken
  from a program, or a file's page asked of its pager, and both are the
  one lock's. A range there already, as the call wants it, needs nothing.
  **Nor is it unmapped by the program's other threads**: `SYS_MUNMAP`,
  `SYS_ADDRSPACE_GIVE` and an unmap of shared memory are refused where a
  call another task of the program is in has checked any of the range
  (`scheduler::pinned_by_another`), asked and done under the space's lock,
  which the check's own walk takes too. A thread
  waiting to read a pipe into a page another thread unmapped was copied
  into nothing when the write came — a fault in the kernel with interrupts
  off, and any program's way to stop the machine (`dtest tables`, `dchild
  unmapread`). And the pin is made before the check, not after it: an
  unmap between the two went through. A new way to take a program's
  mapping away asks too.
- **Memory is taken only from what can do without it, and given up only
  when it is somewhere else.** `reclaim.rs` is the whole of it and says
  what and why; the rules it leaves:
  - *Never from a driver or a server* (`scheduler::space_gives_memory`):
    they are what a page is written out with and read back by, and one
    waiting for its own page waits for ever. That is also why the pager
    for memory, the file server it writes through and the disk driver
    under that are a server, a server and a driver.
  - *The last frames are theirs* (`reclaim::frame`, `reserve`): what gives
    a program's page its frame asks there, not `pmm::alloc`, and is
    refused while the last hundred-and-twenty-eighth of memory is all
    that is free. `pmm::alloc` itself is for the kernel's own needs.
  - *A frame of an object's cache is counted once for each entry that maps
    it* (`pmm::mapped`, in `back_object` and `copy_pt`; `pmm::unmapped`, in
    `memobj::drop_entry` and where a mapping is taken back): nought is what
    lets the cache give it up. Map one somewhere new and not count it, and
    the cache gives the frame away under the mapping.
  - *The cache is as big as the machine, and an entry is found by what
    names it* (`memobj::init`): room for a quarter of memory's pages, in a
    table made at boot. It was 8192 pages whatever the machine, and rustc —
    which maps its own code, two hundred megabytes of it — took back a page
    it was using for every page it touched past them: building the kernel
    on Quark read six gigabytes from the disk in twenty minutes and did not
    finish. An entry is found by its key, by its frame's place (`page_of`)
    or by its object's pages (`lowest`, `release`); a walk of the whole
    table grows with the machine, and is for once a pass, never once a
    page.
  - *A page number of the object memory is written out to is counted once
    for each reservation that names it* (`memobj::swap_ref`, `swap_unref`):
    a fork copies a reservation, and each side has its own page when it
    touches it. Nought is a number that is free, and the lowest free is the
    one given: a file that backs the object is then as long as the most
    that was ever out at once.
  - *Nothing is given up that its pager has not written* — and said it
    wrote (`CTL_TAKE_OUT`, `CTL_WRITTEN`): a page taken to be written is
    neither clean nor dirty until then, so a full disk leaves it in memory
    rather than nowhere.
  - *A limit is not a shortage* (`Fault::Limit`, `Fault::NoMemory`): a task
    at its own limit is ended at once, as before, and only one the machine
    has no frame for waits.
  - *A fault that has waited is taken again, not served again* (`idt.rs`):
    once memory has been found the instruction runs again and faults again
    if it has to. While it waited another thread may have been given the
    page, which `back` calls nothing promised — of four threads that
    touched the same new pages as memory ran out, three were ended so — or
    the page may have gone. And where the fault was is read once, at the
    door, before anything there can wait — the lock, or a stop
    (`arrived`): CR2 is the processor's, and the faults of whatever ran
    while this one waited write it. Read after the stop, a child stopped
    straight after its fork was served at another program's page, or at
    nought on a processor that had never faulted, and ended for touching
    it; jobtest met it once in an acceptance on four processors, and
    `stopfault` in `../quarkutils` meets it in seconds.
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
  a CAP_TASK_MGMT holder can install one. It names its task the same way
  (`FdKind::Ipc`'s `endpoint`, and the old socket's): it held the TID once,
  and a descriptor to a server that had died wrote to whatever task was made
  next — every service's output, once the services had a log.
- **A server calls a client back only with a capability the client offered.**
  `sys_call_offer` puts one on a call and `sys_cap_take` accepts it; nothing
  else can fill a server's CSpace, and a claim or registration made without
  one is refused.
- **A descriptor is its program's, and every change to the table is one
  step.** Every task of a program uses one table (`fdtable.rs`), so a sibling
  can be preempted half way through anything. "Find a free number and fill
  it" is `fdtable::install`, with the descriptor tables' lock held across
  both halves (`sync::RANK_FDTABLE`, before what a descriptor names);
  reading a descriptor and taking a reference for a copy of it is
  `get_retained`. A call that looks a descriptor up and then acts on it in
  two steps has a window in which a sibling closes it and somebody else is
  given the slot. The rest of a program's record — its signals, alarm,
  timers, use, name, limits, umask and trap — has a lock of its own
  (`fdtable::PROGRAMS`, `RANK_PROGRAM`), after the things a wait holds, so
  that a wait asks about signals under its own lock; which tasks use a
  table changes with both held, and a fork copies the two halves a step
  each.
- **A task waits on one thing at a time, and where is a link in its own
  record** (`waitlist.rs`): a counter's, a timer's, a pipe's, a terminal's
  and a listener's waiters, and those parked on a poll set, are a list
  through those links, which has no length to fill — the arrays of four and
  eight they were refused the fifth or the ninth, whose read came back at
  once, and a set had one, which a second thread waiting on it took from the
  first. A task comes off by its own link: when it is woken, when a signal
  ends its wait, when it dies, and at the reap before its record is freed;
  and a set that goes takes everybody off its list first, or a set made
  with its number would be handed their links. A new kind's waiters go the
  same way: an `On`, its lists' ends in the object, and a `forget` that
  `waitlist::forget` finds them by. A list, and the links of the tasks on
  it, are kept under the lock of the kind of thing it belongs to (each
  kind a program makes has one, `sync::RANK_POLLSET` to `RANK_SHMEM`):
  whoever adds, takes off or wakes holds it, and the kind's `forget` takes
  it and looks again once it holds it — so nothing calls
  `waitlist::forget` with that lock held.
- **A futex waiter is its task's record too** (`futex.rs`): the word's key,
  its deadline, why it woke, and a link into one of 256 lists by the key's
  hash — so no wait is refused for room (there were sixty-four, and the
  sixty-fifth was answered at once, which a lock's user takes for a wake and
  spins on), and a waiter can be moved to another word's list
  (`SYS_FUTEX_REQUEUE`, keyed as a wait on that word would be). A wake, a
  deadline and a signal each take the waiter off its list and say why in
  its record, for it to read when it runs; and a task that dies waiting is
  taken off where it dies (`close_descriptors`), not at its reap — a wake
  counted for it would be one a waiter that runs again was not given.
  **A wait, a wake and a requeue are made without the one lock**
  (`syscall::unlocked`), under the lists' locks and the waiters' records:
  asking whether to wait and parking are one step under the word's list,
  and the word is read through its frame under its address space's lock
  (`futex::read_word`) — a sibling's write to a page shared since a fork
  takes the entry away while it puts the copy in (`own`), and a read
  through the program's own mapping then was a fault in the kernel with a
  lock held. A word that lends its holder a place (below) is still the
  one lock's: its lending looks at every task. Each
  list has a lock of its own (`sync::RANK_FUTEX`), which a waiter's links,
  and its record while it waits, are kept under: the word is read, the
  waiter linked and its task blocked with its list's lock held, and a wake
  looks with the same lock; a requeue takes both lists, in the order of
  where they are; and whatever finds a waiter by its task — its own wake, a
  signal, its death — takes the list its key names and looks again once it
  holds it, since a requeue may have moved it meanwhile.
- **A word whose holder is lent its waiters' places is the kernel's to hand
  on** (`futex::lock_pi`, `SYS_FUTEX_PI`; Linux's FUTEX_LOCK_PI, a word that
  holds its holder's task id). A waiter for one is a futex waiter that knows
  whose the word is (`pi_owner`), and the scheduler asks it as it asks IPC
  (`pi_waits_on`, `waits_on`): the holder runs at the best of its waiters'
  places, and so does whatever it waits for in turn, 32 deep. Whatever ends
  such a wait — a hand-on, a deadline, a signal, a death — takes the lending
  back *where the waiter comes off its list* (`refresh_priority` of the
  holder): left to the waiter for when it next ran, a holder lent a
  real-time place would keep the processor from the very waiter that would
  take it back. An unlock hands the word to the best placed waiter, and the
  robust list's walk to the best waiter with FUTEX_OWNER_DIED
  (`pi_owner_died`); a holder that dies or execs holding one that was not on
  its list is lent nothing more (`owner_gone`) — its task id will be
  somebody else's. The word is written through its frame after
  `paging::own` (`word_at`), so nothing faults with the futex lock held.
  And wakes and requeues never touch a PI waiter (`each_on`'s `pi`): one
  woken without the word would look again, and one moved would lend to a
  holder it no longer waits for.
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
  collects), and as an object nothing maps has (`memobj::take_idle`: a
  flag on the object, which its pager's next receive collects). That was a
  list thirty-two long too: a program that gave up forty mapped files at
  once left eight untold, and their file server kept them, removed and
  open, until the machine went off — eight files on the orphan list of
  every disk the C suite had run on (`manymaps` in `../quarkutils` counts
  its files back now).
- **A call from the kernel to a pager carries `PAGER_BIT` in its sender**, and
  nothing else can: the bit is set by `call_as` and by no syscall. The reply
  strips the bit and reaches the faulting task.
- **Only the kernel sends as sender 0.** A death notice (`TAG_TASK_DIED`, or
  the one for a program) is a message from sender 0, and no syscall can forge
  that; any program can send the *tag*. Servers depend on the difference.
- **`init` is started with the framebuffer and its boot modules, and nothing
  wider.** Every other `PhysRange` over memory is derived from those, so
  what the kernel hands the first task bounds what any task can map.
- **A screen in memory is nobody's** (`display.rs`). The bootloader's
  framebuffer, where it is RAM, is kept from the allocator at boot
  (`pmm::init`); and a display driver's screen (`SYS_DISPLAY_MEMORY`) is one
  run of frames no task owns and nothing frees — its device's for as long as
  the machine is up, the same run given to a driver started for it again,
  handed out as a `PhysRange` the way the boot framebuffer is, and reached
  by the device of whichever program has claimed it (`iommu::reach`). Owned
  by the driver, its death would hand the frames to the allocator while the
  console was still drawing into them.
- **A `PhysRange` over a device's registers is minted from the device.**
  A PCI device's driver holds that device (see the next rule) and mints
  inside its BARs, as the kernel sized them at boot. `DeviceMemory` is the
  older and wider way, still handed to `init` and asked for by nothing:
  *device memory* (`devmem.rs`) is every address below four gigabytes that
  the firmware's memory map does not list at all, less the first megabyte,
  the interrupt controllers' own pages — a program that could write to a
  local APIC could stop the clock — the IOMMUs', which a program could turn
  off, and the MCFG's window, which is every device's configuration. Either
  way the capability is minted narrow so that no task holds a `PhysRange`
  wider than what it maps (`cap::can_mint`). Device memory is worked out
  from the *whole* map or not at all: a hole in what the kernel kept of the
  map is not a hole in the map, and RAM handed out as device memory is the
  kernel handed out. The map is kept whole by joining neighbours of one
  kind as it is read (`multiboot2::parse_memory_map`); kept one for one, a
  UEFI machine's hundred entries did not fit in sixty-four and the last of
  its memory was never seen. `init`'s capabilities of these kinds go in its
  *last* free slots of the first 256 (`cap::insert_last`): it names its low
  ones itself and counts on the rest of them being empty — of the first 256
  and not of whatever room its space has, so that its numbers stayed where
  they were when a space could grow.
- **A device is its capability, and its configuration is the kernel's.**
  Every PCI function is found once, at boot, before the other processors
  start and before there is a task (`pci::init`): its ids, its class, its
  capabilities and its BARs, sized then, while nothing can be using them.
  Configuration space is reached through the kernel and nothing else — the
  window every device shares, ports 0xCF8 and 0xCFC to 0xCFF, is refused to
  every program whoever holds the ports (`pci::config_port`; a byte at
  0xCF9 is the chipset's reset register and is not the window), and the
  MCFG's window is left out of device memory. A program reaches a device
  with `PciDevice` for it (type 14): `init` holds every device, the device
  manager in `../quarkutils` is handed that and hands each driver its own.
  Everything about a device goes with that one capability: its
  configuration (`SYS_PCI_READ`, `SYS_PCI_WRITE`), what the kernel found
  (`SYS_PCI_DEVICE`), a `PhysRange` or `IoPort` minted inside one of its
  BARs (`pci::memory_covers`, `ports_cover`), its claim and its interrupt by
  message. Three things follow, each kept by a refusal in `pci::kept` or
  beside it:
  - *A device stays where the firmware put it.* No program writes a BAR, a
    ROM base or a bridge's windows: a BAR moved is a capability that lets
    its holder map whatever is now at the old address. A `PhysRange` from a
    device is its BAR's pages, and only where no other device's BAR shares
    them.
  - *Where a message goes is the kernel's to say.* The MSI capability is
    not written by a program; `SYS_MSI_ALLOC` for a device aims it
    (`pci::aim`). MSI-X's table is in the device's own registers and is its
    driver's to write — a gap, below.
  - *Nothing masters the bus that its program has not claimed.* Every
    function but a bridge has bus mastering turned off at boot; turning it
    on is refused until the caller's program has claimed the device; and it
    is turned off again when that program goes (`iommu::program_gone` calls
    `pci::stop`), before the program's frames are anybody else's — on a
    machine with no IOMMU that is all that stops a device writing to them.
  A new way to reach a device's configuration goes through `pci::read` and
  `write`, and asks `kept`.
- **Where there is an IOMMU, a device reaches what its driver was given
  and nothing else** (`iommu.rs`, Intel's VT-d). It is turned on at boot
  with nothing reachable; `SYS_DEVICE_CLAIM` makes a device a program's,
  and from then on it reaches the frames that program asked for for
  devices (`SYS_PHYS_ALLOC`, the frames with an owner), at their own
  addresses, so a driver's numbers do not change. Every change of a
  frame's owner goes through `pmm::set_owner`, `clear_owner` and
  `release_task_frames`, which tell the IOMMU: a frame leaving its owner
  is out of every table, the units told and waited for, before it can be
  anybody else's. A new way to give a program memory for a device, or to
  take it back, goes through them too. A claim is a driver's — a holder
  of the device — and goes when its program does, `exec` included
  (`program_gone`).

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
and `cat` said so. The wait is for the room the next byte needs, and a
newline needs two (`pty::takes`, which the write, the wait and `poll` all
ask): see *More than one processor* for what it was when the wait asked only
whether there was any. What a terminal emulator types does not wait, because
its echo comes back at it and a wait there is a wait on itself.
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

And **signals are one** (`SYS_SIGNAL_FD`, `signalfd`): a descriptor read
for a set of signals, whose reads take what is waiting for the *reader* —
its own task's, then its program's — so a forked child reads its own. It is
readable while one of its set is waiting, and a signal that comes to wait
where one could read it wakes a set watching one (`pollset::note_signals`).
What a read gives is Linux's `signalfd_siginfo`, laid out by the kernel: a
C library cannot tell this read from any other.

And **a local socket is one** before it is connected (`local.rs`): nothing
yet, named, listening. Its name is a file server's, as a named pipe's is —
the server names a socket a task calling it holds, and connects one to
what listens at a name, each by a key of its own (`SYS_SOCKET_BIND`,
`SYS_SOCKET_CONNECT`) — and the kernel knows a listener by the server's
endpoint and the key. Connected, it is a stream, made at once whether
anybody is accepting or not: the connector's descriptor becomes end 0 in
the same slot (`fdtable::swap_if`, close-on-exec mark and all), and end 1
waits in the listener's queue for `accept`. Each end of a stream knows who
is at the other (`stream::Creds`).

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
- A file is always ready. What is not a file — an inotify instance, whose
  events come when they come — is made with a flag that says its server
  will say when it is (`SYS_FD_READY`), and a poll answers that. Its
  server is told whether a read may wait, and answers one that may not
  with "nothing yet" rather than holding it: a descriptor marked
  non-blocking that was held would be a program stopped, not one told to
  try again.

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

`FD_CWD` — Linux's `AT_FDCWD`, −100, as an unsigned word, beside the
numbered descriptors — is the program's working directory, a served
descriptor like any other. It is in the table so that it follows a program
through `fork` and `exec` with no server being told. It was 64, one past a
table of sixty-four; a program's descriptors go to its limit now (1,024 to
start, raised by the program as far as 65,536, `SYS_FD_LIMIT`), in an array
that grows (`grow.rs`), and a number that moved with the limit would have
moved with every raise. Nothing copies a table onto a stack: one of 65,536
would be the stack many times over.

Adding a kind means touching every place that enumerates them, and missing one
is quiet: `FdKind` in `task.rs`, read and write in both their blocking and
non-blocking forms in `syscall.rs`, `pipe::release_fd` and `pipe::retain_fd`
(a kind in one and not the other leaks or double-frees), `fdtable::hold`'s
list of what a task can be parked on, `waitlist::On` and `forget` for a kind
whose waiters are links, and `pollset::watchable` and
`readiness_at` — `poll` answered `POLLNVAL` for a terminal until the last of
those knew about it. And whatever changes what one is ready for says so with
a `pollset::note_*`: a set somebody is parked on can be woken by anything,
but an edge-triggered watch (epoll's `EPOLLET`) is stirred by a note and by
nothing else, so a kind that changes quietly is one an edge never reports.
It says so before it waits, too: a write that fills a pipe and waits for
room has made it readable already, and the reader that would make the room
may be waiting in a poll. Told only once the write was over, cargo — which
polls for what rustc prints — and rustc, printing more than a pipe's 4 KiB,
waited for each other for good.

## What a process is

A program starts one of two ways, and the kernel's part in each is small.

A **spawner** builds one, in user space (`quark_rt::spawn` in
`../quarkutils`): it makes an address space, moves pages into it, wires the
descriptors, hands over the capabilities and starts a task there. The kernel's
rule is `may_prepare` — a task the caller created and has not started is its
own to fill, because nothing else can name it, it holds nothing and it cannot
run — so none of that needs authority over anybody. That includes its band
(`SYS_TASK_PRIORITY`, no better than the caller's own — the band it is in,
not the one it runs at: a program the framebuffer device is calling back is
lent the servers' band while the device waits, and a child put in it then
kept it): the device manager
holds no `TaskMgmt`, and when this rule left the band out every driver it
started ran as an ordinary program, whose memory is taken when memory is
short — a disk's driver written out to the disk it drives, and the machine
waiting on it for good. `TaskMgmt` buys the unbounded form; without it a
program may have 4,096 tasks of the machine's 32,768 — its own, and the
children it has made and not collected, a fork counted as any other
(`syscall::A_PROGRAMS_TASKS`). It was sixteen children for each task that
made them, with a fork not counted at all.

Or a program **forks** and **execs**, which is what a C program does and what
every Unix program assumes:

- **`fork` shares, and a write copies.** The child is a task in a copy of
  the caller's address space that returns 0 from the same system call — which
  works because the syscall stub's eleven pushes always land at
  `kernel_stack_top - 88`, so a task inside a call has its whole register
  frame at a known place. The copy is of the page tables: a page the parent
  owns is one both own until one of them writes it, and that one is given a
  copy then (see the invariant). A page that could not be written is shared
  for good; a page the parent does not own — shared memory, a device, a
  file's page — is shared as it always was. It copied every page for a long
  time, for a child whose first act is nearly always to become another
  program and throw them away: four megabytes cost a thousand frames and
  now cost fifteen. Neither is charged for a copy when it comes — the page
  was counted as its own when it came to have it — so a fork can promise
  memory the machine has not got, and the write that finds that out ends its
  program, as touching a reserved page does.
- **`exec` keeps the task and changes the program.** The ELF is loaded in user
  space, into an address space the caller made, and `SYS_EXEC_SPACE` swaps the
  task into it: same id, same descriptors, same capabilities, same parent, new
  address space — and therefore a new program as far as every server is
  concerned. The thread pointer is cleared with it, or the new program's first
  thread-local reads through an address the old one had. A program with other
  tasks has them ended first, as POSIX has it (`end_siblings`), and quietly:
  none is a child anybody collects or a death anybody is told of, since the
  program goes on. If one of them was the task the program began as, the
  caller takes its place as its parent's child, and a parent already waiting
  looks again.
- **`fork` copies the descriptor table and the capabilities; `exec` keeps
  them.** The child gets a second descriptor for everything the parent has
  open, the working directory included, and a copy of the capability space —
  all of each, or the fork fails: a table and a space grow as they are
  written, and their copies can want memory there is not;
  `exec` closes what was marked for it (`SYS_FD_FLAGS`) and nothing else. A
  thread does neither: it uses its program's table and its program's space
  (`cap::share`), so what one thread is given the others have. A thread used
  to start with a copy of its creator's capabilities, as they stood: a C
  library that looked a service up in one thread was refused it in another.
- **What a program was started as is its program's** (`SYS_PROGRAM_NAME`):
  a hundred and twenty-eight bytes of its arguments, kept in its descriptor
  table's record beside its signals, so its threads have it, `fork` copies
  it and `exec` keeps it — until the program that execs says what it is
  becoming, which the C layer's `execve` does. It is said by the program,
  by a spawner of the child it has made and not started (`may_prepare`), or
  by a holder of `TaskMgmt`, and read by anybody: it is what `ps` shows and
  what `/proc` calls a process. Nothing in the kernel acts on it.
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
  is worth looking for anywhere in the kernel that remembers a number. Pipes
  have no budget now — what a program makes is bounded by its descriptors and
  by `reclaim::may_make` — and the tasks a program may have are counted by
  its program (`scheduler::program_tasks`).
- **What a program makes is made when it makes it**: a counter, a timer, a
  signal descriptor, a pipe — whose buffer is a frame of its own — a shared
  region, a terminal, a local socket, a stream, a poll set, and a served
  descriptor are each a record in a table that grows (`table.rs`), made
  through `reclaim::may_make` so that a program making them without end is
  refused while the kernel can still work, and given back with the last
  thing that names it. They were tables for the whole machine, of eight,
  sixteen, thirty-two, sixty-four, 256 and 1,024. What is in them grows
  too: a set's watches, a listener's queue (to 4,096, `somaxconn`), a
  stream's descriptors in flight (as many as the sender may have) — and
  nothing that grows is gathered on a kernel stack: a `poll`'s entries are
  read and answered where the program has them. A memory object is a
  record made when it is, but its slot is written into page-table entries
  in eleven bits, so there are at most 2,047 (`memobj::MAX_OBJECTS`), 1,024
  a pager. Each kind is kept under a lock of its own (`sync.rs`'s ranks,
  the poll sets' outermost and the shared regions' innermost): its table,
  every one of it and its waiters — where every look at one and change to
  it is made, in the one step it was made in with interrupts off. One kind
  calls into another only in the ranks' order: a poll set asks what it
  watches, a local socket makes a stream, a stream has pipes.
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
  ended thread kept its slot in the task table until its program ended, a
  `waitpid(-1)` could be answered with one, and a program whose only
  "children" were its threads was told to go on waiting. The creator gives
  the word, before the thread is started: one that gave its own was a
  child until it first ran. A thread with no word is still waited for,
  which is how this system's own runtime joins one.
- **A task that dies holding a robust mutex lets it go** (`threads.rs`). A C
  library says where its list of them is (`SYS_ROBUST_LIST`, Linux's
  `set_robust_list`); where a task dies (`close_descriptors`) and where it
  becomes another program (`exec_into`) the kernel walks the list in the
  task's own memory, marks each mutex still marked with its id as one whose
  owner died, and wakes a waiter in that space (`futex::wake_in`) — a page
  shared since a fork made the dying task's own first, as anything the
  kernel writes through a frame is. musl walks the list itself when a
  thread ends the ordinary way; this is for a program that ends without
  doing so, holding a mutex another program waits on. A new way for a task
  to end, or to stop being its program, calls `let_go`.
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

- **A handler is run one of two ways, as the program says.** *Told*: the
  signal is recorded as waiting, a word in the program's own memory is set
  through its page tables, and one wait is ended early; the runtime takes
  what is waiting (`SYS_SIG_TAKE`) and calls the handler as a function, at
  a call. *Run*: the kernel turns a task aside on its way out of the kernel
  and enters the program at the one place it named, with a record of where
  the task was on its stack (`signal::push`); `SYS_SIG_RETURN` puts it
  back. Programs written for this system are told; C programs are run. A
  program that has said nothing is ended, here (`scheduler::end_program`,
  with the negated signal number, as a fault does).
- **A handler is run on every way out to ring 3**: the end of a call
  (`signal::leaving_call`), of an interrupt and of a fault
  (`leaving_interrupt`), and `SYS_SIG_RETURN` itself. The last test is made
  with interrupts off and they stay off to `sysret` or `iretq`, so a signal
  raised after it is seen at the task's next tick or call. A new way out to
  ring 3 asks too, or a program computing on that path is not interrupted.
- **The record is written and read back in one step with the page it is
  on.** `back_range` brings the page in with interrupts on; whether it is
  still there is asked, and the record copied, with the address space's
  lock held (`paging::space_lock`) — between the two, a program short of
  memory could have it taken, or a sibling unmap it.
- **The kernel keeps no floating-point state for a handler.** The place a
  program is entered saves and restores it, in ring 3, where a state that
  is not one is the program's fault and not the kernel's: an `XRSTOR` in
  ring 0 that faults halts the machine.
- **A call made from where its program said none is made is not made**
  (`signal::trap_call`, `SYS_SYSCALL_TRAP`; Linux's syscall user dispatch).
  It is SIGSYS at once, as a fault is, ahead of anything else waiting, with
  every register the call had in the handler's record: a call of this
  kernel's leaves RDX and R8 to R10 as nought and takes no sixth argument,
  and one of Linux's leaves them alone and takes R9 — so the stub keeps R9
  at `%gs:16` before it uses the register, and the record is built from the
  user frame and the arguments rather than by `leaving_call`. No handler,
  or one held back, is the end of the program. It is how a program built
  for Linux runs on Quark's C library: the library traps every call not
  made from its own code, and answers it as its own. A new way in by
  `syscall` asks it too.
- **A mask is a task's, and a signal waits for a task that lets it
  through.** `signal::MASK`. A signal every task of the program holds back
  waits whatever it would do (`fdtable::sig_hold`), and does it when one
  lets it through; so does one raised for a task alone (`TPENDING`) while
  that task holds it back. 9 and 19 are never held back.
- **One signal the program is told of ends one wait: the first to look.**
  `fdtable::sig_interrupted` is true once. Held as a level — "a signal is
  waiting, do not wait" — it turns every loop that sleeps and looks again
  into a spin for as long as the program has not taken the signal, and
  three of those loops are in the kernel: a poll sleeps by receiving from
  its own id. A signal the kernel is to run *is* a level
  (`signal::ends_wait`) — but the call it ends runs the handler on its way
  out, and the level is gone; each of the three loops returns to the
  program when its sleep says it was interrupted, and has to.
- **Asking whether to wait and parking are one step**, with interrupts off
  and the lock of what is waited on held (`pty::wait_readable`,
  `ipc::sys_recv_timeout`, and `signal::ends_wait` beside every wait's own
  question). A signal raised between the two finds nobody parked, and the
  wait outlasts it.
- **A signal ends only a wait that looks again when it is woken.** One the
  program is told of ends a read of a terminal, a poll, a sleep and an open
  of a named pipe. One the kernel runs ends those and a read or write of a
  pipe, a stream or a terminal, a read of a counter or a timer, a futex and
  a wait for a child — each takes the task off its list (`signal::end_wait`)
  and looks again, and each asks `ends_wait` before it parks. A call to a
  server is not one — woken with no reply, it fails — so a task is reached
  by what it is waiting on, never by its state. A new kind of wait is a new
  line in `end_wait` and a new `ends_wait` before it parks.
- **What came with a signal goes with it** (`signal::Info`): Linux's
  `si_code`, who raised it, and what it carried, at the end of a handler's
  record and wherever it is taken. A new place that raises a signal says
  who and as what (`raise_with`, `Info::from_task`; `raise` is the
  kernel's own), and a real-time one can be refused — as many of its
  number waiting as there is room for (`NotRaised::Full`) — which a call
  that raised it on a program's behalf answers (`0xFFFF_FFFE`). What waits
  is kept beside the descriptor tables (`fdtable::WAITING`) and not in
  them: in them it was two hundred kilobytes of noughts in the kernel's
  image.
- **A call a signal cut short says what came of it, to a program that asked**
  (`fdtable::sig_unix`): `INTERRUPTED` for a handler that did not ask for
  SA_RESTART, `RESTART` for one that did, `AGAIN` when nothing ran here.
  A C library cannot tell the three apart by itself, and getting them wrong
  is a `read` that fails with EINTR after a `fg`. Every other program is
  answered `INTERRUPTED`, as it always was.
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
  ever. The alarm is raised by the clock (`clock::expire`, from the first
  processor's timer when it is due, and from the tick in case nobody said),
  one program at a time, each alarm put away before its signal is raised:
  for a program that has said nothing the signal is the end of it, and if
  that is the program the clock interrupted, `signal::alarms` does not
  return. So it is raised with the clock's timer set a tick ahead, which is
  when the rest is looked at if it does not, and the timer is set for what
  is due next only once it has (`fdtable::alarm_after`).

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
  run queue and never switched to. `enqueue` — with `requeue_here` and
  `enqueue_front` — and `donate_to` are where a task becomes runnable, and
  each asks. What would have woken it
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
  clock, which the death asks to look at once (`clock::due`; `job::hang_up`
  from `clock::expire`), and which is already where an alarm may end
  whatever was running. A stopped program nobody can start is a task slot
  gone for good, and its program's allowance with it.
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
- **A signal held back is a signal that may not stop the holder.** A shell
  takes its terminal back from behind with SIGTTOU blocked, which on Unix is
  what stops it being stopped for asking; a read from behind with SIGTTIN
  blocked fails instead. The kernel keeps masks now and asks them
  (`signal::mask_of`); `PTY_FRONT_QUIETLY` is still taken from a runtime
  that keeps a mask of its own.
- **A terminal is changed only from in front.** A process behind that
  changes its settings or its size, or puts a group in front, is stopped by
  SIGTTOU (`change_from_behind` in `syscall.rs`), and one that writes to it
  is too where its settings have `TOSTOP` — as on Unix, the same rule as a
  read, with the same ways out: ignored, held back, or an orphaned group.
  And whoever is in front is told when its size changes (SIGWINCH).

## Scheduling

Four bands, best first: drivers, servers, ordinary programs, idle. A task runs
only when nothing better is waiting, and within its band a task in a
real-time class goes first, by its priority, and then the one that has run
least. A program asks for a band in its `manifest!` block alongside
its capabilities, and a spawner applies it under the same narrowing rule — it
can never grant a better band than it is in, so only `init` can put a driver
in the driver band.

What follows from that, and breaking any of it is quiet:

- **Waiting means blocking.** A task in a better band that spins on
  `sys_yield` is immediately runnable again, so nothing below it ever runs.
  This is fatal rather than merely wasteful now: the runtime's
  `nameserver::lookup_retry` yielded a hundred times between tries and starved
  the VFS out of ever registering. A program sleeps with `SYS_RECV_TIMEOUT` on
  its own TID, which nobody can send to.
- **A synchronous call hands over the CPU.** The caller has blocked and has
  nothing to contribute until the reply, so the callee is switched to directly
  and runs on what is left of the caller's slice rather than a fresh one. A
  server does not earn a quantum every time it is called — and the caller
  does not when it is answered: it goes first (`unblock_task_next`), on what
  was left of the turn it called in (`PerTask::handed`). Given a whole turn
  each time, as it was, a pair of tasks calling each other never came to
  the end of one, and nothing else of their band ran on their processor
  while they called: `kstress` found it, two pairs to a processor, as a
  call unanswered for a second; `dtest turns` holds it.
- **A hand-over keeps interrupts off from waking the callee to switching to
  it.** `make_ready` leaves the callee runnable but in no queue, since it is
  about to run, so `donate_to` takes the flags `call_inner` saved instead of
  saving its own. With a gap between the two, a tick preempted the caller,
  already blocked, and nothing ever ran either task again: fontconfig hung
  about once a minute scanning fonts. `dtest calls` makes three million calls
  in three seconds and caught it on its first run. **And a task's IPC state
  is changed with its record's lock held** (`scheduler::lock_record`, one of
  256 by its number, `sync::RANK_TASK`): both records for a call or a send
  (`lock_records`), and for a receive taking a sender's message, which finds
  the sender without its lock and looks again with it; a sender becomes
  blocked on a receiver with the receiver's held, so a receiver's look and
  its blocking are one step. The record ranks after whatever parks or wakes
  a task — an object, a futex's list, an address space whose unmapping
  leaves an object to its pager — and the watches (`ipc::NOTICES`) and a
  served descriptor's notice rank before it, so a receive collects notices
  before it takes its own record — and blocks only if nothing was said
  since it looked (`noticed`, set under its record by whoever says one).
  Lending a place takes each record it changes (`refresh_priority`), so it
  is done with none held, and under the one lock, since it looks at every
  waiter: a call lends only if it changes the place (`lends`), a reply
  takes back only from a task that runs lent (`runs_lent`). **Who is waiting
  to send to a task is a queue in its record** (`ipc::PerTask::queue_*`),
  in the order they came: a task is on it exactly while it is blocked
  sending or calling there, and whatever ends that wait takes it off with
  both records held — the receive, a deadline, a signal, a death, a reap,
  and a wake that was not the receive's (a driver's interrupt wakes it
  whatever it waits on). It was a scan of every task, which a receive made
  without the one lock could not make: a record may be taken apart under
  it. **And a call reaches the task its capability was for**: the
  endpoint number checked is checked again under the callee's record
  (`ipc::reaches`), since a task made since in the slot has another.
- **A task runs at the place of whoever is waiting on it** — the band, and
  the real-time priority in it (`place_of`) — for as long as that is true.
  Waiting on it is calling it, or waiting to lock a priority-inheriting word
  it holds (`waits_on`). Without it a server called by something urgent is
  preempted by anything in between, and a holder of what a real-time thread
  waits for by anything between the two. It is also what lets the direct switch stay safe: the callee
  already carries the caller's place when the scheduler decides whether
  handing straight over would run something ahead of its betters.
- **Within a band, whoever has run least goes next**, each nanosecond counted
  for more the nicer it is (`VRUN`, `usage::weighted` — Linux's
  weights, so that nice 10 against nought is about one to nine there and
  here). In the order they were queued, a nicer program's shorter turn came
  round sooner, and on four processors nice 10 had about half of nice 0's
  share. A task that was waiting joins a little behind where its band has
  got to on the processor it waits on (a queue's `floor`, `runq::join`),
  not where it left off: started late or woken after a minute, it is not
  owed the minute, and with no floor it took a processor until it had
  caught up. Where the band has got to is the least that what the
  processor runs and has waiting have run, never going back
  (`runq::settle`; Linux's `min_vruntime`). It was the most any task had
  run when chosen, which only rises: a nice task's strides, a call handed
  over and a server lent another band's place drove it up, and since a
  task that moves keeps its distance from it, run times grew without end
  — every task's the largest a run time can be, in a full dtest, and two
  tasks of a processor taking turns whatever their weights. So a task's
  run time is in one processor's measure and one band's
  (`PerTask::measured_on`, `measured_in`), and every way a task comes to
  run, to wait or to another band converts it (`runq::moved`): a move, a
  pull, a hand-over, a lent place. A task whose turn ends keeps its run
  time (`requeue_here`): passed through the floor as if back from sleep,
  one at nought lost its lead each time one at nice 10 beside it ran. Two things are not by run time: a
  caller woken by its reply goes first (`unblock_task_next`), and a yield
  lets everything else ready in the band go first — once (`YIELDED`): sent
  behind everybody for good, a task that yields while it waits for another
  can wait for ever, and not passed over at all, it is chosen again at
  once, having run least.
- **A real-time task goes before every ordinary one of its band**
  (`SYS_SCHED`; Linux's `SCHED_FIFO` and `SCHED_RR`, priorities 1 to 99):
  the best priority first, a FIFO one until it blocks, yields or a better one
  is ready, a round-robin one for ten ticks among its equals. Entering a
  class takes `RealTime`; a task's class is given to what it makes. Its run
  time is not counted where its band's ordinary tasks' is (`count_turn`, and
  a queue's floor moves only for an ordinary task): counted there, three seconds of
  FIFO would put the floor three seconds on, and every ordinary task that
  woke would join that far behind those that had not slept.
- **A processor's real-time tasks have 950 ms of each second**, and past
  that its ordinary tasks of their band go first (`RT_USE`, `throttled`) —
  or, with none ready, the real-time one runs on. The time is counted at
  every switch *and on the tick*: a FIFO task that computes switches only
  when something of a better band preempts it, which a busy machine's
  servers do often enough to count it by and a quiet machine's need never
  do. (`rtsched` cannot tell: taken out, the tick's count still left the
  ordinary thread 4.5% on an ExplOSion machine.) A second begins when the
  last has run out, or when the real-time
  tasks come back after fifty milliseconds away (`charge_rt`): begun only by
  time, it was wherever earlier real-time work had left it, and three
  seconds of FIFO met two throttles or three by where that was. This is
  what leaves a machine whose real-time thread has gone into a loop a
  twentieth of a second in each to stop it with.
- **What outranks the running task runs when it is made ready, where the
  kernel can say so**: at the end of the system call that made it ready
  (`note_ready`, `preempt_if_asked` in `syscall_dispatch`), when the
  clock wakes it between ticks (`woken`), and — put to wait on another
  processor running something worse — by interrupting that one, which
  switches in the handler (`enqueue`, `kicked`): a FIFO thread woken while
  every processor computed waited half a tick there, eight milliseconds
  at worst, and runs within ten microseconds (`wakelatency` in
  `../quarkutils`). Not for a reply: the caller runs where its answerer
  waits. Outranking is one question (`outranked`: a better band, or a
  higher real-time priority in the same one while the processor's share
  lasts), and a hand-over asks it too: a direct switch to an ordinary
  callee must not skip a real-time task any more than a worse band may
  skip a better.

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
- **The ready queue is touched only with interrupts off**, and with its
  processor's queues' lock held (`runq.rs`). Putting a task on it is three
  writes; a tick between them queued the task it preempted in the same
  place, and the one being queued was ready and in no queue. `start_task`
  did that from a system call. Every caller of `unblock_task` holds
  interrupts off, or is an interrupt; `unblock_task` takes the task's
  record and then the queue.

## What a program uses

`usage.rs`, and `SYS_USAGE`, `SYS_NICE` and `SYS_CPU_LIMIT` in `docs/abi.md`.

- **Time is counted whenever the scheduler decides anything** (`usage::charge`,
  from `count_turn`), by the clock: exact, whatever the tick. **And the
  kernel's part at the doors**: `usage::entered` where a task comes in from
  ring 3 — `syscall_dispatch`, the interrupt and exception handlers — and
  `usage::leaving` on every way back, the three `enter_usermode`s included.
  A new way out to ring 3 says so, or the time a program then runs is
  counted as the kernel's. Sampled at the tick, as it first was, the part
  was wrong: a call runs mostly with interrupts off, a tick in it is taken
  after `sysretq`, and a loop of calls was the program's four-fifths of the
  time on one processor model and a seventh on another.
- **A program's use is its tasks', and outlives them.** An ended task's is
  folded into its program's record (`task_ended`, from `close_descriptors`).
  A program that ends leaves its own and what it collected at every task of
  its process not yet taken apart (`ENDED`): the one its parent collects
  may not be the last of them to end. Collecting hands it on (`collected`)
  — only collecting: a child taken apart unwaited-for is nobody's.
- **How long a program may run is its program's**, beside its
  descriptors: its threads have it, whatever it starts is given it
  (`fdtable::runs_like`), `exec` keeps it.
- **How nice a task is, is the task's**, with its class: given by the task
  that makes a thread or a child to it (`create_empty_task`), and kept by
  `exec`. `SYS_NICE` says it of every task of a program, `SYS_SCHED` of one,
  as Linux's `setpriority` does of a thread. It was the program's, and no
  thread could be nicer than its siblings.
- **A limit is looked at on the tick, last** (`usage::limits`), as an alarm
  is: SIGXCPU, or the end of the program, may not return.
- **A processor's time is counted where what it does changes**
  (`usage::now_doing`, `SYS_CPU_INFO`): running a program, running the
  kernel, with nothing to do, or taking an interrupt — at each door and on
  the way back, going to the idle loop and leaving it (`resumed`), as it
  sleeps there (`idling`: a processor started into its idle loop never
  switched to it, and one that ran nothing was counted as in the kernel
  for as long as it slept), and around each interrupt's handling — with
  the interrupts it took and the switches it made. A new door, a new way
  to the idle loop, or a new place an interrupt is handled says what the
  processor does now, or that time is counted as whatever it did before. Idle time was thrown away, and
  `/proc/stat`, `/proc/uptime` and every program that asks how busy the
  machine is had nothing to read.

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
  Nothing else sets the first processor's timer, and a processor with
  nothing to run takes no tick: a deadline nobody mentioned is seen to when
  the first processor next runs something, which on a machine with nothing
  to do is never. Seen to at the next tick, as it was, it was every test
  passing and every wait ten milliseconds long. `dtest clock` asks that
  most of twenty waits end on time, not that one does — a wait that ends on
  a tick is on time once in a while, by where in the tick it began. **And
  what is said while the clock is looking is not missed**: a call made
  without the one lock writes its deadline while `expire` may be looking
  through everybody's, and its look may have passed it. So `due` says
  every deadline sooner than any said since the clock began to look
  (`SAID`) before it asks what the timer is set for, and `expire` sets its
  timer before it asks what was said — whichever is second sees the other.
- **Everything about time passing is `clock::expire`.** A new kind of
  deadline is looked at there and answers with its earliest still to come,
  so that the timer is set for it — however far off: there may be no tick
  to set it nearer. What may not return goes first — a signal's deadline,
  alarms, a program's timers, hang-ups — with the timer set a tick ahead in
  case it does not, each put away before its signal is raised; and what is
  due next is worked out after it.
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

- **One processor is in the kernel at a time, but for calls between two
  tasks, a futex's waits and wakes, and the calls about the caller and the
  time** (`klock.rs`), and that is what keeps every other rule in this
  file true: "interrupts off" still means nothing else is in here, but
  them. The lock is taken at the kernel's three doors —
  `syscall_dispatch`, `exception_handler`, `irq_handler` — and nowhere
  else but where a call
  made without it finds something that is the lock's (`with_kernel`). A
  send, a receive, a call, a reply and a notice, every form, a futex's
  wait, wake and requeue, and the calls about the caller and the time — a
  yield, the clock, who it is, how many processors (`syscall::unlocked`)
  — are made without it, under the locks of what they touch — the tasks'
  records, the caller's capability space, a futex's lists, the pages
  checked, the processor's queues — by a task the door has nothing to say
  to: one ended, stopped or barred from its processor while it ran, or one
  whose program traps its calls, comes in under the lock, and a call made
  without it looks again on its way out (`door_has_news`), as
  `leaving_call` does before it runs a handler. Its program's trap and its
  program's signals are asked there under the task's own record's lock and
  not the programs' records' one lock, as hints (`fdtable::traps`,
  `sig_ready_hint`): what they say is asked again under the one lock before
  anything is done about it, and a signal raised after the look interrupts
  the task's processor and comes to its next door. The programs' records
  have one lock, and a door that took it twice a call was where every call
  on every processor met. The task's own lock is what keeps the record
  there: a task leaves its program under it (`fdtable::leave_held`), and
  one ended from another processor is left out while it is still on its
  way out of a call — read with no lock, its program's record could have
  been given back to the heap under it. It is given across a switch
  as the task switched to expects it: one switched out holding it is
  switched back to holding it, one switched out without it without it
  (`kl_held`), and the idle loop always with it; and it is given up on
  every way out to ring 3 and by the idle loop around its `hlt`. A handler
  remembers in its own frame whether it took the lock (`klock::enter`,
  `leave`) and gives back exactly that. A new way into the kernel takes it,
  or is a call between two tasks and takes the locks those take; a new way
  out — a new trampoline to ring 3 — gives it up. Either mistake panics
  rather than hangs: the lock knows who has it.
- **The kernel's locks are taken in the order of their ranks**
  (`sync.rs`; the table is in `docs/smp.md`). Each lock has a rank, the
  one lock's 0, and a processor takes a lock only above every rank it
  holds; two of one kind — two tasks' records, two run queues — are taken
  in the order of where they are, the second at the rank after its kind's
  (`IrqSpinLock::lock_second`). Anything else stops the machine at once,
  naming both (`[KLOCK order: <held> held, <taken> taken]`), whether or not
  another processor was about to take the other: a lock order is a
  deadlock found by its first use, not its first unlucky one. Always
  checked, in every build — `lockcheck` adds a test of the check at boot.
  A new lock gets a rank from the table, and a new place one is taken
  under another keeps to it. And none is held across a switch
  (`switch_to` stops the machine if it is): interrupts are off while one
  is held, and they come back on only with the last one given back, if
  they were on before the first. A processor that has waited thirty
  seconds for any lock says which and whose, and the machine stops; every
  processor then waiting for a lock says which, so that a deadlock says
  where it is.
- **A wait in the kernel parks, or it can end by itself.** A loop that
  tries, finds it cannot, asks whether to wait, is told there is no need
  and tries again is waiting for somebody else — and with the kernel one
  processor's at a time nobody else can come in to do it. On one processor
  a tick ends such a loop, by running whoever it was waiting for; it costs
  a tick and looks like nothing. On several that task is at the door, and
  the machine has every processor busy for good. A terminal's write was
  one: a newline goes out as two bytes, the write took nothing with one
  byte of room, and the wait saw room. So *the question a wait asks is the
  one its retry will be asked* — one function for both (`pty::takes`) —
  and a new wait that can come back without parking is read for that.
  It was found by asking the machine's monitor where each processor was,
  after half an hour of a test saying nothing. The lock says it now: a
  processor that waits thirty seconds names the one that has the kernel,
  that one says where it is (`[KSTUCK ...]`, if its interrupts are on),
  and the machine stops (`klock::waited`).
- **What one processor writes for itself is on a cache line of its own**
  (`sync::Padded`): a stripe of locks of one kind — the tasks' records',
  the address spaces', the capability spaces', a futex's lists, the
  processors' queues — and every per-processor word (`CHOOSING`,
  `RESCHED`, usage's `SINCE`, `SPENT`, `DOING`, `MARK`). Side by side they
  shared lines, and a line one processor writes and then another goes
  back and forth between them at every write; which ones shared was where
  the array happened to fall, so that a change in the futex, which no call
  ran, cost eight pairs of callers a quarter of their calls. Padded,
  `callbench` on eight processors made 2.13 million calls a second with
  eight pairs where it had made 1.32, and 2.26 with four where it had made
  1.89. A new array of locks or of per-processor words is padded.
  And the clock is read as this processor reads it (`clock::now_here`)
  wherever what is measured is this processor's or a deadline: `now`'s
  step, which no time a program is told ever goes back past, is one word
  every processor writes, and a switch and a call with a deadline read
  the clock several times each.
- **"Which processor is this" has an answer only with interrupts off.** A
  task in a system call is moved wherever it can be preempted. Nothing reads
  `percpu::index()` and acts on it later. `percpu::current()` is one
  instruction through GS, and is right whenever it is asked.
- **A task that is not the caller may be running.** In ring 3, on another
  processor, while the kernel ends it or stops it. So: it is marked, its
  processor is interrupted (`smp::interrupt`), and every task is looked at
  again at each door, with the lock held (`scheduler::arrived`) — one that
  was ended goes no further. A new door calls it. And a switch away from a
  task asks whether it was ended under its record's lock, where the ending
  is written (`schedule_inner`): asked before the lock and answered after
  it, a thread ended as it yielded was put back in a queue, its descriptors
  closed and its death told, and ran on (`dtest smp`).
- **A dead task's state does not say it has stopped running;
  `scheduler::ON_CPU` does.** Ended from another processor, a task is on its
  kernel stack and in its address space until its processor leaves it. Its
  parent is told only then (`UNANNOUNCED`, `announce`, or by the next door
  for one caught as its processor was leaving it), `reap_one` will not
  take it apart before, and `space_in_use` — what is asked before an address
  space is thrown away — counts it. Anything new that frees what a task
  stands on asks `ON_CPU`, not `TaskState::Dead`. `SYS_ADDRSPACE_DESTROY`
  asked only whether a task was alive. **And it says a task has left only
  once its registers are saved**: `context_switch` clears it, after them, so
  a processor that takes a task another is still leaving waits for it. Only
  the idle loop waits — nothing waits for the idle loop, and two processors
  swapping tasks would each be waiting for the other's — and without the one
  lock, which the one leaving may need to finish; anything else puts the
  task back and goes to its idle loop. A task taken to run is claimed under
  its record's lock, and one ended meanwhile is not run; and none is taken
  apart while a processor that may have it in hand, chosen and not yet
  claimed, is still choosing: `reap_one` takes it out of every queue,
  stamps it with the reaping epoch, and takes it apart once every
  processor choosing began after the stamp (`CHOOSING`) — a moment later,
  at the next door under the one lock or the next idle loop. Waiting
  instead for nobody to be choosing at all, a reap on busy processors
  would wait for as long as they were busy.
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
  lock — knows it may put the task there and wake it (`runq::place_for`,
  `smp::wake`), and stops saying so the moment it runs anything.
  `unblock_task_next`, which is a reply to a call, wakes nobody: the caller
  goes to the front of the answerer's processor's queue, the answerer is
  about to wait, and the caller runs in its place.
- **Each processor has run queues of its own, and choosing is never a walk
  of them** (`scheduler/runq.rs`): a queue for each band, each in three
  parts — the real-time tasks by priority, the tasks put at the front in
  the order they came, the rest by how far they have run — the first and
  last pairing heaps linked through the tasks' records. A task made ready
  waits where it last ran if it would run there at once, and on a sleeping
  processor, or the one running the worst, if not; a processor with
  nothing of its own takes from the busiest of those running something,
  cold tasks first; and every
  fourth tick one with less to run than the busiest takes a task light
  enough that the two end nearer even. What a processor has to run is
  weighed, not counted (`usage::weight`, a queue's `load`): counted, two
  programs at nought on one processor and two at nice 10 on another were
  even, and the nicer had half the machine where they are owed a tenth of
  it. And weighed band by band: what a better band runs for a moment is
  no reason for a task of a worse one to move — weighed together, a
  server's turn on a processor at the tick sent computing programs
  elsewhere, and the nice ones had a third of the machine in a full dtest
  run. Any processor may put a task in another's queues or take one out,
  under that processor's queues' lock (`sync::RANK_RUNQ`), which keeps the
  queues and the links of the tasks in them, and what a heap orders them
  by — so how far a task has run, or its place, changes only while it is
  out of its queue, in one step with taking it out and putting it back
  (`runq::recount`, `replace`). A move between processors is two steps,
  never two of these locks held: out under one, in under the other. What
  a queue reads of a task's own record — whether it is still ready, where
  it may run — and what placement reads of other processors' queues are
  read without their locks, as hints. What asks whether something better is
  waiting — a tick, a wake between ticks, a call's hand-over — asks the
  queues of the processor it is on, in one step (`runq::best_band`,
  `best_rt`). With one list for the machine, walked at every choice, a
  program of 4,095 threads waking ten times a second kept the kernel
  choosing, and its own first thread waited minutes to run; and as many
  computing threads as processors went round them, each going back into
  the one list at the end of its turn for whichever processor looked
  next. A new question about what is waiting is answered by the queues'
  own structure, not by looking through them.
- **A task runs only where its affinity lets it** (`SYS_AFFINITY`,
  `PerTask::allowed`, `may_run_on`): placement, a pull, balancing, a
  reply put at the front, a call's hand-over and a task put back after its
  turn each keep to it, and a task found running where it may no longer is
  moved at its processor's next door (`arrived`). It is given to threads
  and children and kept by `exec`. Read, it is the online processors of
  it, as Linux answers: a C library counts its bits for how many
  processors there are, and as kept — all 256, for a task told nothing —
  musl's `sysconf` said 256. An idle processor goes round its loop again
  only if something ran (`run_ready`): what waits elsewhere may be
  nothing it may run, and the loop holds the kernel's lock.
- **The clock and every device interrupt the first processor.** The others
  have a tick of their own from their local APIC while they run something,
  and it does one thing: `scheduler::timer_tick`. What is due — timeouts,
  timers, alarms — is seen to in one place, by the first processor
  (`clock::expire`): from its own local APIC's timer, set for the soonest
  thing due however far off, and on its tick in case something was not
  said. A deadline written down on another processor that is sooner than
  that timer is set for is said to the first with an interrupt
  (`idt::VEC_CLOCK`), an ordinary one, which takes the lock.
- **A processor with nothing to run takes no tick** (`scheduler::stop_tick`
  before it naps, `start_tick` as it leaves its idle loop for a task). The
  first masks the 8254's line, where the clock is the counter and its own
  timer is there to fire what is due (`clock::tick_may_stop`); the others
  stop their local APIC's. So nothing may wait for a tick to be seen to: a
  deadline is said to the clock (`clock::due`); a task put to wait on a
  sleeping processor wakes it (`smp::wake`); one left waiting on a busy
  processor that a sleeping one may run goes to wait there from the busy
  one's tick (`runq::hand_to_sleeper`) — not as it is left there, which is
  nearly always a driver or a server preempting for a moment, and woke a
  processor to nothing every time; and an idle processor does not take
  what another idle one was woken to run (`runq::busiest`). What the first processor's
  tick did besides is the clock's (a signal's deadline, a hang-up) or an
  interrupt's: an IOMMU says what it stops (`idt::VEC_IOMMU`). And
  `pit::ticks` stops with it: nothing but the boot may wait on that count.
  A machine with nothing to do takes what its programs have due, and a
  wake for each: `dtest idle` asks for fewer than five hundred interrupts
  in ten seconds, where each processor's tick was a thousand. (ExplOSion's
  console looked at its pipe, its terminal and the keyboard a hundred times
  a second, and the machine was never idle; its network stack still wakes
  five times a second.)
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
- **A processor taken offline runs nothing, and nothing asks it anything**
  (`smp::set_online`, `park`). It is said to be offline at once
  (`percpu::set_offline`), and from then every way a task comes to a
  processor asks whether it is online (`may_run_on`): placement, a pull,
  balancing, a hand-over, a task put back after its turn. What it runs
  moves at its next door, as for an affinity that no longer has it; what
  waits there is given to the others from its idle loop
  (`runq::give_away`); then it parks — no lock, no tick, an interrupt
  answered and nothing more (`idt::irq_handler`) — and a shootdown skips
  it (`tlb::settle`), so it forgets every translation as it comes back,
  before it touches anything that may have changed: no page is global, and
  loading CR3 is all of it. A task kept only to processors that are offline
  may run on any (`runq::place_for`), as on Linux, or it would never run
  again. The first processor is never taken offline: the clock and every
  device are its. A new walk of the processors asks `percpu::online`, and a
  new way to ask one to do something asks it of online ones, or is
  answered by a parked one without the kernel.
- **The other processors are started before there is a task** (`smp::start`
  in `kernel_main`), with what the first processor has turned on: CR0, CR4,
  EFER, the `syscall` MSRs, x2APIC mode (`lapic::init_other`: one that has
  been reset is in xAPIC mode, and its registers as MSRs are a fault).
  Something turned on later on the first — a CR4 bit, an MSR — has to be
  turned on on the others, or a task that moves finds it gone. One that
  does not answer is reset again (INIT) before the next is started on the
  same page, whose words it would otherwise read late. And what is a processor's own each says as it starts:
  its index in TSC_AUX (`cpu::say_processor_index`), where a program's
  `RDPID` and `RDTSCP` find it — left at nought, every processor said it
  was the first — and where it sits (`cpu::place`), which only CPUID on
  that processor can say.

## Known gaps

- Memory past 510 GiB is not used: the kernel's map of it is PML4[0] less
  the gigabytes its stacks and its heap are in.
- Memory is written out only where a system starts a pager for it
  (`swapd` in `../quarkutils`), and only as fast as that pager's disk: it
  writes through the file server, a page a call. With none, or with it
  full, a machine that runs out ends whichever task touched the page it
  could not give (SIGBUS), not the biggest. Memory is looked for when a
  frame is wanted and there is none, by whoever wanted it; nothing looks
  ahead of time. A page a fork left in two programs is not taken while it
  is in both, and a page of a file mapped to be written through is not
  taken at all. Reading an untouched page gives it a frame of its own,
  where Linux maps one shared page of zeroes. Rust programs' heaps still
  come from `SYS_MMAP`, backed at once.
- **A call to a server is not cut short by a signal** (see *Signals*): a
  file's read or write, a socket's, a lock's wait. The handler runs when the
  server answers. Only the real-time signals queue, 64 a program and 16 a
  task behind the first of each number; one past that is not raised, and
  its raiser is told. A handler a program is told of — the way of a
  program written for this system — runs at a system-call boundary and
  nowhere else, and is told of a real-time signal waiting behind another
  only by being told again. And the three *task* signals of `SYS_SIGNAL` — bits in one
  task's notification word, with a deadline — are still what a program
  written for this system is asked to stop with.
- The date is UTC, and there is no time zone: what keeps one is a C library.
  Nothing keeps the clock right once it is set — it runs at the rate the
  processor's counter was measured at when the machine started, which is good
  to a few parts in ten thousand.
- A machine whose processor has no counter that can be trusted — one that is
  not *invariant*, on hardware that is not a hypervisor's — keeps time by the
  tick, ten milliseconds wide, as every machine did. One with no local APIC
  has the fine clock and wakes on ticks.
- A task woken runs at once only if it is of a better band than what a
  processor it may run on is running, or of a higher real-time priority in
  the same band, or a processor is idle: one of the same band waits its
  turn, on time or not. A FIFO task that is preempted goes
  behind its equals rather than ahead of them, as POSIX would have it, and
  `SCHED_BATCH`, `SCHED_IDLE`, `SCHED_DEADLINE` and `SCHED_RESET_ON_FORK`
  are refused.
- The page cache holds a quarter of memory's pages — at least 8192, at
  most a million — across 2,047 objects, memory on its way out included, and
  a machine taking pages from programs faster than its pager writes them
  takes no more until some have been written. Its table is made at boot
  and does not grow.
- A child is the *task's* that made it, not the program's: a thread cannot
  wait for a child another thread of its program forked, which POSIX lets
  any thread do. And the children of a thread that has ended are nobody's:
  no wait is given them, and no SIGCHLD is raised for them.
- A poll set a task is parked on is not held the way a pipe is: a sibling
  closing the set while another thread waits on it leaves that thread to its
  timeout. A one-shot `SYS_POLL` makes a set of its own and is not affected.
- **The kernel is one processor's at a time, but for calls between two
  tasks, a futex's waits and wakes, and the calls about the caller and the
  time** (`syscall::unlocked`). Programs run on every processor; whatever
  else they ask of the kernel waits for it to be empty: a fault, a pipe's
  read or write, a poll, a counter, a timer, a signal descriptor, a served
  descriptor, a terminal and a socket; a fork, an exec, an exit and a
  kill; a mapping made or taken away; a capability given or taken; a
  pager's work; mounts, PCI, power and every device's interrupt; the
  clock's expiry; signals; and a futex word that lends its holder a place.
  The conversion stopped where what it was for was met — four pairs of
  callers on four processors make 3.7 times what one pair makes, where
  under the one lock they made what one made — and nothing comes out from
  under it for its own sake: a program whose threads fault or use pipes on
  many processors at once still meets there. Each of those has the lock
  it would be under (`sync.rs`, the table in `docs/smp.md`), and
  `docs/smp.md` says what taking a fault and a pipe out would mean. A
  pager's receive takes the one lock when one of its objects has gone
  idle, and a call that lends a place to a task better placed than its
  callee takes it to work the places out. And every call of a program's
  threads checks its endpoint in the program's capability space, under
  that space's lock: eight pairs of one program's threads make seven times
  what one pair makes, not eight.
- Every deadline on the machine is the first processor's to fire and to be
  told of: a program on another processor that sets one sooner than the
  first's timer is set for interrupts it to say so, and each wakes it when
  it is due, wherever its task runs. A processor asleep is still asked to
  forget its translations when a kernel stack is given back
  (`tlb::stale_everywhere`), which every task's end does.
- Every device interrupts the first processor, a message included. The
  I/O APIC's lines above the sixteen ISA interrupts are not used: which
  device is on which is in the firmware's bytecode, not its tables. A
  device that wants an interrupt of its own sends a message. One message
  each: nothing gives a device several (MSI-X, or MSI's multiple
  messages).
- PCI is the first segment's, found once at boot: no hot-plug, and a
  machine with more than one segment has the rest left out. A BAR the
  firmware did not place stays unplaced, and a small BAR that shares a
  page with another device's cannot be mapped at all. MSI-X's table is in
  the device's registers, so a driver aims its device's messages there as
  it likes: nothing remaps interrupts. `DeviceMemory` is still one
  authority for every device's registers, and is still handed to `init`,
  though nothing asks for it now; a device that is not a PCI function has
  only that, and above four gigabytes there is none.
- 256 processors at most (`percpu::MAX_CPUS`). An APIC id past 254 needs
  x2APIC mode, which the kernel turns on wherever the processor has it;
  one past 255 is never sent a device's interrupt — without the IOMMU's
  interrupt remapping a message names a processor in eight bits — which
  costs nothing while every device interrupts the first. The processors'
  own stacks are kernel stacks (`kstack.rs`): on a machine of 256, 511 of
  the 32,768, and a task the kernel has no stack for is not made. A
  processor the firmware says could be brought online is not.
- An NMI and a machine check are taken on the stack they arrive on: neither
  has one of its own (an IST entry, as a double fault has). In the three
  instructions `syscall_entry` runs on the program's stack before it finds
  its own, that is the program's — under SMAP a double fault. Nothing a
  program does raises either.
