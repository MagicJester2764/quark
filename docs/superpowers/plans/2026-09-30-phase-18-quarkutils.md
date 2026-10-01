# Phase 18 — `quarkutils`: the kernel repo holds the kernel and nothing else

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.
> Inline execution, commits on `main`, a boot test where a task changes what
> boots.

**Goal:** `quark` builds a kernel with no `user/` directory in it, `quarkutils`
builds every program against the ABI the kernel *installed*, and ExplOSion
assembles an image from the two plus `bang` — booting to the same shell.

**Architecture:** the userland leaves with its history (`git filter-repo`, so
`git log` and `git blame` keep working in the new repo) and nothing is rewritten
in `quark`, which just stops carrying it. The two repos never name each other's
checkout: the kernel's `make install` puts a generated header and the ABI
document in a `DESTDIR`, and `quarkutils` checks its own copy of the numbers
against that. ExplOSion, which is allowed to reach down, installs one and then
the other into the same stage.

**Tech Stack:** git-filter-repo; make; the pinned nightly; ExplOSion's
`tools/boot-test.sh`, `dtest`, `runtests` and `e2fsck`.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 18 — `quarkutils`".

## Global Constraints

- **No history is rewritten in `quark`.** The userland's past stays in its
  log; one commit removes the tree.
- **Flat siblings:** `~/src/osdev/{quark,quarkutils,bang,explosion,rust}`.
- **Neither `quark` nor `quarkutils` names the other's checkout.** What crosses
  is what `make install` put in a `DESTDIR`, and nothing else.
- **Each of the two builds alone.** `quarkutils` with no kernel on disk skips
  the comparison against the kernel's ABI and says so, the way `make` already
  skips the hosted programs when the std fork is absent.
- **Behaviour does not change** except where a task says it does: `dtest` 267,
  `runtests` 23 and 31, `e2fsck` clean, and GTK's `hello-world` still draws, on
  ext2 and ext4.
- **Publishing is the user's call.** `quarkutils` needs a GitHub repository
  that does not exist; nothing that only makes sense once it does is pushed
  before the user says so.

## File Structure

After the phase:

```
quark/                      the kernel
  src/  drivers/  linker.ld  Cargo.toml  .cargo/  rust-toolchain.toml
  docs/abi.md               the contract
  docs/superpowers/plans/   the project's plans, where they have always been
  tools/check-abi.sh        the document agrees with the source
  tools/gen-abi-header.sh   src/syscall.rs -> quark/abi.h
  Makefile                  kernel.bin, the two .drv modules, install

quarkutils/                 everything that runs on it (was quark/user/)
  quark-rt/  init/  nameserver/  keyboard/  disk/  vfs/  net/  fb/  qtty/
  input/  wm/  qsh/  login/  ...          one directory per program, as before
  libc/  linux-abi/         the C library and the Linux system-call surface
  linker.ld                 the user link script
  x86_64-unknown-quark.json the hosted target
  rust-std-patches/         the seed the std fork grew from
  docs/vfs.md  docs/wayland.md
  tools/check-abi.sh        its numbers agree with each other, and with the
                            kernel's when the kernel's are installed
  Makefile  rust-toolchain.toml  README.md  CLAUDE.md
```

---

### Task 1: The document is the contract, so it has to be complete

With one repo the kernel's source was what the runtime was checked against and
`docs/abi.md` was a description of it. With two, the document *is* the
interface — and three calls have no entry in it beyond a mention in the version
history.

**Files:**
- Modify: `quark/tools/check-abi.sh`
- Modify: `quark/docs/abi.md`

- [x] **Step 1: make the check fail on what is wrong now.** Add to
  `tools/check-abi.sh`: every `SYS_*` constant in `src/syscall.rs` has a row in
  a reference table of `docs/abi.md` with the same number (either column
  order — the deprecated table lists name first), no row names a call the
  kernel does not have, and the document's `**Version X.Y.**` equals
  `ABI_VERSION_MAJOR.ABI_VERSION_MINOR`. Run it; expect it to fail naming
  `SYS_ADDRSPACE_SELF` (41), `SYS_SET_FS_BASE` (102) and
  `SYS_TASK_START_ARG` (103).
- [x] **Step 2: write the three rows**, from what the kernel does: read each
  arm of the dispatch for its arguments, its return values and what it asks
  for.
- [x] **Step 3: run the check; expect it to pass.** Commit.

---

### Task 2: The kernel installs its ABI

**Files:**
- Create: `quark/tools/gen-abi-header.sh`
- Modify: `quark/Makefile` (the `install` target)

**Interfaces:**
- Produces: `$(DESTDIR)/usr/include/quark/abi.h` — `QUARK_ABI_VERSION_MAJOR`,
  `QUARK_ABI_VERSION_MINOR` and one `#define SYS_NAME number` per call, sorted
  by number — and `$(DESTDIR)/usr/share/doc/quark/abi.md`.

- [x] **Step 1: `tools/gen-abi-header.sh`** reads `src/syscall.rs` and writes
  the header to stdout. Generated, never edited: the Rust constants stay the
  one place a number is written in this repository.
- [x] **Step 2: `make install`** writes both files. Check the header compiles
  (`cc -fsyntax-only`) and carries 113 calls.
- [x] **Step 3:** commit.

---

### Task 3: `quarkutils`, with its history

**Files:**
- Create: `~/src/osdev/quarkutils/` (a new repository)
- Create in it: `Makefile`, `rust-toolchain.toml`, `.gitignore`,
  `tools/check-abi.sh`, `README.md`, `CLAUDE.md`

- [x] **Step 1: record what the tree installs today**, to compare against:
  `make -C quark install DESTDIR=/tmp/.../before`, and the sorted list of files
  with their sizes.
- [x] **Step 2: split.** A fresh clone of `quark`, then
  ```bash
  git filter-repo --path user/ --path x86_64-unknown-quark.json \
      --path rust-std-patches/ --path docs/vfs.md --path docs/wayland.md \
      --path-rename user/:
  ```
  The clone's `origin` is removed by the tool, which is wanted: nothing here
  should be pushable to `quark` by accident.
- [x] **Step 3: the build.** A `Makefile` made from the user half of
  `quark/Makefile` — the same programs, the same install layout
  (`drivers/init.elf`, `boot/*.ELF`, `usr/bin/*.ELF`, `etc/PASSWD`), one rule
  per kind of program instead of one per program. `rust-toolchain.toml` with
  the same pin, because a crate here no longer inherits the kernel's.
- [x] **Step 4: its own check.** `tools/check-abi.sh`: quark-rt and the C
  header agree, no number is used twice; and, given the kernel's installed
  `abi.h`, quark-rt's table equals it exactly. With no installed header it
  says it skipped that half.
- [x] **Step 5: build it alone and compare.** `make install` into a fresh
  directory, then the kernel's install into the same one; the file list must
  equal step 1's.
- [x] **Step 6:** `README.md`, `CLAUDE.md` (the userland half of
  `quark/CLAUDE.md`, every paragraph on exactly one side), commit.

---

### Task 4: The fork's patch path

**Files:**
- Modify: `rust/library/Cargo.toml` (one line)

- [x] **Step 1:** `quark-rt = { path = '../../quarkutils/quark-rt' }`, a
  commit on the fork's `quark` branch.
- [x] **Step 2:** `make` in `quarkutils` builds `hello` and `httpget` against
  it; the stamp logic that cleans a hosted build when quark-rt changes moves
  with the rule.

---

### Task 5: ExplOSion collects from three

**Files:**
- Modify: `explosion/Makefile`
- Create: `explosion/toolchain/musl-wrappers.sh` (out of `build-musl.sh`)
- Modify: `explosion/toolchain/build-musl.sh`, `build.sh`,
  `build-xkbcommon.sh`, `README.md`s

- [x] **Step 1: staging.** `QUARKUTILS_DIR ?= ../quarkutils`; the kernel is
  installed first and the userland second, into the same stage, with the
  kernel's header required rather than optional — the integrated build is the
  one place the comparison must not be skipped.
- [x] **Step 2: the toolchain's paths.** The musl specs name three things
  inside the userland checkout — the C library's headers, `manifest.o` and
  `liblinux-abi.a`. They become `$QUARKUTILS_DIR/...`, and writing the specs
  and the two wrappers becomes a script of its own so that moving a checkout
  does not mean rebuilding musl. `QUARK_SRC` stops meaning two things.
- [x] **Step 3: boot.** The image assembles from the three and boots to the
  shell; `dtest` passes. `quark/user/` is still on disk at this point and
  nothing reads it.

---

### Task 6: A userland that says which ABI it was built for

Two repositories can be built at different times, which one could not: a
runtime compiled for ABI 3 can now meet a kernel that has moved on.

**Files:**
- Modify: `quarkutils/quark-rt/src/syscall.rs`, `quarkutils/tools/check-abi.sh`,
  `quarkutils/init/src/main.rs`

- [x] **Step 1:** `quark_rt::syscall::ABI_VERSION_MAJOR` / `_MINOR`: the ABI
  this runtime was written against. The check compares them with the installed
  header: same major, and a minor the kernel has reached.
- [x] **Step 2:** `init` asks the kernel (`SYS_ABI_VERSION`, which has not
  moved since 1.0) before it does anything else, says both versions, and stops
  if the major differs — every number after that is a guess.
- [x] **Step 3:** boot; the line is on the console. Commit.

---

### Task 7: `quark` holds the kernel and nothing else

**Files:**
- Delete: `quark/user/`, `quark/x86_64-unknown-quark.json`,
  `quark/rust-std-patches/`, `quark/docs/vfs.md`, `quark/docs/wayland.md`
- Modify: `quark/Makefile`, `quark/tools/check-abi.sh`, `quark/.gitignore`,
  `quark/README.md`, `quark/CLAUDE.md`

- [x] **Step 1:** `git rm`; the Makefile keeps the kernel, the two `.drv`
  modules, the GRUB image and `install`.
- [x] **Step 2:** `tools/check-abi.sh` loses the two comparisons that moved and
  keeps what is the kernel's: one number per call, and a document that agrees.
- [x] **Step 3:** `README.md` and `CLAUDE.md` say what is here now and where
  the rest went.
- [x] **Step 4:** `make clean && make` with no `user/` on disk. Commit.

---

### Task 8: Acceptance

- [x] **Step 1:** everything rebuilt from the new layout — both repos clean,
  the C world relinked against the regenerated specs.
- [x] **Step 2:** ext2 and ext4: `dtest`, `runtests /etc/libc.tests`,
  `runtests /etc/pixman.tests`, `wm hello-world`, `check-rootfs.sh`.
- [x] **Step 3:** `ROADMAP.md`; the memory index's paths.
- [x] **Step 4:** the question that is the user's: create
  `MagicJester2764/quarkutils` and push. *Answered: public, like the others;
  all four are pushed.*
