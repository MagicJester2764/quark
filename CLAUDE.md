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
  frames supplied by another task are mapped *without* it. Adding a new mapping
  path means deciding this deliberately.
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
  and `sys_addrspace_map` accept frames the caller owns (`pmm::owns_range`), so
  a task that allocated a frame may map it holding no capability at all. That is
  what almost every mapper does. A `PhysRange` grant is for frames the allocator
  never owned — the framebuffer, device MMIO — and for a page another task
  allocated and passed over IPC. Never grant `CAP_MAP_PHYS` to narrow it:
  `populate_from_bitmask` expands that bit into a full-range `PhysRange`, which
  silently reopens everything the explicit grants closed.
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

`init` spawns `CONSOLE`, `INPUT` and `VFS` in passes of their own. If a program
misbehaves for lack of a capability, check that its pass actually calls
`grant_caps_from_manifest` — there is no shared path that does it for them, and
INPUT's pass once granted nothing at all, which the UID bypass hid.

## Known gaps

- `PhysRange` is narrow where it can be: console holds exactly the framebuffer,
  and login, the shell and everything they spawn hold none at all. DISK, VFS and
  NET still hold `PhysRange(0, 4 GiB)`, because each maps a DMA page the
  *client* allocated and named over IPC, which has no static extent to grant.
  Closing that needs the frames handed over explicitly — shared memory, or an
  ownership transfer on the IPC — rather than a range grant.
- Endpoint sets are TID bitmasks, not true endpoint objects. A service and its
  clients are named by slot number, not identity.
- The rust fork is one commit on `upstream/main`. Rebasing it means re-checking
  the PAL against std's internals, which move: the allocator PAL shape, the
  futex module location, `RawOsError`'s home and `BorrowedCursor`'s parameters
  have all changed under it before.
