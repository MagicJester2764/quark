# Phase 16 — GTK, and the Unix under it

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.
> Inline execution, commits on `main`, a boot test per task.

**Goal:** an application somebody else wrote, built against a toolkit somebody
else wrote, draws a window on Quark.

**Architecture:** the toolkit stands on a floor — a C++ compiler, waiting that
works, glib, text shaping — and every part of that floor is a port of upstream
sources with *no patches to them*. Where a port needs something Quark has not
got, Quark grows it. Each task ends with a program running on Quark that says
so.

**Tech Stack:** gcc 15.2 (c,c++) and libstdc++ against musl; glib 2.82.5;
harfbuzz; fribidi; pango; graphene; gdk-pixbuf; libepoxy; GTK 4; all cross-built
by scripts in `../explosion/toolchain`, staged into the image by
`../explosion/Makefile`, and run under QEMU by `tools/boot-test.sh`.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 16 — GTK and Qt".

## Global Constraints

- **Nothing patches an upstream client.** If weston, glib, pango or GTK needs
  changing to run here, the bug is in Quark. `config.sub` learning the word
  `quark` is not a patch to the program; it is a patch to autoconf's idea of
  what operating systems exist.
- **Every hole found gets a test in `../explosion/toolchain/tests`**, named in
  `tests/libc.tests`, so `runtests` keeps it caught.
- Commits on `main`, one per task, pushed. `../../ROADMAP.md` updated at the end.
- The ABI is bumped once for the phase, to **3.0**, and `tools/check-abi.sh`
  must pass.
- Verification is a boot in QEMU with a screenshot; `dtest` and `runtests` must
  stay green on ext2 *and* ext4, and `check-rootfs.sh` clean after.

---

### Task 1: A C++ compiler for Quark

Harfbuzz is C++, and so is every part of Qt. `x86_64-quark-gcc` was configured
`--enable-languages=c`, so there is no `cc1plus` and no `libstdc++` — a C++
program cannot be compiled at all, let alone linked.

**Files:**
- Modify: `../explosion/toolchain/build.sh` (add `c++` to the languages)
- Create: `../explosion/toolchain/build-libstdcxx.sh`
- Modify: `../explosion/toolchain/build-musl.sh` (write the `musl-g++` wrapper)
- Create: `../explosion/toolchain/tests/cxxtest.cpp`
- Modify: `../explosion/toolchain/build-tests.sh` (build `.cpp` tests too)
- Modify: `../explosion/toolchain/tests/libc.tests`, `README.md`

**Interfaces:**
- Produces: `x86_64-quark-g++`, `x86_64-quark-musl-g++`, `libstdc++.a` and
  `libsupc++.a` in the musl prefix, and C++ headers under
  `$PREFIX/include/c++/15.2.0`.

- [x] **Step 1: add C++ to the cross compiler**

  In `build.sh`, `--enable-languages=c` becomes `--enable-languages=c,c++`.
  `--disable-libstdcxx` **stays**: gcc's in-tree libstdc++ would be built
  against the sysroot's tiny `libc`, which has no `wchar.h` and no threads.
  libstdc++ is built separately, against musl, in step 3.

- [x] **Step 2: build and install it**

  ```bash
  mkdir -p ~/src/build-gcc-quark-cxx && cd ~/src/build-gcc-quark-cxx
  ~/src/gcc/configure --target=x86_64-quark --prefix="$HOME/opt/cross" \
      --with-sysroot="$HOME/opt/cross/x86_64-quark/sys-root" \
      --enable-languages=c,c++ --enable-initfini-array \
      --disable-nls --disable-shared --disable-threads \
      --disable-libssp --disable-libquadmath --disable-libatomic \
      --disable-libgomp --disable-libvtv --disable-libstdcxx
  make -j16 all-gcc && make -j16 all-target-libgcc
  make install-gcc install-target-libgcc
  x86_64-quark-g++ --version      # expect: 15.2.0
  ```

- [x] **Step 3: write `build-libstdcxx.sh`**

  libstdc++-v3 configures on its own, which is what makes this possible: it is
  built with the musl wrapper, for the musl prefix, and never sees the other
  sysroot.

  ```sh
  SRC=${1:?usage: build-libstdcxx.sh <gcc-src>}
  PREFIX=${PREFIX:-$HOME/opt/cross/x86_64-quark/musl}
  VER=$(cat "$SRC/gcc/BASE-VER")
  rm -rf "$SRC/build-libstdcxx-quark"
  mkdir -p "$SRC/build-libstdcxx-quark" && cd "$SRC/build-libstdcxx-quark"
  "$SRC/libstdc++-v3/configure" --host=x86_64-quark --prefix="$PREFIX" \
      --disable-shared --enable-static --disable-libstdcxx-pch \
      --disable-nls --disable-libstdcxx-verbose \
      --with-gxx-include-dir="$PREFIX/include/c++/$VER" \
      CC=x86_64-quark-musl-gcc CXX=x86_64-quark-musl-g++ \
      CFLAGS="-O2" CXXFLAGS="-O2"
  make -j"$(nproc)"
  make install
  ```

  `--disable-libstdcxx-verbose` because the verbose terminate handler prints
  through `fputs` to `stderr` before anything has set up a FILE, and there is
  nothing to gain from a message that may not arrive.

- [x] **Step 4: write the `musl-g++` wrapper**

  In `build-musl.sh`, beside the `musl-gcc` wrapper. It is the same rotation of
  arguments and the same specs file, with the C++ include directories added by
  hand: the specs say `-nostdinc`, which takes away the compiler's own idea of
  where its headers are, and the C++ ones are among them.

  ```sh
  exec x86_64-quark-g++ -specs="$PREFIX/lib/musl-quark.specs" \
      -isystem "$PREFIX/include/c++/$VER" \
      -isystem "$PREFIX/include/c++/$VER/x86_64-quark" \
      -isystem "$PREFIX/include/c++/$VER/backward" "$@"
  ```

- [x] **Step 5: write the failing test**

  `tests/cxxtest.cpp` — a static constructor, `std::vector`, `std::string`,
  `std::map`, `std::sort`, a virtual call through a base pointer, `dynamic_cast`
  and a thrown exception caught by type. Exceptions are the interesting one:
  unwinding reads `.eh_frame_hdr` through `dl_iterate_phdr`, which on a static
  program means the program headers must be in the auxv — which they are,
  because the argument page carries them.

- [x] **Step 6: build the tests with a C++ compiler**

  `build-tests.sh` loops over `tests/*.c`; it takes `tests/*.cpp` too, built
  with `x86_64-quark-musl-g++`. Add `cxxtest` to `tests/libc.tests`.

- [x] **Step 7: boot and check**

  ```bash
  sh $SP/rebuild.sh hd
  RUNDIR=/tmp/claude-1000/run16 sh tools/boot-test.sh <keys> <shot.ppm>
  ```
  Expected: `cxxtest: 0 failed`.

- [x] **Step 8: commit**

---

### Task 2: Waiting that works

Everything with a main loop waits the same way: poll, with a timeout, on a
descriptor another thread can make ready. Four pieces of that are missing or
lying, and each was found by running a real program.

**Files:**
- Modify: `user/linux-abi/src/syscall.c` (futex timeout, `eventfd2`, `poll`)
- Modify: `user/linux-abi/src/files.c` (`read`/`write` honour `O_NONBLOCK`)
- Modify: `user/linux-abi/src/net.c` (`pipe2(O_NONBLOCK)`)
- Modify: `user/linux-abi/src/abi.h` (`LX_ETIMEDOUT`)
- Modify: `src/syscall.rs`, `src/task.rs`, `src/pipe.rs`, `src/pollset.rs`
- Create: `src/eventfd.rs`
- Modify: `docs/abi.md`, `user/libc/include/quark/syscall.h`
- Create: `../explosion/toolchain/tests/polltest.c`,
  `../explosion/toolchain/tests/gsync.c`

**Interfaces:**
- Produces: `SYS_EVENT_CREATE` (kernel), `FdKind::Event`, a `read`/`write` that
  returns `EAGAIN` rather than blocking when the descriptor says non-blocking.

- [x] **Step 1: write the failing tests**

  `tests/polltest.c`: the monotonic clock advances; `poll` times out; `poll`
  sees a ready pipe; `poll` with no descriptors is a sleep; `eventfd` counts,
  polls and empties; another thread wakes a waiting poll.

  `tests/gsync.c`: a futex wait with a timeout comes back; a mismatched futex
  does not wait; pthread and GMutex under contention; GCond both ways;
  `g_cond_wait_until` gives up. (This one needs glib, so it is skipped by
  `build-tests.sh` until Task 3 — write it now, run it then.)

- [x] **Step 2: a futex wait with a timeout**

  `LX_futex` drops its fourth argument, so every timed wait waits for ever, and
  it returns 0 whatever happened, so a caller cannot tell a timeout from a
  wake. `g_cond_wait_until` is `FUTEX_WAIT` with a relative timespec and
  `errno == ETIMEDOUT`; so is musl's `sem_timedwait`. Use
  `SYS_FUTEX_WAIT_TIMEOUT` (130), rounding the timespec up to 100 Hz ticks, and
  map the kernel's answers: 1 → `EAGAIN`, 2 → `ETIMEDOUT`.

- [x] **Step 3: `O_NONBLOCK` on a descriptor means what it says**

  `F_SETFL` sets a bit in `nonblock_mask` that only `sendmsg`/`recvmsg` read;
  `read` and `write` always use the blocking calls. `pipe2` refuses
  `O_NONBLOCK` outright. glib's wakeup is a pipe read one byte at a time
  "until it is empty", so a blocking read means the main loop stops for good
  *holding the context lock* — which is a program that hangs rather than one
  that fails.

  `__quark_read` uses `SYS_FD_READ_NB` (66) when the bit is set and turns
  `WOULD_BLOCK` into `-EAGAIN`; `__quark_write` does the same through a new
  `SYS_FD_WRITE_NB`; `__quark_pipe` sets the bit for both ends instead of
  refusing.

- [x] **Step 4: `poll` with no descriptors**

  A loop whose sources are all timeouts polls nothing at all for a while.
  `SYS_POLL` returns at once when `nfds == 0` instead of sleeping.

- [x] **Step 5: `eventfd`**

  A counter with a descriptor: `eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK)` is the
  first thing glib reaches for, and libwayland's and GTK's loops use one too.
  `src/eventfd.rs` beside `timerfd.rs`: a `u64` counter, readable when it is
  not zero, `EFD_SEMAPHORE` subtracting one rather than all. `FdKind::Event`,
  `SYS_EVENT_CREATE`, `pollset` readiness, `pipe::release_fd`/`retain_fd`.

- [x] **Step 6: run the tests, bump the ABI, commit**

  `polltest: 0 failed`. `docs/abi.md` gets the new calls and the version 3.0
  row; `tools/check-abi.sh` passes.

---

### Task 3: glib

**Files:**
- Create: `../explosion/toolchain/build-pcre2.sh`, `build-glib.sh`
- Modify: `../explosion/toolchain/teach-config-sub.sh` (two `config.sub`
  vintages)
- Create: `../explosion/toolchain/tests/glibtest.c`
- Modify: `../explosion/Makefile` (a bigger root)

- [x] **Step 1: PCRE2**, which `GRegex` is and which glib's build will not
  start without. autotools; `config.sub` needs the quark line, and pcre2's
  `config.sub` ends its OS list differently from the one libpng and expat ship,
  so `teach-config-sub.sh` learns both shapes.

- [x] **Step 2: glib**, meson, static, with the parts Quark has no equivalent
  of turned off (`selinux`, `libmount`, `xattr`, `systemtap`, `sysprof`,
  `dtrace`, `introspection`, `nls`, `libelf`, `man-pages`, `documentation`,
  `tests`). The cross file gets the answers meson cannot get by running a
  program: musl's `*printf` are C99 and Unix98; a Quark stack does not grow;
  `va_list` is copyable; there is no `/proc/self/cmdline`.

- [x] **Step 3: `tests/glibtest.c`** — a hash table, a string, split and join,
  a GObject with a property and a signal, a GRegex, a main loop that runs
  timeouts, a thread that wakes it, a thread pool, and a file through GLib and
  then through GIO.

- [x] **Step 4: room for it.** A program that links glib statically is four
  megabytes; the 64 MiB root fills up. 128 MiB, and the staging strip is what
  keeps that from being 250.

- [x] **Step 5: boot, check `glibtest: 0 failed`, commit**

---

### Task 4: harfbuzz

**Files:**
- Create: `../explosion/toolchain/build-harfbuzz.sh`
- Create: `../explosion/toolchain/tests/hbtest.c`

- [ ] **Step 1:** meson, `-Dtests=disabled -Ddocs=disabled -Dintrospection=disabled
  -Dcairo=enabled -Dfreetype=enabled -Dglib=enabled -Dicu=disabled`, C++ from
  Task 1.
- [ ] **Step 2:** `hbtest.c` shapes a string of Latin text with the DejaVu Sans
  on the image and checks the glyph count, the cluster map and the advance
  widths against what the same harfbuzz says on the host.
- [ ] **Step 3:** boot, check, commit.

---

### Task 5: fribidi and pango

**Files:**
- Create: `../explosion/toolchain/build-fribidi.sh`, `build-pango.sh`
- Create: `../explosion/toolchain/tests/pangotest.c`

- [ ] **Step 1: fribidi**, which pango requires: meson, `-Ddocs=false
  -Dbin=false -Dtests=false`.
- [ ] **Step 2: pango**, meson, with the cairo, freetype and fontconfig
  backends and no introspection.
- [ ] **Step 3: `tests/pangotest.c`** — a `PangoLayout` in a cairo image
  surface, in a font found through fontconfig, with a checksum of the result
  and the layout's reported extents.
- [ ] **Step 4:** boot, check, commit. A screenshot of text drawn by pango in a
  window belongs in the phase's evidence.

---

### Task 6: the rest of what a toolkit stands on

**Files:**
- Create: `build-graphene.sh`, `build-gdk-pixbuf.sh`, `build-epoxy.sh`
- Modify: `bootstrap-wayland.sh` (install into the prefix, `libwayland-egl`
  included)

- [ ] **Step 1: wayland installed rather than used in place.** GTK finds
  `wayland-client`, `wayland-cursor`, `wayland-egl` and `wayland-scanner`
  through pkg-config, and nothing is installed into the musl prefix today —
  every client so far named the build directory. `ninja install`, and the
  clients' build scripts use the prefix.
- [ ] **Step 2: graphene** (meson, `-Dgtk_doc=false -Dtests=false
  -Dinstalled_tests=false`), a vector maths library with SSE paths.
- [ ] **Step 3: gdk-pixbuf** (meson, `-Dtests=false -Dman=false
  -Dintrospection=disabled -Dpng=enabled -Djpeg=disabled -Dtiff=disabled`),
  which needs the loaders built in rather than as modules: there is no dlopen.
- [ ] **Step 4: libepoxy** with `-Degl=no -Dglx=no -Dx11=false`. GTK links it
  whatever renderer it uses.
- [ ] **Step 5:** a test per library that links and runs, and a commit.

---

### Task 7: GTK 4, with the cairo renderer

**Files:**
- Create: `../explosion/toolchain/build-gtk.sh`
- Modify: `../explosion/Makefile` (stage GTK's data)

- [ ] **Step 1: configure it.** `-Dbuild-demos=false -Dbuild-examples=false
  -Dbuild-tests=false -Dbuild-testsuite=false -Dintrospection=disabled
  -Dvulkan=disabled -Dmedia-gstreamer=disabled -Dprint-cups=disabled
  -Dwayland-backend=true -Dx11-backend=false -Dmacos-backend=false
  -Dwin32-backend=false`.
- [ ] **Step 2: what it needs at runtime** — `GSK_RENDERER=cairo` and
  `GDK_DEBUG=gl-disable`, GTK's own resources (compiled into the library), and
  the settings it reads. A GTK program that cannot reach a settings portal
  falls back to defaults; check that it does rather than waiting for one.
- [ ] **Step 3: the first window.** `gtk4-demo` is not built here (no demos),
  so the application is a program from GTK's own examples built against the
  installed library — `examples/hello-world.c` from the GTK documentation,
  which is somebody else's program against somebody else's toolkit.
- [ ] **Step 4: boot it under `wm`, screenshot the window, commit.**

---

### Task 8: the phase's acceptance

- [ ] **Step 1:** `dtest` and `runtests` green on ext2 and ext4.
- [ ] **Step 2:** `tools/check-rootfs.sh` clean after both.
- [ ] **Step 3:** a screenshot of a GTK window on Quark.
- [ ] **Step 4:** `CLAUDE.md` gains what a port of this size taught, and
  `../../ROADMAP.md` gets Phase 16's "what it took".
- [ ] **Step 5:** commit and push.
