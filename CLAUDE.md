# Working on Quark

Quark is an x86-64 microkernel. It is one of three repos that build together
and must be checked out as siblings:

```
repos/
  quark/       this repo — kernel, drivers, user space
  bang/        UEFI bootloader, and nothing else
  explosion/   the distro: stages the other two and assembles the image
  rust/        fork of rust-lang/rust carrying the x86_64-unknown-quark std PAL
```

`explosion` refers to `../quark` and `../bang`, and the fork's
`library/Cargo.toml` patches `quark-rt` through the relative path
`../../quark/user/quark-rt`. Anything other than a flat sibling layout breaks
the build. The dependency runs one way — the distro reaches down to the kernel
and the bootloader, never the reverse.

## Toolchain

Pinned to `nightly-2026-03-01` in `rust-toolchain.toml`. `../bang` pins the
same nightly, but for its own reasons rather than to match this tree: it does
not depend on the fork at all, and pins because newer toolchains rewrite the
uefi crate's UCS-2 loops into a `wcslen` libcall it has to supply. Keeping the
two equal only saves rustup a second download.

**The pin must equal the commit the fork is based on.** `../rust`'s `library/`
is a checkout of upstream at one commit and only compiles with the rustc built
from it; a newer compiler rejects its own `core` (`impl const Trait for Type`
becomes "expected a trait, found type", features get removed). This is
checkable rather than guessable:

```bash
rustc --version                 # ... (38c0de8dc 2026-02-28)
git -C ../rust log -1 --format=%H $(git -C ../rust merge-base HEAD upstream/main 2>/dev/null || echo HEAD~2)
```

The short hash in `rustc --version` must match the fork's base commit. It does:
`38c0de8dc` for both. If you rebase the fork, move both pins in the same commit.

Note the off-by-one — the date in a rustup channel is the *publish* date, so
`nightly-2026-03-01` is the build dated 02-28. A floating `nightly` channel is
what to avoid: it drifts forward and silently leaves the fork behind.

Fresh machine:

```bash
rustup toolchain install nightly-2026-03-01
rustup component add rust-src llvm-tools-preview --toolchain nightly-2026-03-01
rustup target add x86_64-unknown-none x86_64-unknown-uefi --toolchain nightly-2026-03-01
git -C ../rust submodule update --init --depth 1 library/backtrace
```

That submodule is the easy one to miss. Without it `std` fails with
`couldn't read .../backtrace/src/lib.rs`, which stops the build before
`../explosion` can stage a kernel — so the image keeps whatever it had and you
debug the wrong binary.

The fork is needed only for the hosted `hello`. Without it on disk, `make`
skips that one program and says so; the kernel and every other program still
build, because a kernel should not need a patched rustc checkout to compile.

## Build and run

```bash
make            # kernel, drivers and every user program
make install DESTDIR=<dir>   # stage the artifacts for a distro to consume

cd ../explosion
make run        # stage both trees, assemble the image, boot it in QEMU
```

Quark builds a kernel and the programs that run on it. It does not know what an
image looks like, and nothing here reaches into a sibling repo — `make install`
lays artifacts out and ExplOSion collects them.

The hosted `hello` compiles `std` from source and is the memory-hungriest step;
an unexplained build death is usually the OOM killer. Note that
`cargo -Z build-std` does not track `quark-rt`, which reaches `hello` only
through the fork's `library/Cargo.toml` patch, so the Makefile hashes the
quark-rt sources and cleans the hosted build when they change. Without that,
editing quark-rt leaves a stale `hello` linked against the previous copy —
which is how it ended up calling pre-Phase-0 syscall numbers and faulting.

ExplOSion's QEMU targets pass `-cpu max` deliberately. Default CPU models expose
neither SMEP nor SMAP, so the kernel's supervisor-mode protections are silently
inactive without it — a boot test on the default CPU proves nothing about them.
The kernel prints which it enabled to serial at boot.

Only serial reaches stdout; `console::puts` goes to the framebuffer. To see
user-space output headlessly, screendump over QMP rather than assuming the
system hung.

## Invariants that must not regress

These were established deliberately. Breaking one silently re-opens a hole.

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
- **A spawner gives a program its pages; it does not lend them.** The loader
  builds the image in its own `sys_mmap` memory and moves it across, so the
  child owns its code and stack and frees them when it goes. Lending frames
  with `sys_addrspace_map` kept them the spawner's: a shell leaked every program
  it ran, and a spawner that exited first freed them under its children.
- **A dead task holds all its memory until it is reaped.** `sys_wait` reaps the
  child it returns; the idle loop reaps the rest. Anything that collects a
  child some other way must reap it too, or a parent running programs back to
  back — which never lets the machine idle — runs out of memory.
- **Every task has its own floating-point state**, saved and restored on every
  switch (`fpu.rs`). The kernel is soft-float and never touches the registers,
  so this is the whole of it. FXSAVE is enough only while CR4.OSXSAVE is clear:
  enabling AVX without moving to XSAVE hands one task another's YMM registers.
- **The argument page carries the program's own header table** — its end
  belongs to it however long the command line is (`spawn::PHDRS_AT`, mirrored
  in `user/libc/include/quark/layout.h`). musl finds a static program's
  thread-local template through it, and without it every thread-local lands
  outside its block. So every spawner calls `set_args`, even with no
  arguments; a program reading an argument page that was never mapped faults.
- **A fault in ring 3 ends the task, never the machine.** The task exits with
  the negated Linux signal number (`idt.rs`), and only a fault taken in ring 0
  halts. musl's `abort()` is a privileged `hlt`, so before this, one failed
  assert stopped everything.
- **C objects must put constructors in `.init_array`.** The cross compiler is
  configured `--enable-initfini-array`, and the user link script places the
  arrays and refuses `.ctors` outright: nothing here links the crtbegin that
  would run them, so an object carrying them has constructors that silently
  never run.
- **Capabilities are the authority.** There is no UID 0 bypass; `uid == 0` no
  longer short-circuits `cap::task_has_*`. A service that cannot do something
  is missing a capability, not a privilege level.
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
- **A server copies what a client lent; it never maps a client's page.** Data
  travels with the call (`SYS_CALL_LEND`, then `SYS_LENT_READ` and
  `SYS_LENT_WRITE`), so no server needs authority over physical memory to serve
  anyone. A protocol that names a physical address makes its server a deputy
  that will read or write any page in the machine — that is why the disk, VFS
  and NET servers once held all of it. A shared-memory handle is no better
  when the request names it: handles are global numbers.
- **No task holds a `PhysRange` wider than one device.** init is started with
  the framebuffer and its boot modules, `fb` holds the framebuffer and lends it
  on, and no driver holds any. `dtest physical` walks every CSpace and fails
  otherwise.
- **A program declares what it needs; a spawner grants from that.** Capabilities
  come from a `quark_rt::manifest!` block compiled into the image, found by
  scanning for its magic, not from a table of names in `init`. A spawner mints
  each request from a capability it already holds, so it can never hand out more
  than it has — the shell holds no `PhysRange` and therefore cannot give one
  away. The framebuffer is the one exception: its address comes from the
  bootloader at runtime, so `init` grants it directly.
- **IPC needs an Endpoint capability.** `sys_send`/`sys_call`/`sys_notify` are
  gated by a destination bitmask. IPC the kernel performs through an installed
  fd bypasses this on purpose: the fd is the authorisation, and only a
  CAP_TASK_MGMT holder can install one. Reaping clears a dead TID's bit from
  every CSpace, because TIDs are recycled and a stale bit would otherwise
  transfer to the slot's next occupant.

`init` spawns `FB`, `CONSOLE`, `INPUT` and `VFS` in passes of their own. If a
program misbehaves for lack of a capability, check that its pass actually calls
`grant_caps_from_manifest` — there is no shared path that does it for them, and
INPUT's pass once granted nothing at all, which the UID bypass hid.

## The screen

`user/fb` is the framebuffer device: it owns the hardware the way `/dev/fb0`
does, knows the mode, and decides who draws. It has no opinion about windows.

Everything else is a client of it. `user/qtty` is the text console: it claims
the display at boot and draws fullscreen — that is what the machine boots into,
a plain TTY. `user/wm` is a compositor you *run*: `wm <program>` takes the
display, starts that program, composites its windows, and gives the display back
when it exits.

`wm` speaks **Wayland**, not a protocol shaped like it. It hands each program a
socketpair end as descriptor 3 and `WAYLAND_SOCKET=3`, which is what
`wl_display_connect` looks at first — so upstream libwayland runs unpatched.
`user/wm/src` is one module per part of that: `client` (a connection and its
buffered bytes), `objects` (one id table per client, which is what makes "a
client cannot name another client's objects" true by construction), `surface`,
`shell`, `shm`, `seat`, `clipboard`, `cursor`, `keymap`, `protocol`, `draw`.
`user/wmdemo` and the older six-tag window protocol still work alongside it.

The keyboard goes with it, and so does the pointer. `user/input` has the same
claim protocol: while a program holds it, raw key and pointer events go to that
program and line readers wait. The compositor claims input when it claims the
display and hands each event to the focused window — whoever owns the screen
owns the keyboard, the way switching virtual terminals has always worked.

Both come from one driver. A PS/2 mouse is not a second device: it is the same
i8042 answering on the same data port 0x60, with IRQ 12 instead of 1 and bit 5
of the status port saying which device a byte came from. `user/keyboard` holds
both lines and routes on that bit — never on which interrupt fired, because a
byte for one device can be waiting when the other's interrupt arrives. Two
drivers sharing port 0x60 would take each other's bytes, and the symptom of
losing that race is a keyboard that types rubbish or stops.

Three things to know before changing any of it:

- **The display is lent, not shared.** `init` grants the framebuffer
  `PhysRange` to `fb` and nowhere else; `fb` mints a derived capability per
  claimant and revokes it to take the display back. Revocation governs the
  right to *map*, not mappings that already exist, so the outgoing owner is
  told and answers before the new one is let in — and must empty its
  capability slot, since granting into an occupied one fails.
- **Guard every framebuffer write on still owning the display**, not just the
  flush. The console gated its flush and not `hide_cursor` or `scroll`, and
  carried on writing into memory it had just unmapped.
- **Composite into a back buffer.** Painting onto the visible surface means the
  cleared screen is briefly the one on the monitor, once per frame; a cursor
  blink is enough to make that a visible flash. `sys_mmap`/`sys_munmap` take at
  most 256 pages, so a screenful takes a loop.
- **Repaint the region that changed, not the screen.** A commit says which
  window changed; repainting all of it costs a megapixel of backdrop and a
  four-megabyte copy, which a client committing a dozen times a second turns
  into a compositor with no time left to read the keyboard. `wm` clips every
  drawing primitive to a region and copies only that region out.
- **Events are pulled, not pushed.** A server cannot originate IPC to a program
  it spawned: `sys_send`/`sys_call` need an `Endpoint` naming the destination,
  and a TID that did not exist at spawn time cannot be minted into one. A reply
  needs no capability, so every hop here is the client asking.

## Scheduling

Four bands, best first: drivers, servers, ordinary programs, idle. A task runs
only when nothing better is waiting and takes turns within its own band. A
program asks for a band in its `manifest!` block alongside its capabilities,
and a spawner applies it under the same narrowing rule — it can never grant a
better band than it is in, so only `init` can put a driver in the driver band.

Three things follow from that, and breaking any of them is quiet:

- **Waiting means blocking.** A task in a better band that spins on
  `sys_yield` is immediately runnable again, so nothing below it ever runs.
  This is fatal rather than merely wasteful now: `nameserver::lookup_retry`
  yielded a hundred times between tries and starved the VFS out of ever
  registering. Use `sleep_ticks`, or `sys_recv_timeout` if there is also
  something to hear.
- **A synchronous call hands over the CPU.** The caller has blocked and has
  nothing to contribute until the reply, so the callee is switched to directly
  and runs on what is left of the caller's slice rather than a fresh one. A
  server does not earn a quantum every time it is called.
- **A task runs at the band of whoever is waiting on it**, for as long as that
  is true. Without it a server called by something urgent is preempted by
  anything in between. It is also what lets the direct switch stay safe: the
  callee already carries the caller's band when the scheduler decides whether
  handing straight over would run something ahead of its betters.

## Known gaps

- Endpoint sets are TID bitmasks, not true endpoint objects. A service and its
  clients are named by slot number, not identity.
- Nothing is demand-paged: memory is backed when it is mapped, not when it is
  first touched. A program that maps far more than it uses — pixman's stress
  test asks for a 2.7 GB mask and draws into a corner — is refused where Linux
  would say yes, and a request bigger than free memory takes all of it for a
  moment before it is. The C library's `mmap` gives back a partial mapping, so
  the refusal is clean; the program has to check for it.
- Focus is a single stack with little policy: Tab cycles, a new window takes it,
  and a click raises the one under the pointer. Keyboard focus and pointer focus
  are tracked separately, as Wayland requires, but there is no follow-mouse, no
  focus stealing prevention, and no way to move or resize a window.
- The rust fork is one commit on `upstream/main`. Rebasing it means re-checking
  the PAL against std's internals, which move: the allocator PAL shape, the
  futex module location, `RawOsError`'s home and `BorrowedCursor`'s parameters
  have all changed under it before.
