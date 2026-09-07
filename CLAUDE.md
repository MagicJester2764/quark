# Working on Quark

Quark is an x86-64 microkernel. It is one of three repos that build together
and must be checked out as siblings:

```
repos/
  quark/   this repo — kernel, drivers, user space
  bang/    UEFI bootloader; also owns the disk image and QEMU targets
  rust/    fork of rust-lang/rust carrying the x86_64-unknown-quark std PAL
```

`bang` refers to `../quark`, and the fork's `library/Cargo.toml` patches
`quark-rt` through the relative path `../../quark/user/quark-rt`. Anything
other than a flat sibling layout breaks the build.

## Toolchain

Pinned to `nightly-2026-08-26` in `rust-toolchain.toml`. The pin is load
bearing: the fork's `library/` tracks a specific rustc vintage, and a floating
`nightly` channel drifts out from under it. Note the off-by-one — the date in a
rustup channel is the *publish* date, so `nightly-2026-08-26` is the build
dated 08-25. Pinning to `nightly-2026-08-25` selects an *older* compiler that
is missing built-ins `library/` needs.

Fresh machine:

```bash
rustup toolchain install nightly-2026-08-26
rustup component add rust-src llvm-tools-preview --toolchain nightly-2026-08-26
rustup target add x86_64-unknown-none x86_64-unknown-uefi --toolchain nightly-2026-08-26
git -C ../rust submodule update --init --depth 1 library/backtrace
```

That submodule is the easy one to miss. Without it `std` fails with
`couldn't read .../backtrace/src/lib.rs`, which stops `make sync-quark` before
it copies `kernel.bin` — so the image silently keeps booting a stale kernel and
you debug the wrong binary.

## Build and run

```bash
cd ../bang
make sync-quark   # builds the kernel + all user programs, copies artifacts here
make hd           # assembles hdimage.bin
make run          # QEMU
```

`make sync-quark` compiles `std` from source and is the memory-hungriest step;
an unexplained build death is usually the OOM killer.

`make run` passes `-cpu max` deliberately. QEMU's default CPU models expose
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
- **IPC needs an Endpoint capability.** `sys_send`/`sys_call`/`sys_notify` are
  gated by a destination bitmask. IPC the kernel performs through an installed
  fd bypasses this on purpose: the fd is the authorisation, and only a
  CAP_TASK_MGMT holder can install one. Reaping clears a dead TID's bit from
  every CSpace, because TIDs are recycled and a stale bit would otherwise
  transfer to the slot's next occupant.

`init` grants capabilities per program in `grant_caps_by_name`. Note that
`CONSOLE`, `INPUT` and `VFS` are each spawned in their own pass — if a program
misbehaves for lack of a capability, check that its pass actually calls
`grant_caps_by_name`. INPUT's did not, and ran with an empty CSpace for as long
as the UID bypass hid it.

## Known gaps

- `sys_phys_free` verifies ownership per frame, but `init` hands services
  `PhysRange(0, 4 GiB)`, so that capability constrains little in practice.
- Endpoint sets are TID bitmasks, not true endpoint objects. A service and its
  clients are named by slot number, not identity.
- The rust fork is one commit on `upstream/main`. Rebasing it means re-checking
  the PAL against std's internals, which move: the allocator PAL shape, the
  futex module location, `RawOsError`'s home and `BorrowedCursor`'s parameters
  have all changed under it before.
