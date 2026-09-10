# Phase 10 — Descriptors, Streams and Waiting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Quark the four Unix primitives it never grew — a bidirectional
stream, descriptor passing, memory addressable as a descriptor, and a way to
wait on several descriptors — plus an environment, so that Phase 8 can run
unmodified libwayland.

**Architecture:** Everything lands in the existing per-task descriptor table.
A socketpair is two kernel pipes and a descriptor FIFO per direction, so the
byte plumbing and its blocking are reused rather than rewritten. A memfd is an
existing `shmem` region with a descriptor face, and *passing* one across a
stream is what adds the receiver to that region's access mask — the capability
transfer and the descriptor transfer are the same act. Waiting is an epoll-like
set object which is itself a descriptor, with a one-shot `poll` beside it
because libwayland calls `poll()` and building a set per frame is wasteful.

**Tech Stack:** Rust (`x86_64-unknown-none`, no_std) for the kernel and
`quark-rt`; C11 freestanding for `user/libc` and `user/linux-abi`. No test
framework — verification is a `no_std` program that asserts and exits with a
code, booted in QEMU with serial captured to a file.

**Spec:** `docs/wayland.md` (the "What Wayland needs, and where it comes from"
table) and `../../ROADMAP.md` Phase 10.

## Global Constraints

- **Syscall numbers are declared twice** — `src/syscall.rs` and
  `user/quark-rt/src/syscall.rs` — and `tools/check-abi.sh` fails the build if
  they differ. Every new number goes in both files in the same commit.
- **Free number ranges:** file descriptors `71–79`, shared memory `53–63`,
  memory `42–47`. Do not use numbers outside a subsystem's block.
- **`u64::MAX` means failure** and no call returns it as a success value.
- **User pointers are validated with `validate_user_ptr{,_mut}`**, never a
  range check — the kernel runs on the caller's CR3 and an in-range but
  unmapped address faults inside the kernel.
- **`UserAccess` guards must not span a block or a yield.** Copy into a kernel
  buffer first.
- **User mappings live at or above `paging::USER_MIN_ADDR`.** Validate with
  `paging::user_range_ok`.
- **`MAX_TASKS` is 64**, and `access`/`mapped` masks are `u64` bitmaps that
  depend on it.
- Build with `make` in `quark/`; boot with `make hd && make run` in
  `../explosion`. Only `make run` passes `-serial stdio`.
- Commit messages end with `Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>`
  and **no session link**.

---

## File Structure

**Kernel — created:**

| File | Responsibility |
|---|---|
| `src/stream.rs` | Socketpair objects: two pipe handles plus a descriptor FIFO per direction, and the end refcounts. |
| `src/pollset.rs` | The epoll-like set: watch lists, readiness evaluation, and the waiter that blocks on one. |

**Kernel — modified:**

| File | Change |
|---|---|
| `src/task.rs` | `MAX_FDS` 8 → 32; `FdKind` gains `StreamEnd`, `MemFd`, `PollSet`. |
| `src/shmem.rs` | `MAX_SHMEM` 32 → 256; `MAX_PAGES_PER_REGION` 1024 → 4096; a region becomes a list of contiguous runs. |
| `src/pipe.rs` | Readiness predicates, and a small list of pollsets to wake on state change. |
| `src/syscall.rs` | Ten new numbers and their dispatch arms; `ABI_VERSION_MINOR` 5 → 6. |
| `src/scheduler.rs` | Descriptor release on task teardown covers the new kinds. |

**User space — created:**

| File | Responsibility |
|---|---|
| `user/dtest/` | The Phase 10 acceptance program. Grows one section per task. |
| `user/libc/src/env.c` | `getenv`, `setenv`, `unsetenv`, `environ` for Quark's own C library. |

**User space — modified:**

| File | Change |
|---|---|
| `user/quark-rt/src/syscall.rs` | The same ten numbers, and a wrapper apiece. |
| `user/quark-rt/src/args.rs` | Read the environment section of the args page. |
| `user/quark-rt/src/spawn.rs` | Write it. |
| `user/linux-abi/src/start.c` | Build a real `envp` array instead of an empty one. |
| `user/linux-abi/src/syscall.c` | `memfd_create`, `socketpair`, `sendmsg`, `recvmsg`, `poll`, `epoll_*`. |
| `docs/abi.md` | The new calls, and the version. |
| `Makefile` | Register `user/dtest`. |

**Why `src/stream.rs` rather than growing `src/pipe.rs`:** `pipe.rs` is 379
lines and owns one thing — a byte buffer with waiters. A stream is a *pair* of
those plus descriptor transfer, and putting both in one file would mean the
half that Wayland depends on and the half every shell pipeline depends on
share a lock and a review.

---

### Task 1: The test harness, and a descriptor you can close

There is no `SYS_FD_CLOSE` in the ABI today — a descriptor can be installed and
never released. Everything later in this plan allocates descriptors, so this
comes first, and it brings the harness the rest of the plan tests through.

**Files:**
- Create: `user/dtest/Cargo.toml`, `user/dtest/src/main.rs`
- Modify: `src/syscall.rs` (number + dispatch), `src/pipe.rs` (release helper),
  `user/quark-rt/src/syscall.rs` (number + wrapper), `Makefile`

**Interfaces:**
- Consumes: nothing.
- Produces: `SYS_FD_CLOSE: u64 = 71`;
  `quark_rt::syscall::sys_fd_close(fd: usize) -> Result<(), ()>`;
  `dtest`'s `check(what: &str, ok: bool)` harness, used by every later task.

- [ ] **Step 1: Write the failing test**

A new user program needs **three** files before its source, not one. Miss the
config and it links at its default base rather than Quark's, and the kernel
refuses the image with "failed to load ELF"; miss the symlink and the linker
cannot find the script the config names.

```bash
mkdir -p user/dtest/.cargo
ln -s ../linker.ld user/dtest/linker.ld
```

Create `user/dtest/.cargo/config.toml`:

```toml
[build]
target = "x86_64-unknown-none"

[target.x86_64-unknown-none]
rustflags = ["-C", "link-arg=-Tlinker.ld", "-C", "relocation-model=static", "-C", "code-model=large"]
```

Create `user/dtest/Cargo.toml`:

```toml
[package]
name = "dtest"
version = "0.1.0"
edition = "2021"

[dependencies]
quark-rt = { path = "../quark-rt" }

[profile.dev]
panic = "abort"

[profile.release]
panic = "abort"
```

Create `user/dtest/src/main.rs`:

```rust
#![no_std]
#![no_main]

//! Phase 10 acceptance: descriptors, streams, waiting and the environment.
//!
//! There is no test framework here, so this is one: a program that asserts and
//! exits non-zero. Run it from the shell, or read its output on the serial
//! line. Each section corresponds to one task of the Phase 10 plan.

use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall};

quark_rt::manifest!([CapReq::task_mgmt(0), CapReq::phys_alloc(64)]);

static mut PASSED: u32 = 0;
static mut FAILED: u32 = 0;

fn check(what: &str, ok: bool) {
    unsafe {
        if ok {
            PASSED += 1;
            println!("  ok    {}", what);
        } else {
            FAILED += 1;
            println!("  FAIL  {}", what);
        }
    }
}

/// A pipe wired to two of our own descriptors, for tests that need a
/// descriptor that behaves like something.
fn own_pipe(read_fd: usize, write_fd: usize) -> Result<(), ()> {
    let me = syscall::sys_getpid() as usize;
    let h = syscall::sys_pipe_create()?;
    syscall::sys_pipe_fd_set(me, read_fd, h, false)?;
    syscall::sys_pipe_fd_set(me, write_fd, h, true)?;
    Ok(())
}

fn test_close() {
    println!("close:");
    if own_pipe(3, 4).is_err() {
        check("pipe wired to fd 3 and 4", false);
        return;
    }
    check("pipe wired to fd 3 and 4", true);

    // Both return a count, or u64::MAX; there is no Result on this path.
    let mut buf = [0u8; 8];
    check("write to the write end", syscall::sys_fd_write(4, b"hi") == 2);
    check(
        "read gets the bytes back",
        syscall::sys_fd_read(3, &mut buf) == 2 && &buf[..2] == b"hi",
    );

    // Closing the last writer is what turns a read into EOF. Without a close
    // call there is no way to say so.
    check("close the write end", syscall::sys_fd_close(4).is_ok());
    check("read now reports EOF", syscall::sys_fd_read(3, &mut buf) == 0);
    check("close the read end", syscall::sys_fd_close(3).is_ok());
    check(
        "closing an empty descriptor fails",
        syscall::sys_fd_close(3).is_err(),
    );
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[dtest] Phase 10 acceptance");
    test_close();

    unsafe {
        println!("[dtest] {} passed, {} failed", PASSED, FAILED);
        syscall::sys_exit_code(if FAILED == 0 { 0 } else { 1 });
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dtest] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
```

Register it in `Makefile` — add beside the other program variables:

```make
DTEST_DIR := user/dtest
DTEST_ELF := $(DTEST_DIR)/target/$(TARGET)/release/dtest
```

add `$(DTEST_ELF)` to the end of the `user:` target's prerequisites, add a rule
beside the others:

```make
$(DTEST_ELF): FORCE
	cd $(DTEST_DIR) && cargo build --release
```

and add `dtest:DTEST` to `USR_PROGRAMS`.

- [ ] **Step 2: Run it to verify it fails**

Run: `make 2>&1 | tail -20`
Expected: FAIL — `` cannot find function `sys_fd_close` in module `syscall` ``.

- [ ] **Step 3: Write the minimal implementation**

In `src/syscall.rs`, beside the other file-descriptor numbers:

```rust
pub const SYS_FD_CLOSE: u64 = 71;
```

and a dispatch arm beside `SYS_PIPE_FD_SET`:

```rust
SYS_FD_CLOSE => {
    // Releasing a descriptor is releasing whatever it refers to: a pipe
    // loses a reader or a writer, and a reader reaching zero is what turns
    // the peer's next read into EOF. Nothing here needs a capability — a
    // task may always drop its own.
    let fd = arg0 as usize;
    if fd >= crate::task::MAX_FDS {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    // `get_task_mut` is an unsafe fn: it hands out a `&'static mut` into the
    // task table, so every use is inside an unsafe block.
    let kind = unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => core::mem::replace(&mut t.fds[fd], crate::task::FdKind::Empty),
            None => return u64::MAX,
        }
    };
    if kind.is_empty() {
        return u64::MAX;
    }
    crate::pipe::release_fd(&kind);
    0
}
```

In `src/pipe.rs`, factor the per-descriptor release that `cleanup_task_fds`
already performs so a single descriptor can use it:

```rust
/// Drop one descriptor's reference to whatever it names.
///
/// `cleanup_task_fds` does this for a whole table when a task dies; a task
/// closing one descriptor needs exactly the same work for one entry.
pub fn release_fd(kind: &FdKind) {
    match kind {
        FdKind::PipeRead(h) => drop_ref(*h, false),
        FdKind::PipeWrite(h) => drop_ref(*h, true),
        _ => {}
    }
}
```

Add its mirror in the same commit, because `SYS_FD_DUP` already has a per-kind
refcount bump written inline (`src/syscall.rs:1093`) and every kind this plan
adds has to appear there too. One function means one place:

```rust
/// Take a reference on whatever a descriptor names, for a copy of it.
///
/// The mirror of `release_fd`, and deliberately beside it: `SYS_FD_DUP` and
/// `SYS_PIPE_FD_SET` both make a second descriptor for one object, and a kind
/// added to one of these and not the other leaks or double-frees.
pub fn retain_fd(kind: &FdKind) -> Result<(), ()> {
    match kind {
        FdKind::PipeRead(h) => add_ref(*h, false),
        FdKind::PipeWrite(h) => add_ref(*h, true),
        _ => Ok(()),
    }
}
```

and replace the inline `match kind { FdKind::PipeRead(..) => ... }` in
`SYS_FD_DUP` with
`if crate::pipe::retain_fd(&kind).is_err() { return u64::MAX; }`.

`drop_ref` is currently private (`fn drop_ref`, line 346). Make it `pub fn` —
`src/stream.rs` calls it in Task 5.

Then rewrite `cleanup_task_fds` to call `release_fd`:

```rust
pub fn cleanup_task_fds(fds: &[FdKind; MAX_FDS]) {
    for kind in fds.iter() {
        release_fd(kind);
    }
}
```

In `user/quark-rt/src/syscall.rs`, the number beside `SYS_PIPE_FD_SET`:

```rust
pub const SYS_FD_CLOSE: u64 = 71;
```

and the wrapper beside `sys_pipe_fd_set`:

```rust
/// Release a descriptor. The last reader or writer of a pipe closing is what
/// makes the other end see end-of-file.
pub fn sys_fd_close(fd: usize) -> Result<(), ()> {
    let ret = unsafe { syscall1(SYS_FD_CLOSE, fd as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}
```

- [ ] **Step 4: Run it to verify it passes**

```bash
cd /home/nrupard/src/osdev/quark && make 2>&1 | tail -5
cd ../explosion && make hd 2>&1 | tail -3
```

Then boot and run `dtest` from the shell, capturing serial:

```bash
cd /home/nrupard/src/osdev/explosion
qemu-system-x86_64 $(test -w /dev/kvm && echo -enable-kvm) -cpu max \
  -L ../bang/firmware-redist/ovmf/ -pflash ../bang/firmware-redist/ovmf/OVMF_CODE.fd \
  -pflash ../bang/firmware-redist/ovmf/OVMF_VARS.fd \
  -hda hdimage.bin -display none \
  -qmp unix:/tmp/qmp.sock,server,nowait -serial file:/tmp/dtest.serial &
```

Drive it with the QMP key-sender (type `root`, then `dtest`), then:

```bash
grep -E "ok |FAIL|passed" /tmp/dtest.serial
```

Expected: eight `ok` lines and `0 failed`.

- [ ] **Step 5: Commit**

```bash
git add src/syscall.rs src/pipe.rs user/quark-rt/src/syscall.rs user/dtest Makefile
git commit -m "$(cat <<'EOF'
Let a task close a descriptor

There was no way to. A descriptor could be installed and never released, which
means the last writer of a pipe could never go away and a reader could never
see end-of-file — the one thing a pipe is for.

`retain_fd` arrives with `release_fd` as its mirror. sys_fd_dup had the
per-kind refcount bump written inline, so every new kind of descriptor had two
places to be remembered in — and one of them was easy to miss, which leaks or
double-frees depending which.

Brings `user/dtest` with it, which is this repo's nearest thing to a test
framework: a program that asserts and exits non-zero. Phase 10 adds a section
to it per piece.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: A descriptor table worth the name

**Files:**
- Modify: `src/task.rs:12`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: `sys_fd_close`, `check`, `own_pipe` from Task 1.
- Produces: `task::MAX_FDS == 32`.

Eight descriptors is three spoken for plus five. A Wayland client holds its
compositor connection and a memory object per pool; a terminal adds a clipboard
pipe. `MAX_FDS` appears in `src/task.rs`, `src/pipe.rs`, `src/scheduler.rs`
— all of them read the constant, so only the constant changes.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, and call it from `_start` after
`test_close()`:

```rust
fn test_fd_table() {
    println!("descriptor table:");
    // Eight pipes is the per-task limit, which gives sixteen ends — enough to
    // prove the table is deeper than the eight entries it used to have.
    let mut wired = 0;
    for i in 0..8 {
        let r = 3 + i * 2;
        let w = 4 + i * 2;
        if r >= 32 || w >= 32 || own_pipe(r, w).is_err() {
            break;
        }
        wired += 1;
    }
    check("wired eight pipes into sixteen descriptors", wired == 8);

    // The highest of them must actually work, not merely be accepted.
    let mut buf = [0u8; 8];
    check("write to fd 18", syscall::sys_fd_write(18, b"deep") == 4);
    check(
        "read from fd 17",
        syscall::sys_fd_read(17, &mut buf) == 4 && &buf[..4] == b"deep",
    );

    for i in 0..wired {
        let _ = syscall::sys_fd_close(3 + i * 2);
        let _ = syscall::sys_fd_close(4 + i * 2);
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Build, boot, run `dtest`.
Expected: `FAIL  wired eight pipes into sixteen descriptors` — the fifth pipe's
write end is fd 12, past the old limit of 8.

- [ ] **Step 3: Write the minimal implementation**

In `src/task.rs`:

```rust
/// Descriptors per task.
///
/// Eight was three spoken for and five left, which is not enough for a program
/// holding a display-server connection, a memory object per buffer pool and a
/// pipe or two. Thirty-two costs `MAX_TASKS * 32 * size_of::<FdKind>()` of
/// kernel memory — about fifty kilobytes for the whole system — and is spent
/// whether or not it is used, because the table is inline in the task.
pub const MAX_FDS: usize = 32;
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: three more `ok` lines, `0 failed`.
Also confirm nothing regressed: run `ls /usr/bin` and `seq 1 5 | wc -l` from
the shell — both use descriptors and pipes.

- [ ] **Step 5: Commit**

```bash
git add src/task.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Thirty-two descriptors, not eight

Three are spoken for, so eight left five. A Wayland client holds a compositor
connection and a memory object per buffer pool before it has done anything, and
a terminal adds a clipboard pipe on top.

The table is inline in the task, so this is about fifty kilobytes across the
whole system, spent whether or not it is used.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: Shared regions become lists of runs

**Files:**
- Modify: `src/shmem.rs`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: `check` from Task 1.
- Produces: `shmem::MAX_SHMEM == 256`, `shmem::MAX_PAGES_PER_REGION == 4096`;
  a region backed by up to `MAX_RUNS` contiguous runs rather than one.

Today a region is one contiguous physical run, and `MAX_PAGES_PER_REGION` is
1024 — a 1280×800 buffer is exactly 1000 pages. Raising the ceiling alone makes
a promise the allocator cannot keep, because a 4096-page contiguous run on a
128 MiB machine will fail once memory fragments. A run list is what makes the
ceiling real.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
const SHM_AT: usize = 0x94_0000_0000;

fn test_big_region() {
    println!("shared memory:");
    // 2000 pages is two 1280x800 buffers: the case that could not be
    // expressed when a region was capped at 1024 pages.
    let handle = match syscall::sys_shmem_create(2000) {
        Ok(h) => h,
        Err(()) => {
            check("create a 2000-page region", false);
            return;
        }
    };
    check("create a 2000-page region", true);
    check("map it", syscall::sys_shmem_map(handle, SHM_AT).is_ok());

    // Write the page number into the first word of every page and read it
    // back. A run list that stitches its runs together wrongly shows up here
    // and nowhere else.
    let mut good = true;
    for p in 0..2000usize {
        let at = (SHM_AT + p * 4096) as *mut u64;
        unsafe { core::ptr::write_volatile(at, p as u64 ^ 0x5A5A_0000) };
    }
    for p in 0..2000usize {
        let at = (SHM_AT + p * 4096) as *const u64;
        if unsafe { core::ptr::read_volatile(at) } != p as u64 ^ 0x5A5A_0000 {
            good = false;
            break;
        }
    }
    check("every one of its 2000 pages is distinct and readable", good);

    check("unmap", syscall::sys_shmem_unmap(handle, SHM_AT).is_ok());
    check("destroy", syscall::sys_shmem_destroy(handle).is_ok());
}
```

- [ ] **Step 2: Run it to verify it fails**

Build, boot, run `dtest`.
Expected: `FAIL  create a 2000-page region` — `create` rejects anything over
`MAX_PAGES_PER_REGION`, which is 1024.

- [ ] **Step 3: Write the minimal implementation**

In `src/shmem.rs`, replace the two constants and the region's single `base`:

```rust
/// Regions in the system.
///
/// Thirty-two was half a region per task. Linux's System V limit is 4096 and
/// its POSIX shared memory has no count at all; macOS's 32 is a legacy knob
/// nothing modern uses. This is a fixed array like everything else in this
/// kernel, so the number is what it costs: 256 regions is about sixteen
/// kilobytes.
const MAX_SHMEM: usize = 256;

/// Pages one region may hold. A 1920x1080 buffer is 2025 pages, so this is two
/// of them with room.
const MAX_PAGES_PER_REGION: usize = 4096;

/// Contiguous runs one region may be assembled from.
///
/// A region used to be a single run, which made the page limit a promise the
/// allocator could not keep: 4096 contiguous pages is sixteen megabytes in one
/// piece, and memory fragments. Sixteen runs covers any real allocation, and
/// costs 256 bytes per region rather than the eight kilobytes a per-frame
/// array would.
const MAX_RUNS: usize = 16;

#[derive(Clone, Copy)]
struct Run {
    base: usize,
    pages: usize,
}
```

The region gains the runs and loses `base`:

```rust
struct ShmemRegion {
    in_use: bool,
    runs: [Run; MAX_RUNS],
    run_count: usize,
    page_count: usize,
    creator: usize,
    access: u64,
    mapped: u64,
    pending_destroy: bool,
}
```

with `empty()` updated to `runs: [Run { base: 0, pages: 0 }; MAX_RUNS],
run_count: 0,`.

Add the lookup every consumer needs:

```rust
impl ShmemRegion {
    /// Physical address of the region's `index`-th page.
    ///
    /// Linear over the runs rather than a division, because the runs are
    /// unequal. Sixteen of them at most, and every caller walks the region in
    /// order anyway.
    fn frame_at(&self, index: usize) -> Option<usize> {
        let mut seen = 0;
        for r in &self.runs[..self.run_count] {
            if index < seen + r.pages {
                return Some(r.base + (index - seen) * 4096);
            }
            seen += r.pages;
        }
        None
    }
}
```

`release` frees every run:

```rust
unsafe fn release(region: &mut ShmemRegion) {
    let mut freed = 0;
    for r in &region.runs[..region.run_count] {
        for j in 0..r.pages {
            pmm::free(pmm::PhysFrame::from_address(r.base + j * 4096));
            freed += 1;
        }
    }
    scheduler::uncharge_task_mem(region.creator, freed);
    *region = ShmemRegion::empty();
}
```

`create` asks for the largest run it can and halves on failure:

```rust
    // Take the biggest contiguous runs the allocator will give, halving the
    // request each time it refuses. A fresh machine satisfies this in one run;
    // a fragmented one in several, which is the entire point.
    let mut want = pages;
    let mut got = 0usize;
    let mut runs = [Run { base: 0, pages: 0 }; MAX_RUNS];
    let mut run_count = 0usize;
    while got < pages {
        if run_count == MAX_RUNS || want == 0 {
            break;
        }
        let ask = want.min(pages - got);
        match pmm::alloc_contiguous(ask) {
            Some(frame) => {
                runs[run_count] = Run { base: frame.address(), pages: ask };
                run_count += 1;
                got += ask;
            }
            None => want /= 2,
        }
    }
    if got < pages {
        let flags = irq_save();
        unsafe {
            let region = &mut regions()[handle];
            region.runs = runs;
            region.run_count = run_count;
            release(region);
        }
        irq_restore(flags);
        return u64::MAX;
    }
    // Zero every run (identity-mapped) so nothing leaks from a previous owner.
    for r in &runs[..run_count] {
        unsafe { core::ptr::write_bytes(r.base as *mut u8, 0, r.pages * 4096) };
    }
    let flags = irq_save();
    unsafe {
        let region = &mut regions()[handle];
        region.runs = runs;
        region.run_count = run_count;
    }
    irq_restore(flags);
```

`map` walks by page rather than by offset from a base:

```rust
        for i in 0..page_count {
            let v = vaddr + i * 4096;
            let phys = match region.frame_at(i) {
                Some(p) => p,
                None => {
                    for j in 0..i {
                        let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                    }
                    irq_restore(flags);
                    return u64::MAX;
                }
            };
            if paging::map_page(cr3, v, phys, pte_flags).is_err() {
                for j in 0..i {
                    let _ = paging::unmap_page(cr3, vaddr + j * 4096);
                }
                irq_restore(flags);
                return u64::MAX;
            }
        }
```

Then `grep -n 'region.base\|\.base +' src/shmem.rs` and convert every remaining
use to `frame_at`.

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: five more `ok` lines.
Then confirm nothing regressed in the existing users of shmem — the VFS's
bulk directory read and the compositor's windows:

```bash
# from the shell
ls /usr/bin
wm wmdemo      # Escape to exit
```

Both must behave as before.

- [ ] **Step 5: Commit**

```bash
git add src/shmem.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
A shared region is a list of runs, not one of them

The page limit was 1024 and a 1280x800 window is exactly 1000 pages, so a
client that wanted two buffers could not have them. Raising the limit alone
would have been a promise the allocator cannot keep: 4096 pages is sixteen
megabytes in one piece, on a machine with a hundred and twenty-eight.

So a region is assembled from up to sixteen contiguous runs, asking for the
largest the allocator will give and halving on refusal. A fresh machine still
satisfies it in one run. Sixteen runs is 256 bytes per region, against the
eight kilobytes an array of frame addresses would have cost — which is why the
original chose one run.

Also raises the region count from 32, which was half a region per task. Linux
allows 4096 System V segments and no limit at all on POSIX shared memory;
macOS's 32 is a legacy knob nothing uses.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Memory with a descriptor face

**Files:**
- Create: none
- Modify: `src/task.rs` (`FdKind::MemFd`), `src/shmem.rs` (expose
  `region_pages`, `add_access`), `src/syscall.rs`, `src/pipe.rs`
  (`release_fd`), `user/quark-rt/src/syscall.rs`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Task 3's region changes; Task 1's `release_fd`.
- Produces: `FdKind::MemFd { handle: usize }`;
  `SYS_MEMFD_CREATE: u64 = 53`, `SYS_MMAP_FD: u64 = 42`;
  `sys_memfd_create(pages: usize) -> Result<usize, ()>` returning a descriptor,
  and `sys_mmap_fd(fd: usize, vaddr: usize) -> Result<(), ()>`;
  `shmem::pages_of(handle) -> Option<usize>` and
  `shmem::add_access(handle, tid)`.

`wl_shm` is a client creating memory, mapping it, and passing the descriptor.
Quark has the region and the mapping already; what is missing is that a handle
is not a descriptor, so it cannot be passed, inherited, or closed.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
const MEMFD_AT: usize = 0x95_0000_0000;

fn test_memfd() {
    println!("memory as a descriptor:");
    let fd = match syscall::sys_memfd_create(4) {
        Ok(f) => f,
        Err(()) => {
            check("create a four-page memory descriptor", false);
            return;
        }
    };
    check("create a four-page memory descriptor", fd >= 3);
    check("map it", syscall::sys_mmap_fd(fd, MEMFD_AT).is_ok());

    unsafe { core::ptr::write_volatile(MEMFD_AT as *mut u64, 0xFEED_FACE) };
    check(
        "what was written is there",
        unsafe { core::ptr::read_volatile(MEMFD_AT as *const u64) } == 0xFEED_FACE,
    );

    check("close it", syscall::sys_fd_close(fd).is_ok());
    check(
        "mapping a closed descriptor fails",
        syscall::sys_mmap_fd(fd, MEMFD_AT + 0x10000).is_err(),
    );
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `sys_memfd_create` ``.

- [ ] **Step 3: Write the minimal implementation**

`src/task.rs`, in `FdKind`:

```rust
    /// A shared-memory region, named by a descriptor so that it can be passed
    /// across a stream and closed like anything else.
    MemFd { handle: usize },
```

`src/shmem.rs`, two accessors the descriptor layer needs:

```rust
/// How many pages a region holds, or `None` if the handle is not live.
pub fn pages_of(handle: usize) -> Option<usize> {
    if handle >= MAX_SHMEM {
        return None;
    }
    let flags = irq_save();
    let r = unsafe { &regions()[handle] };
    let out = if r.in_use && !r.pending_destroy { Some(r.page_count) } else { None };
    irq_restore(flags);
    out
}

/// Admit `tid` to a region, with no check on the caller.
///
/// This is not `grant`: it is called only when a descriptor for the region is
/// *received* over a stream, so the receiver asked for it and the sender chose
/// to send it. Both halves consented, which is more than `grant` requires.
pub fn add_access(handle: usize, tid: usize) -> bool {
    if handle >= MAX_SHMEM || tid >= MAX_TASKS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let r = &mut regions()[handle];
        if r.in_use && !r.pending_destroy {
            r.access |= 1u64 << tid;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    ok
}

/// Drop a descriptor's reference. The region goes when its last one does.
pub fn close_ref(handle: usize) {
    let tid = scheduler::current_tid();
    if handle >= MAX_SHMEM {
        return;
    }
    let flags = irq_save();
    unsafe {
        let r = &mut regions()[handle];
        if r.in_use {
            r.access &= !(1u64 << tid);
            if r.access == 0 && r.mapped == 0 {
                release(r);
            } else if r.access == 0 {
                r.pending_destroy = true;
            }
        }
    }
    irq_restore(flags);
}
```

`src/syscall.rs`, the numbers and arms:

```rust
pub const SYS_MMAP_FD: u64 = 42;
pub const SYS_MEMFD_CREATE: u64 = 53;
```

```rust
SYS_MEMFD_CREATE => {
    // Memory a program can name and hand over. The region is exactly what
    // SYS_SHMEM_CREATE makes; the descriptor is what lets it travel.
    let pages = arg0 as usize;
    let handle = crate::shmem::create(pages);
    if handle == u64::MAX {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    match scheduler::install_fd(tid, crate::task::FdKind::MemFd { handle: handle as usize }) {
        Some(fd) => fd as u64,
        None => {
            crate::shmem::close_ref(handle as usize);
            u64::MAX
        }
    }
}
SYS_MMAP_FD => {
    let fd = arg0 as usize;
    let vaddr = arg1 as usize;
    let tid = scheduler::current_tid();
    if fd >= crate::task::MAX_FDS {
        return u64::MAX;
    }
    let handle = match scheduler::get_task_mut(tid) {
        Some(t) => match t.fds[fd] {
            crate::task::FdKind::MemFd { handle } => handle,
            _ => return u64::MAX,
        },
        None => return u64::MAX,
    };
    crate::shmem::map(handle, vaddr)
}
```

`src/scheduler.rs`, the helper both this and Task 5 need:

```rust
/// Put `kind` in the lowest free descriptor at or above 3, and say which.
///
/// At or above 3 because 0, 1 and 2 are whatever a spawner wired them to, and
/// a program allocating a descriptor never means to take stdin's place.
pub fn install_fd(tid: usize, kind: crate::task::FdKind) -> Option<usize> {
    unsafe {
        let task = TASKS[tid].as_mut()?;
        for fd in 3..crate::task::MAX_FDS {
            if task.fds[fd].is_empty() {
                task.fds[fd] = kind;
                return Some(fd);
            }
        }
    }
    None
}
```

`src/pipe.rs`, extend both halves — a duplicated memory descriptor is a second
reference to the region:

```rust
    // in release_fd
        FdKind::MemFd { handle } => crate::shmem::close_ref(*handle),

    // in retain_fd
        FdKind::MemFd { handle } => {
            crate::shmem::add_access(*handle, scheduler::current_tid());
            Ok(())
        }
```

and in `SYS_FD_DUP`, admit the *target* rather than the caller, since that is
who ends up holding the copy:

```rust
            if let crate::task::FdKind::MemFd { handle } = kind {
                crate::shmem::add_access(handle, target_tid);
            }
```

`user/quark-rt/src/syscall.rs`, numbers and wrappers:

```rust
pub const SYS_MMAP_FD: u64 = 42;
pub const SYS_MEMFD_CREATE: u64 = 53;

/// Allocate `pages` of shareable memory and name it with a descriptor.
pub fn sys_memfd_create(pages: usize) -> Result<usize, ()> {
    let ret = unsafe { syscall1(SYS_MEMFD_CREATE, pages as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Map memory named by a descriptor at `vaddr`.
pub fn sys_mmap_fd(fd: usize, vaddr: usize) -> Result<(), ()> {
    let ret = unsafe { syscall2(SYS_MMAP_FD, fd as u64, vaddr as u64) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: five more `ok` lines, `0 failed`.

- [ ] **Step 5: Commit**

```bash
git add src/task.rs src/shmem.rs src/syscall.rs src/scheduler.rs src/pipe.rs \
        user/quark-rt/src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Name memory with a descriptor

Quark had the region and the mapping already. What it did not have is that a
shared-memory handle is not a descriptor, so it could not be passed to another
task, inherited across a spawn, or closed like anything else a program holds.

`wl_shm` is a client creating memory, mapping it, and handing the descriptor to
the compositor, so this is the shape that has to exist before any of that can.

`add_access` is deliberately not `grant`: it has no check on its caller, and it
is reachable only when a descriptor is *received* over a stream. The sender
chose to send and the receiver asked to take, which is a stronger claim than
`grant` makes.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: A socketpair

**Files:**
- Create: `src/stream.rs`
- Modify: `src/main.rs` (`mod stream;`), `src/task.rs`, `src/syscall.rs`,
  `src/pipe.rs`, `user/quark-rt/src/syscall.rs`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Task 1's `release_fd`, Task 4's `install_fd`.
- Produces: `FdKind::StreamEnd { stream: usize, end: u8 }`;
  `SYS_SOCKETPAIR: u64 = 72`;
  `sys_socketpair() -> Result<(usize, usize), ()>` returning two descriptors;
  `stream::readable(s, end) -> bool`, `stream::writable(s, end) -> bool` for
  Task 7.

A stream is two pipes, one per direction, so the byte buffering and its
blocking are the existing code. What is new is the pairing and the descriptor
FIFO Task 6 fills.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
fn test_socketpair() {
    println!("socketpair:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => {
            check("create a pair", false);
            return;
        }
    };
    check("create a pair", a >= 3 && b >= 3 && a != b);

    let mut buf = [0u8; 16];
    check("a writes", syscall::sys_fd_write(a, b"ping") == 4);
    check(
        "b reads what a wrote",
        syscall::sys_fd_read(b, &mut buf) == 4 && &buf[..4] == b"ping",
    );
    // The direction that a pipe cannot do.
    check("b writes", syscall::sys_fd_write(b, b"pong") == 4);
    check(
        "a reads what b wrote",
        syscall::sys_fd_read(a, &mut buf) == 4 && &buf[..4] == b"pong",
    );

    check("close a", syscall::sys_fd_close(a).is_ok());
    check("b now reads EOF", syscall::sys_fd_read(b, &mut buf) == 0);
    check("close b", syscall::sys_fd_close(b).is_ok());
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `sys_socketpair` ``.

- [ ] **Step 3: Write the minimal implementation**

Create `src/stream.rs`:

```rust
//! Connected pairs of byte streams.
//!
//! A pipe carries bytes one way. Everything that speaks a protocol — a display
//! server and its client, most obviously — needs both, and needs them paired,
//! so that closing one end is something the other can observe.
//!
//! The bytes themselves are two pipes, because a pipe is already a buffer with
//! readers, writers and tasks blocked on both. What a stream adds is the
//! pairing and, in the next commit, a queue of descriptors in flight: a
//! message can carry a handle to an object, which is the thing `SCM_RIGHTS`
//! does on Unix and the reason `wl_shm` works at all.

use crate::pipe;
use crate::task::FdKind;

const MAX_STREAMS: usize = 64;
/// Descriptors in flight in one direction.
///
/// A protocol attaches at most one descriptor to a message and the peer reads
/// messages in order, so this only has to absorb a burst.
const FD_QUEUE: usize = 8;

struct Stream {
    in_use: bool,
    creator: usize,
    /// Written by end 0, read by end 1, and the reverse.
    zero_to_one: usize,
    one_to_zero: usize,
    /// Descriptors travelling towards end 1, then towards end 0.
    q: [[FdKind; FD_QUEUE]; 2],
    q_len: [usize; 2],
    /// How many descriptors name each end.
    ///
    /// A count and not a flag, because `SYS_FD_DUP` makes a second descriptor
    /// for one end — which is exactly how a parent hands a child its side of a
    /// connection. With a flag, the parent closing its copy would tell the peer
    /// the end had gone while the child was still holding it.
    refs: [usize; 2],
}

impl Stream {
    const fn empty() -> Self {
        Stream {
            in_use: false,
            creator: 0,
            zero_to_one: 0,
            one_to_zero: 0,
            q: [[FdKind::Empty; FD_QUEUE]; 2],
            q_len: [0; 2],
            refs: [0; 2],
        }
    }
}

static mut STREAMS: [Stream; MAX_STREAMS] = {
    const S: Stream = Stream::empty();
    [S; MAX_STREAMS]
};

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

#[inline(always)]
unsafe fn streams() -> &'static mut [Stream; MAX_STREAMS] { unsafe {
    &mut *core::ptr::addr_of_mut!(STREAMS)
}}

/// The pipe an end reads from, and the one it writes to.
pub fn pipes_for(stream: usize, end: u8) -> Option<(usize, usize)> {
    if stream >= MAX_STREAMS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &streams()[stream];
        if !s.in_use {
            None
        } else if end == 0 {
            Some((s.one_to_zero, s.zero_to_one))
        } else {
            Some((s.zero_to_one, s.one_to_zero))
        }
    };
    irq_restore(flags);
    out
}

/// Make a connected pair. Returns the stream index, or `None`.
pub fn create(tid: usize) -> Option<usize> {
    // The two pipes first: if either is refused there is nothing to unwind but
    // the other, and no stream slot has been claimed.
    let a = pipe::create()?;
    let b = match pipe::create() {
        Some(b) => b,
        None => {
            pipe::drop_unreferenced(a);
            return None;
        }
    };
    // Each end holds a reader on one pipe and a writer on the other, so both
    // pipes have exactly one of each for as long as both ends live.
    let _ = pipe::add_ref(a, false);
    let _ = pipe::add_ref(a, true);
    let _ = pipe::add_ref(b, false);
    let _ = pipe::add_ref(b, true);

    let flags = irq_save();
    let idx = unsafe {
        match streams().iter().position(|s| !s.in_use) {
            Some(i) => i,
            None => {
                irq_restore(flags);
                pipe::drop_ref(a, false); pipe::drop_ref(a, true);
                pipe::drop_ref(b, false); pipe::drop_ref(b, true);
                return None;
            }
        }
    };
    unsafe {
        let s = &mut streams()[idx];
        *s = Stream::empty();
        s.in_use = true;
        s.creator = tid;
        s.zero_to_one = a;
        s.one_to_zero = b;
        s.refs = [1, 1];
    }
    irq_restore(flags);
    Some(idx)
}

/// Take a reference on an end, for a second descriptor naming it.
pub fn retain_end(stream: usize, end: u8) -> Result<(), ()> {
    if stream >= MAX_STREAMS || end > 1 {
        return Err(());
    }
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut streams()[stream];
        if s.in_use && s.refs[end as usize] > 0 {
            s.refs[end as usize] += 1;
            true
        } else {
            false
        }
    };
    irq_restore(flags);
    if ok { Ok(()) } else { Err(()) }
}

/// One descriptor naming this end has gone. The end goes with the last of them.
pub fn close_end(stream: usize, end: u8) {
    if stream >= MAX_STREAMS || end > 1 {
        return;
    }
    let flags = irq_save();
    let (drop_pipes, orphans) = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.refs[end as usize] == 0 {
            irq_restore(flags);
            return;
        }
        s.refs[end as usize] -= 1;
        if s.refs[end as usize] > 0 {
            // Somebody else still holds this end; nothing observable happens.
            irq_restore(flags);
            return;
        }
        // Anything still in flight towards the peer is released with the end
        // that would have delivered it.
        let mut orphans = [FdKind::Empty; FD_QUEUE];
        let n = s.q_len[1 - end as usize];
        orphans[..n].copy_from_slice(&s.q[1 - end as usize][..n]);
        s.q_len[1 - end as usize] = 0;
        let both_gone = s.refs[0] == 0 && s.refs[1] == 0;
        let ab = (s.zero_to_one, s.one_to_zero);
        if both_gone {
            *s = Stream::empty();
        }
        (if both_gone { Some(ab) } else { None }, (orphans, n))
    };
    irq_restore(flags);

    // Dropping the writer this end held is what gives the peer end-of-file.
    if let Some((a, b)) = drop_pipes {
        pipe::drop_ref(a, false); pipe::drop_ref(a, true);
        pipe::drop_ref(b, false); pipe::drop_ref(b, true);
    } else {
        // Only this end went: release its half of both pipes so the peer sees
        // EOF on the direction this end was writing.
        if let Some((rd, wr)) = pipes_for(stream, end) {
            pipe::drop_ref(rd, false);
            pipe::drop_ref(wr, true);
        }
    }
    let (orphans, n) = orphans;
    for kind in &orphans[..n] {
        crate::pipe::release_fd(kind);
    }
}

/// Is there something for this end to read?
pub fn readable(stream: usize, end: u8) -> bool {
    match pipes_for(stream, end) {
        Some((rd, _)) => pipe::readable(rd),
        None => false,
    }
}

/// Is there room for this end to write?
pub fn writable(stream: usize, end: u8) -> bool {
    match pipes_for(stream, end) {
        Some((_, wr)) => pipe::writable(wr),
        None => false,
    }
}
```

`src/main.rs`, beside the other module declarations:

```rust
mod stream;
```

`src/task.rs`, in `FdKind`:

```rust
    /// One end of a connected pair. `end` is 0 or 1.
    StreamEnd { stream: usize, end: u8 },
```

`src/pipe.rs` — three small additions the stream needs. `readable`/`writable`
are also what Task 7 evaluates:

```rust
/// Is a read on this pipe able to return now — either with bytes, or with the
/// end-of-file that a departed writer means?
pub fn readable(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            false
        } else {
            PIPES[handle].len > 0 || PIPES[handle].writers == 0
        }
    };
    irq_restore(flags);
    out
}

/// Is there room to write?
pub fn writable(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        if handle >= MAX_PIPES || !PIPES[handle].in_use {
            false
        } else {
            PIPES[handle].len < PIPE_BUF_SIZE
        }
    };
    irq_restore(flags);
    out
}

/// Free a pipe that was created and never wired to a descriptor.
pub fn drop_unreferenced(handle: usize) {
    let flags = irq_save();
    unsafe {
        if handle < MAX_PIPES
            && PIPES[handle].in_use
            && PIPES[handle].readers == 0
            && PIPES[handle].writers == 0
        {
            PIPES[handle] = Pipe::new();
        }
    }
    irq_restore(flags);
}
```

Make `drop_ref` and `add_ref` `pub` if they are not already, and extend both
halves:

```rust
    // in release_fd
        FdKind::StreamEnd { stream, end } => crate::stream::close_end(*stream, *end),

    // in retain_fd
        FdKind::StreamEnd { stream, end } => crate::stream::retain_end(*stream, *end),
```

This is the case that makes the reference count necessary rather than tidy:
handing a child one side of a connection is `SYS_FD_DUP` followed by the parent
closing its own copy, and with a flag the peer would be told the end had gone
while the child still held it.

`src/syscall.rs` — the number, the arm, and routing read/write:

```rust
pub const SYS_SOCKETPAIR: u64 = 72;
```

```rust
SYS_SOCKETPAIR => {
    let tid = scheduler::current_tid();
    let s = match crate::stream::create(tid) {
        Some(s) => s,
        None => return u64::MAX,
    };
    let a = scheduler::install_fd(tid, crate::task::FdKind::StreamEnd { stream: s, end: 0 });
    let b = scheduler::install_fd(tid, crate::task::FdKind::StreamEnd { stream: s, end: 1 });
    match (a, b) {
        (Some(a), Some(b)) => ((a as u64) << 32) | b as u64,
        _ => {
            if let Some(fd) = a { let _ = scheduler::clear_fd(tid, fd); }
            if let Some(fd) = b { let _ = scheduler::clear_fd(tid, fd); }
            crate::stream::close_end(s, 0);
            crate::stream::close_end(s, 1);
            u64::MAX
        }
    }
}
```

In whichever function `SYS_FD_READ` and `SYS_FD_WRITE` dispatch through, add a
`StreamEnd` case that resolves to the right pipe with
`stream::pipes_for(stream, end)` — reads take the first of the pair, writes the
second — and then calls the existing `pipe::read` / `pipe::write`.

`src/scheduler.rs`:

```rust
/// Empty one descriptor without releasing what it named. For unwinding a
/// partial install, where the caller releases the object itself.
pub fn clear_fd(tid: usize, fd: usize) -> Result<(), ()> {
    unsafe {
        let task = TASKS[tid].as_mut().ok_or(())?;
        if fd >= crate::task::MAX_FDS {
            return Err(());
        }
        task.fds[fd] = crate::task::FdKind::Empty;
        Ok(())
    }
}
```

`user/quark-rt/src/syscall.rs`:

```rust
pub const SYS_SOCKETPAIR: u64 = 72;

/// A connected pair of byte streams, both ends in this task's descriptor
/// table. Either end may be moved into another task with `sys_fd_dup`.
pub fn sys_socketpair() -> Result<(usize, usize), ()> {
    let ret = unsafe { syscall0(SYS_SOCKETPAIR) };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(((ret >> 32) as usize, (ret & 0xFFFF_FFFF) as usize))
    }
}
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: eight more `ok` lines, `0 failed`.
Also confirm pipes still work: `seq 1 12 | wc -l` must say 12.

- [ ] **Step 5: Commit**

```bash
git add src/stream.rs src/main.rs src/task.rs src/pipe.rs src/syscall.rs \
        src/scheduler.rs user/quark-rt/src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
A socketpair: two pipes that know about each other

A pipe carries bytes one way, and anything that speaks a protocol needs both
directions paired, so that one end closing is something the other can observe.

The bytes are two pipes rather than a new buffer, because a pipe is already a
ring with readers, writers and blocked tasks. What a stream adds is the pairing
and, next, a queue of descriptors in flight.

Both ends land in the caller's own table, the way socketpair(2) works; moving
one into a child is sys_fd_dup followed by closing our copy. That is why an end
is reference counted rather than flagged — with a flag, the parent closing its
copy would tell the peer the end had gone while the child was still holding
it.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: Descriptor passing

**Files:**
- Modify: `src/stream.rs`, `src/syscall.rs`, `user/quark-rt/src/syscall.rs`,
  `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Tasks 4 and 5.
- Produces: `SYS_FD_SEND: u64 = 73`, `SYS_FD_RECV: u64 = 74`;
  `sys_fd_send(fd, buf: &[u8], pass: Option<usize>) -> Result<usize, ()>`;
  `sys_fd_recv(fd, buf: &mut [u8], at: Option<usize>) -> Result<(usize, bool), ()>`.

This is `SCM_RIGHTS`. It needs **no authority over the peer**, which is the
distinction from `SYS_FD_DUP`: dup pushes a descriptor at a task that did not
ask, so it requires `TaskMgmt`; passing hands one to a task that called
`recv`, and consent on both sides is the whole authorisation.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
const PASSED_AT: usize = 0x96_0000_0000;

fn test_fd_passing() {
    println!("descriptor passing:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair to pass over", false); return; }
    };
    let mem = match syscall::sys_memfd_create(2) {
        Ok(f) => f,
        Err(()) => { check("memory to pass", false); return; }
    };
    check("a pair and some memory", true);

    // Write a witness through the sender's own mapping first.
    check("map it here", syscall::sys_mmap_fd(mem, PASSED_AT).is_ok());
    unsafe { core::ptr::write_volatile(PASSED_AT as *mut u64, 0xC0FFEE) };

    check(
        "send the descriptor with a byte",
        syscall::sys_fd_send(a, b"m", Some(mem)) == Ok(1),
    );

    let mut buf = [0u8; 4];
    let got = syscall::sys_fd_recv(b, &mut buf, Some(20));
    check("receive says a descriptor came", got == Ok((1, true)));

    // The received descriptor is a different number naming the same memory.
    check("map the received descriptor", syscall::sys_mmap_fd(20, PASSED_AT + 0x8000).is_ok());
    check(
        "it is the same memory",
        unsafe { core::ptr::read_volatile((PASSED_AT + 0x8000) as *const u64) } == 0xC0FFEE,
    );

    // Receiving when nothing was attached must not invent one.
    check("send with no descriptor", syscall::sys_fd_send(a, b"x", None) == Ok(1));
    check(
        "receive says none came",
        syscall::sys_fd_recv(b, &mut buf, Some(21)) == Ok((1, false)),
    );

    let _ = syscall::sys_fd_close(20);
    let _ = syscall::sys_fd_close(mem);
    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `sys_fd_send` ``.

- [ ] **Step 3: Write the minimal implementation**

In `src/stream.rs`:

```rust
/// Queue a descriptor for the peer of `end`. False if the queue is full.
pub fn push_fd(stream: usize, end: u8, kind: FdKind) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return false;
    }
    let to = 1 - end as usize;
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.refs[to] == 0 || s.q_len[to] == FD_QUEUE {
            false
        } else {
            s.q[to][s.q_len[to]] = kind;
            s.q_len[to] += 1;
            true
        }
    };
    irq_restore(flags);
    ok
}

/// Take the descriptor at the head of this end's queue, if any.
pub fn pop_fd(stream: usize, end: u8) -> Option<FdKind> {
    if stream >= MAX_STREAMS || end > 1 {
        return None;
    }
    let me = end as usize;
    let flags = irq_save();
    let out = unsafe {
        let s = &mut streams()[stream];
        if !s.in_use || s.q_len[me] == 0 {
            None
        } else {
            let head = s.q[me][0];
            for i in 1..s.q_len[me] {
                s.q[me][i - 1] = s.q[me][i];
            }
            s.q_len[me] -= 1;
            Some(head)
        }
    };
    irq_restore(flags);
    out
}
```

In `src/syscall.rs`:

```rust
pub const SYS_FD_SEND: u64 = 73;
pub const SYS_FD_RECV: u64 = 74;
```

```rust
SYS_FD_SEND => {
    // arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to pass or u64::MAX
    let fd = arg0 as usize;
    let len = arg2 as usize;
    let pass = arg3;
    let tid = scheduler::current_tid();
    if fd >= crate::task::MAX_FDS {
        return u64::MAX;
    }
    let (stream, end) = match scheduler::get_task_mut(tid) {
        Some(t) => match t.fds[fd] {
            crate::task::FdKind::StreamEnd { stream, end } => (stream, end),
            _ => return u64::MAX,
        },
        None => return u64::MAX,
    };

    // The descriptor goes on the queue before the bytes, so a peer that reads
    // the bytes never has to wonder whether the handle is still coming.
    if pass != u64::MAX {
        let pfd = pass as usize;
        if pfd >= crate::task::MAX_FDS {
            return u64::MAX;
        }
        let kind = match scheduler::get_task_mut(tid) {
            Some(t) => t.fds[pfd],
            None => return u64::MAX,
        };
        if kind.is_empty() {
            return u64::MAX;
        }
        if !crate::stream::push_fd(stream, end, kind) {
            return u64::MAX;
        }
    }

    let (_, wr) = match crate::stream::pipes_for(stream, end) {
        Some(p) => p,
        None => return u64::MAX,
    };
    crate::pipe::write(wr, arg1 as *const u8, len)
}
SYS_FD_RECV => {
    // arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to install at, or
    // u64::MAX to leave any attached descriptor queued.
    let fd = arg0 as usize;
    let len = arg2 as usize;
    let at = arg3;
    let tid = scheduler::current_tid();
    if fd >= crate::task::MAX_FDS {
        return u64::MAX;
    }
    let (stream, end) = match scheduler::get_task_mut(tid) {
        Some(t) => match t.fds[fd] {
            crate::task::FdKind::StreamEnd { stream, end } => (stream, end),
            _ => return u64::MAX,
        },
        None => return u64::MAX,
    };
    let (rd, _) = match crate::stream::pipes_for(stream, end) {
        Some(p) => p,
        None => return u64::MAX,
    };
    let n = crate::pipe::read(rd, arg1 as *mut u8, len);
    if n == u64::MAX {
        return u64::MAX;
    }

    let mut got = 0u64;
    if at != u64::MAX {
        let slot = at as usize;
        if slot < crate::task::MAX_FDS {
            if let Some(kind) = crate::stream::pop_fd(stream, end) {
                // Memory arriving this way admits the receiver to the region.
                // The sender chose to send and the receiver asked to take, so
                // no authority over either is needed.
                if let crate::task::FdKind::MemFd { handle } = kind {
                    crate::shmem::add_access(handle, tid);
                }
                unsafe {
                    if let Some(t) = scheduler::get_task_mut(tid) {
                        if t.fds[slot].is_empty() {
                            t.fds[slot] = kind;
                            got = 1;
                        } else {
                            crate::pipe::release_fd(&kind);
                        }
                    }
                }
            }
        }
    }
    (got << 32) | n
}
```

`user/quark-rt/src/syscall.rs`:

```rust
pub const SYS_FD_SEND: u64 = 73;
pub const SYS_FD_RECV: u64 = 74;

/// Write to a stream, optionally handing the peer one of our descriptors.
///
/// Passing needs no authority over the peer: it takes delivery by calling
/// `sys_fd_recv`. That is the difference from `sys_fd_dup`, which puts a
/// descriptor into a task that did not ask and therefore needs `TaskMgmt`.
pub fn sys_fd_send(fd: usize, buf: &[u8], pass: Option<usize>) -> Result<usize, ()> {
    let p = pass.map(|f| f as u64).unwrap_or(u64::MAX);
    let ret = unsafe {
        syscall4(SYS_FD_SEND, fd as u64, buf.as_ptr() as u64, buf.len() as u64, p)
    };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Read from a stream. If `at` is given and a descriptor was attached, it is
/// installed there. Returns the byte count and whether one arrived.
pub fn sys_fd_recv(fd: usize, buf: &mut [u8], at: Option<usize>) -> Result<(usize, bool), ()> {
    let a = at.map(|f| f as u64).unwrap_or(u64::MAX);
    let ret = unsafe {
        syscall4(SYS_FD_RECV, fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, a)
    };
    if ret == u64::MAX {
        Err(())
    } else {
        Ok(((ret & 0xFFFF_FFFF) as usize, (ret >> 32) != 0))
    }
}
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: eight more `ok` lines, `0 failed`.

- [ ] **Step 5: Commit**

```bash
git add src/stream.rs src/syscall.rs user/quark-rt/src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Pass a descriptor along with the bytes

SCM_RIGHTS, in effect. A message can carry a handle to an object, which is how
a Wayland client gives its compositor a buffer and how a clipboard transfer
gets its pipe.

It needs no authority over the peer, and that is the point rather than an
oversight. sys_fd_dup puts a descriptor into a task that never asked, so it
requires TaskMgmt over that task. Passing hands one to a task that called
recv: the sender chose to send and the receiver asked to take, and consent on
both sides is the whole authorisation.

Memory arriving this way admits the receiver to the region, which is why
shmem::add_access exists and checks nothing about its caller — it is reachable
only from here.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Waiting on a set

**Files:**
- Create: `src/pollset.rs`
- Modify: `src/main.rs`, `src/task.rs`, `src/pipe.rs`, `src/syscall.rs`,
  `user/quark-rt/src/syscall.rs`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Tasks 4, 5, 6.
- Produces: `FdKind::PollSet { set: usize }`;
  `SYS_POLLSET_CREATE: u64 = 75`, `SYS_POLLSET_CTL: u64 = 76`,
  `SYS_POLLSET_WAIT: u64 = 77`;
  `QW_READABLE: u32 = 1`, `QW_WRITABLE: u32 = 2`, `QW_HANGUP: u32 = 4`;
  `#[repr(C)] pub struct Ready { pub token: u64, pub events: u32, pub _pad: u32 }`;
  `sys_pollset_create()`, `sys_pollset_add/modify/remove`,
  `sys_pollset_wait(set_fd, out: &mut [Ready], timeout_ticks: u64)`.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
fn test_pollset() {
    println!("waiting on a set:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair to watch", false); return; }
    };
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("create a set", false); return; }
    };
    check("create a set", set >= 3);
    check("watch b for readable", syscall::sys_pollset_add(set, b, syscall::POLL_READABLE, 0xB).is_ok());
    check("watch a for writable", syscall::sys_pollset_add(set, a, syscall::POLL_WRITABLE, 0xA).is_ok());

    let mut ready = [syscall::Ready::empty(); 4];

    // `a` is writable now and `b` is not readable, so exactly one fires.
    let n = syscall::sys_pollset_wait(set, &mut ready, 50);
    check("one is ready", n == Ok(1));
    check("and it is the writable one", ready[0].token == 0xA);

    // Stop watching `a`, then nothing is ready until something is written.
    check("stop watching a", syscall::sys_pollset_remove(set, a).is_ok());
    let n = syscall::sys_pollset_wait(set, &mut ready, 5);
    check("nothing ready, and it timed out", n == Ok(0));

    check("write to a", syscall::sys_fd_write(a, b"go") == 2);
    let n = syscall::sys_pollset_wait(set, &mut ready, 50);
    check("now b is ready", n == Ok(1) && ready[0].token == 0xB);
    check("readable is what it reports", ready[0].events & syscall::POLL_READABLE != 0);

    // A closed peer is a hangup rather than a silence.
    let mut buf = [0u8; 4];
    let _ = syscall::sys_fd_read(b, &mut buf);
    check("close a", syscall::sys_fd_close(a).is_ok());
    let n = syscall::sys_pollset_wait(set, &mut ready, 50);
    check("b reports hangup", n == Ok(1) && ready[0].events & syscall::POLL_HANGUP != 0);

    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(b);
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `sys_pollset_create` ``.

- [ ] **Step 3: Write the minimal implementation**

Create `src/pollset.rs`:

```rust
//! Waiting on more than one descriptor.
//!
//! A set is itself a descriptor, which is what makes it composable and is why
//! this is epoll-shaped rather than a call that takes an array: the set is
//! built once and waited on many times, instead of being marshalled across the
//! boundary on every iteration of a loop that runs once per frame.
//!
//! Readiness is evaluated rather than stored. A stored bit has to be
//! invalidated by everything that could change it, and the ways to get that
//! wrong are a task that sleeps through data already waiting for it. Sixteen
//! entries scanned at wake-up is cheaper than being wrong.

use crate::task::FdKind;
use crate::{pipe, scheduler, stream};

const MAX_SETS: usize = 64;
const MAX_WATCHED: usize = 32;

pub const READABLE: u32 = 1;
pub const WRITABLE: u32 = 2;
pub const HANGUP: u32 = 4;

#[derive(Clone, Copy)]
struct Watch {
    fd: usize,
    events: u32,
    token: u64,
    used: bool,
}

struct PollSet {
    in_use: bool,
    owner: usize,
    watches: [Watch; MAX_WATCHED],
}

impl PollSet {
    const fn empty() -> Self {
        PollSet {
            in_use: false,
            owner: 0,
            watches: [Watch { fd: 0, events: 0, token: 0, used: false }; MAX_WATCHED],
        }
    }
}

static mut SETS: [PollSet; MAX_SETS] = {
    const S: PollSet = PollSet::empty();
    [S; MAX_SETS]
};

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe { core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack)) };
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe { core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack)) };
}

#[inline(always)]
unsafe fn sets() -> &'static mut [PollSet; MAX_SETS] { unsafe {
    &mut *core::ptr::addr_of_mut!(SETS)
}}

pub fn create(tid: usize) -> Option<usize> {
    let flags = irq_save();
    let out = unsafe {
        match sets().iter().position(|s| !s.in_use) {
            Some(i) => {
                sets()[i] = PollSet::empty();
                sets()[i].in_use = true;
                sets()[i].owner = tid;
                Some(i)
            }
            None => None,
        }
    };
    irq_restore(flags);
    out
}

pub fn destroy(set: usize) {
    if set >= MAX_SETS {
        return;
    }
    let flags = irq_save();
    unsafe { sets()[set] = PollSet::empty() };
    irq_restore(flags);
}

/// op: 0 add, 1 modify, 2 remove.
pub fn ctl(set: usize, tid: usize, op: u64, fd: usize, events: u32, token: u64) -> bool {
    if set >= MAX_SETS {
        return false;
    }
    let flags = irq_save();
    let ok = unsafe {
        let s = &mut sets()[set];
        if !s.in_use || s.owner != tid {
            false
        } else {
            match op {
                2 => {
                    for w in s.watches.iter_mut() {
                        if w.used && w.fd == fd {
                            w.used = false;
                        }
                    }
                    true
                }
                1 => {
                    let mut found = false;
                    for w in s.watches.iter_mut() {
                        if w.used && w.fd == fd {
                            w.events = events;
                            w.token = token;
                            found = true;
                        }
                    }
                    found
                }
                _ => {
                    if s.watches.iter().any(|w| w.used && w.fd == fd) {
                        false
                    } else {
                        match s.watches.iter_mut().find(|w| !w.used) {
                            Some(w) => {
                                *w = Watch { fd, events, token, used: true };
                                true
                            }
                            None => false,
                        }
                    }
                }
            }
        }
    };
    irq_restore(flags);
    ok
}

/// What a descriptor can do right now.
///
/// Only the kinds that can meaningfully block are answerable. An IPC endpoint
/// has no buffer to be ready, so adding one is refused at `ctl` time rather
/// than reported as never ready — a caller waiting forever on something that
/// cannot become ready deserves an error, not silence.
fn readiness(tid: usize, fd: usize) -> u32 {
    let kind = unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) if fd < crate::task::MAX_FDS => t.fds[fd],
            _ => return 0,
        }
    };
    let mut out = 0;
    match kind {
        FdKind::PipeRead(h) => {
            if pipe::readable(h) { out |= READABLE; }
            if pipe::no_writers(h) { out |= HANGUP; }
        }
        FdKind::PipeWrite(h) => {
            if pipe::writable(h) { out |= WRITABLE; }
            if pipe::no_readers(h) { out |= HANGUP; }
        }
        FdKind::StreamEnd { stream: s, end } => {
            if stream::readable(s, end) { out |= READABLE; }
            if stream::writable(s, end) { out |= WRITABLE; }
            if stream::peer_gone(s, end) { out |= HANGUP; }
        }
        _ => {}
    }
    out
}

/// Collect what is ready. Returns how many entries were filled.
pub fn scan(set: usize, tid: usize, out: &mut [(u64, u32)]) -> usize {
    if set >= MAX_SETS {
        return 0;
    }
    let mut n = 0;
    let flags = irq_save();
    let watches = unsafe {
        let s = &sets()[set];
        if !s.in_use || s.owner != tid {
            irq_restore(flags);
            return 0;
        }
        s.watches
    };
    irq_restore(flags);

    for w in watches.iter() {
        if !w.used || n == out.len() {
            continue;
        }
        // Hangup is always reported, asked for or not, because a caller
        // waiting for readable on a descriptor whose peer has gone would
        // otherwise wait for something that can never arrive.
        let r = readiness(tid, w.fd);
        let hit = (r & w.events) | (r & HANGUP);
        if hit != 0 {
            out[n] = (w.token, hit);
            n += 1;
        }
    }
    n
}
```

Add to `src/pipe.rs`:

```rust
/// No writer remains, so a read will never block again.
pub fn no_writers(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].writers == 0
    };
    irq_restore(flags);
    out
}

/// No reader remains, so a write has nowhere to go.
pub fn no_readers(handle: usize) -> bool {
    let flags = irq_save();
    let out = unsafe {
        handle < MAX_PIPES && PIPES[handle].in_use && PIPES[handle].readers == 0
    };
    irq_restore(flags);
    out
}
```

and to `src/stream.rs`:

```rust
/// Has the other end's descriptor gone?
pub fn peer_gone(stream: usize, end: u8) -> bool {
    if stream >= MAX_STREAMS || end > 1 {
        return true;
    }
    let flags = irq_save();
    let out = unsafe {
        let s = &streams()[stream];
        !s.in_use || s.refs[1 - end as usize] == 0
    };
    irq_restore(flags);
    out
}
```

`src/main.rs`: `mod pollset;`

`src/task.rs`, in `FdKind`: `PollSet { set: usize },`

`src/pipe.rs`, in `release_fd`:

```rust
        FdKind::PollSet { set } => crate::pollset::destroy(*set),
```

`src/syscall.rs` — the numbers and arms. `wait` sleeps in ten-tick slices and
rescans, which is honest about what this first version is: correctness before
the wake-up path, and the tick is the same one `sys_recv_timeout` already uses.

```rust
pub const SYS_POLLSET_CREATE: u64 = 75;
pub const SYS_POLLSET_CTL: u64 = 76;
pub const SYS_POLLSET_WAIT: u64 = 77;
```

```rust
SYS_POLLSET_CREATE => {
    let tid = scheduler::current_tid();
    match crate::pollset::create(tid) {
        Some(set) => match scheduler::install_fd(tid, crate::task::FdKind::PollSet { set }) {
            Some(fd) => fd as u64,
            None => { crate::pollset::destroy(set); u64::MAX }
        },
        None => u64::MAX,
    }
}
SYS_POLLSET_CTL => {
    // arg0 = set fd, arg1 = op, arg2 = fd, arg3 = events, arg4 = token
    let tid = scheduler::current_tid();
    let set = match pollset_of(tid, arg0 as usize) { Some(s) => s, None => return u64::MAX };
    let target = arg2 as usize;
    // Refuse what can never become ready, rather than accept it and go quiet.
    if arg1 != 2 && !crate::pollset::watchable(tid, target) {
        return u64::MAX;
    }
    if crate::pollset::ctl(set, tid, arg1, target, arg3 as u32, arg4) { 0 } else { u64::MAX }
}
SYS_POLLSET_WAIT => {
    // arg0 = set fd, arg1 = out array of (u64 token, u32 events, u32 pad),
    // arg2 = capacity, arg3 = timeout in ticks
    let tid = scheduler::current_tid();
    let set = match pollset_of(tid, arg0 as usize) { Some(s) => s, None => return u64::MAX };
    let cap = (arg2 as usize).min(64);
    if cap == 0 {
        return u64::MAX;
    }
    // Private to this file — the check lives beside the dispatcher, not in
    // `paging`.
    if !validate_user_ptr_mut(arg1, (cap * 16) as u64) {
        return u64::MAX;
    }

    let deadline = crate::pit::ticks().saturating_add(arg3);
    let mut found = [(0u64, 0u32); 64];
    loop {
        let n = crate::pollset::scan(set, tid, &mut found[..cap]);
        if n > 0 {
            let ua = crate::cpu::UserAccess::begin();
            for i in 0..n {
                unsafe {
                    let p = (arg1 as *mut u8).add(i * 16);
                    *(p as *mut u64) = found[i].0;
                    *(p.add(8) as *mut u32) = found[i].1;
                    *(p.add(12) as *mut u32) = 0;
                }
            }
            drop(ua);
            return n as u64;
        }
        let now = crate::pit::ticks();
        if now >= deadline {
            return 0;
        }
        // There is no `sleep` in this kernel. A task sleeps by receiving from
        // its own TID with a timeout — nobody can send to that, so only the
        // deadline ends it — and `sleep_ticks` in quark-rt is exactly this.
        // Reusing it means the existing timeout sweep already knows how to
        // abandon the block, and no new sweep has to be written.
        let _ = crate::ipc::sys_recv_timeout(tid, deadline - now);
    }
}
```

with the helper beside them:

```rust
fn pollset_of(tid: usize, fd: usize) -> Option<usize> {
    if fd >= crate::task::MAX_FDS {
        return None;
    }
    unsafe {
        match scheduler::get_task_mut(tid)?.fds[fd] {
            crate::task::FdKind::PollSet { set } => Some(set),
            _ => None,
        }
    }
}
```

and in `src/pollset.rs`:

```rust
/// Can this descriptor ever become ready? Only kinds with a buffer can.
pub fn watchable(tid: usize, fd: usize) -> bool {
    unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) if fd < crate::task::MAX_FDS => matches!(
                t.fds[fd],
                FdKind::PipeRead(_) | FdKind::PipeWrite(_) | FdKind::StreamEnd { .. }
            ),
            _ => false,
        }
    }
}
```

`user/quark-rt/src/syscall.rs`:

```rust
pub const SYS_POLLSET_CREATE: u64 = 75;
pub const SYS_POLLSET_CTL: u64 = 76;
pub const SYS_POLLSET_WAIT: u64 = 77;

pub const POLL_READABLE: u32 = 1;
pub const POLL_WRITABLE: u32 = 2;
pub const POLL_HANGUP: u32 = 4;

/// One ready descriptor, as the kernel writes it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Ready {
    pub token: u64,
    pub events: u32,
    pub _pad: u32,
}

impl Ready {
    pub const fn empty() -> Self {
        Ready { token: 0, events: 0, _pad: 0 }
    }
}

/// A set of descriptors to wait on. It is itself a descriptor.
pub fn sys_pollset_create() -> Result<usize, ()> {
    let ret = unsafe { syscall0(SYS_POLLSET_CREATE) };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

/// Watch `fd` for `events`, reporting `token` when it fires.
pub fn sys_pollset_add(set: usize, fd: usize, events: u32, token: u64) -> Result<(), ()> {
    let ret = unsafe {
        syscall5(SYS_POLLSET_CTL, set as u64, 0, fd as u64, events as u64, token)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_pollset_modify(set: usize, fd: usize, events: u32, token: u64) -> Result<(), ()> {
    let ret = unsafe {
        syscall5(SYS_POLLSET_CTL, set as u64, 1, fd as u64, events as u64, token)
    };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

pub fn sys_pollset_remove(set: usize, fd: usize) -> Result<(), ()> {
    let ret = unsafe { syscall5(SYS_POLLSET_CTL, set as u64, 2, fd as u64, 0, 0) };
    if ret == u64::MAX { Err(()) } else { Ok(()) }
}

/// Wait until something in the set is ready, or `timeout_ticks` pass.
/// Returns how many entries of `out` were filled; 0 means it timed out.
pub fn sys_pollset_wait(set: usize, out: &mut [Ready], timeout_ticks: u64) -> Result<usize, ()> {
    let ret = unsafe {
        syscall4(
            SYS_POLLSET_WAIT,
            set as u64,
            out.as_mut_ptr() as u64,
            out.len() as u64,
            timeout_ticks,
        )
    };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: eleven more `ok` lines, `0 failed`.

- [ ] **Step 5: Commit**

```bash
git add src/pollset.rs src/main.rs src/task.rs src/pipe.rs src/stream.rs \
        src/syscall.rs user/quark-rt/src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Wait on a set of descriptors

Quark had no way to wait on more than one thing, and it shows everywhere: the
compositor polls the keyboard on a timer because it cannot block on both a
client and the input server at once.

A set is itself a descriptor, which is why this is epoll-shaped rather than a
call taking an array: the set is built once and waited on many times, rather
than marshalled across the boundary every time round a loop that runs once per
frame.

Readiness is evaluated at wake-up rather than stored. A stored bit has to be
invalidated by everything that could change it, and the way to get that wrong
is a task that sleeps through data already waiting for it.

Adding a descriptor that can never become ready is refused rather than
accepted, because a caller waiting forever on an IPC endpoint deserves an error
and not silence. Hangup is reported whether or not it was asked for, for the
same reason.

This version sleeps and rescans rather than being woken by the pipe that became
ready — correctness before the wake-up path, which is the next commit. The
sleep is `sys_recv_timeout` on the task's own TID, which is how everything in
this system already sleeps, so the existing timeout sweep abandons the block
and no new one had to be written.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Wake the waiter instead of rescanning

**Files:**
- Modify: `src/pollset.rs`, `src/pipe.rs`, `src/syscall.rs`,
  `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Task 7.
- Produces: `pollset::park(set, tid)`, `pollset::wake_watchers(fd_kind_key)`;
  `pipe::note_change(handle)` called from `pipe::read` and `pipe::write`.

Task 7 polls on a tick, which is the thing this phase exists to remove. A pipe
whose state changes must wake the sets watching it.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
fn test_pollset_latency() {
    println!("waiting wakes promptly:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("a set", false); return; }
    };
    let _ = syscall::sys_pollset_add(set, b, syscall::POLL_READABLE, 1);

    // Write first, then wait: the data is already there, so a correct
    // implementation returns without sleeping at all.
    let _ = syscall::sys_fd_write(a, b"now");
    let before = syscall::sys_ticks();
    let mut ready = [syscall::Ready::empty(); 2];
    let n = syscall::sys_pollset_wait(set, &mut ready, 100);
    let elapsed = syscall::sys_ticks() - before;
    check("data already waiting returns at once", n == Ok(1) && elapsed <= 1);

    let mut buf = [0u8; 8];
    let _ = syscall::sys_fd_read(b, &mut buf);

    // Nothing to read: this must time out at the deadline and not before.
    let before = syscall::sys_ticks();
    let n = syscall::sys_pollset_wait(set, &mut ready, 20);
    let elapsed = syscall::sys_ticks() - before;
    check("an empty wait runs its full timeout", n == Ok(0) && elapsed >= 20);

    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}
```

- [ ] **Step 2: Run it to verify it fails**

Build, boot, run `dtest`.
Expected: `FAIL  an empty wait runs its full timeout` is likely to pass by
accident, but `data already waiting returns at once` may already pass. Record
what actually happens: the value of this test is that it *pins* the latency
before the wake-up path is added and proves it did not regress after.

If both already pass, the timing assertions are still the regression net for
Step 3, which changes how the wait sleeps.

- [ ] **Step 3: Write the minimal implementation**

In `src/pollset.rs`, a parked waiter per set, and a wake that scans sets rather
than the reverse — there are at most 64 sets and the alternative is a back
pointer in every pipe:

```rust
/// The task blocked in `wait` on this set, if any.
///
/// One waiter per set. Two tasks waiting on one set would each need to be told
/// which of them takes an event, and nothing here shares a set.
static mut WAITERS: [usize; MAX_SETS] = [usize::MAX; MAX_SETS];

pub fn park(set: usize, tid: usize) {
    if set < MAX_SETS {
        let flags = irq_save();
        unsafe { (*core::ptr::addr_of_mut!(WAITERS))[set] = tid };
        irq_restore(flags);
    }
}

pub fn unpark(set: usize) {
    if set < MAX_SETS {
        let flags = irq_save();
        unsafe { (*core::ptr::addr_of_mut!(WAITERS))[set] = usize::MAX };
        irq_restore(flags);
    }
}

/// A pipe changed state. Wake any set watching a descriptor that names it.
///
/// Sets are scanned rather than pipes carrying a list of watchers: there are
/// sixty-four sets with at most thirty-two watches each, and the alternative
/// puts a back pointer in every pipe for the benefit of the rare one that is
/// watched.
pub fn note_pipe(handle: usize) {
    let flags = irq_save();
    let mut wake = [usize::MAX; MAX_SETS];
    let mut n = 0;
    unsafe {
        for i in 0..MAX_SETS {
            let waiter = (*core::ptr::addr_of!(WAITERS))[i];
            if waiter == usize::MAX || !sets()[i].in_use {
                continue;
            }
            let owner = sets()[i].owner;
            for w in sets()[i].watches.iter() {
                if !w.used {
                    continue;
                }
                if names_pipe(owner, w.fd, handle) {
                    wake[n] = waiter;
                    n += 1;
                    break;
                }
            }
        }
    }
    irq_restore(flags);
    for i in 0..n {
        scheduler::unblock_task(wake[i]);
    }
}

fn names_pipe(tid: usize, fd: usize, handle: usize) -> bool {
    unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) if fd < crate::task::MAX_FDS => match t.fds[fd] {
                FdKind::PipeRead(h) | FdKind::PipeWrite(h) => h == handle,
                FdKind::StreamEnd { stream: s, end } => match stream::pipes_for(s, end) {
                    Some((rd, wr)) => rd == handle || wr == handle,
                    None => false,
                },
                _ => false,
            },
            _ => false,
        }
    }
}
```

In `src/pipe.rs`, call it wherever `len`, `readers` or `writers` change — at
the end of `read`, `write`, `add_ref` and `drop_ref`:

```rust
    crate::pollset::note_pipe(handle);
```

In `src/syscall.rs`, replace the `SYS_POLLSET_WAIT` sleep loop:

```rust
    let deadline = crate::pit::ticks().saturating_add(arg3);
    let mut found = [(0u64, 0u32); 64];
    loop {
        let n = crate::pollset::scan(set, tid, &mut found[..cap]);
        if n > 0 {
            crate::pollset::unpark(set);
            // ... write the entries out as before, then return n
        }
        if crate::pit::ticks() >= deadline {
            crate::pollset::unpark(set);
            return 0;
        }
        // Park before the last check so that a change landing between the scan
        // and the block is a wake-up rather than a missed one.
        crate::pollset::park(set, tid);
        if crate::pollset::scan(set, tid, &mut found[..cap]) > 0 {
            crate::pollset::unpark(set);
            continue;
        }
        let now = crate::pit::ticks();
        let _ = crate::ipc::sys_recv_timeout(tid, deadline.saturating_sub(now));
    }
```

The block is still `sys_recv_timeout` on the task's own TID — what changes is
that it no longer runs its full tick, because `note_pipe` now ends it early.
Waking a task blocked that way needs one addition to `src/ipc.rs`, because
`sys_notify` only wakes a task in `RecvBlocked(TID_ANY)` and this one is in
`RecvBlocked(itself)`:

```rust
/// Wake a task sleeping in `sys_recv_timeout` on its own TID.
///
/// That is how a task sleeps here — nobody can send to your own TID, so only
/// the deadline ends the block. `sys_notify` deliberately wakes only a task
/// waiting on `TID_ANY`, since a notification is a message and that task asked
/// for one. This is not a message: it is a sleeper being told its deadline no
/// longer matters.
pub fn wake_sleeper(tid: usize) {
    if tid >= MAX_TASKS {
        return;
    }
    let flags = irq_save();
    unsafe {
        if matches!(TASK_IPC[tid].state, IpcState::RecvBlocked(t) if t == tid) {
            TASK_IPC[tid].state = IpcState::None;
            TASK_TIMEOUT[tid] = 0;
            scheduler::unblock_task(tid);
        }
    }
    irq_restore(flags);
}
```

and `note_pipe` calls it rather than `scheduler::unblock_task` directly:

```rust
    for i in 0..n {
        crate::ipc::wake_sleeper(wake[i]);
    }
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: both timing checks pass, and every check
from Task 7 still passes.

- [ ] **Step 5: Commit**

```bash
git add src/pollset.rs src/pipe.rs src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Wake a waiting set instead of rescanning it

The first version of wait slept a tick and looked again, which is the polling
this phase exists to remove. A pipe whose state changes now wakes the sets
watching it.

Sets are scanned rather than pipes carrying a list of their watchers. There are
sixty-four sets with at most thirty-two watches each, and the alternative puts a
back pointer in every pipe for the benefit of the rare one that anybody
watches.

The park happens before the last scan rather than after, so that a change
landing between looking and blocking is a wake-up instead of a missed one.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: One-shot poll

**Files:**
- Modify: `src/syscall.rs`, `src/pollset.rs`, `user/quark-rt/src/syscall.rs`,
  `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: Tasks 7 and 8.
- Produces: `SYS_POLL: u64 = 78`;
  `#[repr(C)] pub struct PollFd { pub fd: u32, pub events: u32, pub revents: u32, pub _pad: u32 }`;
  `sys_poll(fds: &mut [PollFd], timeout_ticks: u64) -> Result<usize, ()>`.

libwayland-client calls `poll()`, not `epoll`. Without this, every dispatch
builds and tears down a set for two descriptors — three system calls where one
will do.

- [ ] **Step 1: Write the failing test**

```rust
fn test_poll() {
    println!("one-shot poll:");
    let (a, b) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let mut fds = [
        syscall::PollFd::new(b, syscall::POLL_READABLE),
        syscall::PollFd::new(a, syscall::POLL_WRITABLE),
    ];
    let n = syscall::sys_poll(&mut fds, 50);
    check("the writable end fires", n == Ok(1));
    check("and it is the second entry", fds[1].revents & syscall::POLL_WRITABLE != 0);
    check("the first reports nothing", fds[0].revents == 0);

    let _ = syscall::sys_fd_write(a, b"z");
    let mut fds = [syscall::PollFd::new(b, syscall::POLL_READABLE)];
    check("after a write it is readable", syscall::sys_poll(&mut fds, 50) == Ok(1));

    let _ = syscall::sys_fd_close(a);
    let _ = syscall::sys_fd_close(b);
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `sys_poll` ``.

- [ ] **Step 3: Write the minimal implementation**

In `src/pollset.rs`, expose the readiness function the one-shot needs:

```rust
/// What one descriptor can do now, for callers with no set.
pub fn readiness_of(tid: usize, fd: usize) -> u32 {
    readiness(tid, fd)
}
```

In `src/syscall.rs`:

```rust
pub const SYS_POLL: u64 = 78;
```

```rust
SYS_POLL => {
    // arg0 = array of (u32 fd, u32 events, u32 revents, u32 pad),
    // arg1 = count, arg2 = timeout in ticks.
    //
    // The same evaluation a set does, without the set. libwayland calls
    // poll(), and making it build and tear down a set for two descriptors
    // every frame would be three system calls where one will do.
    let n = (arg1 as usize).min(32);
    if n == 0 || !validate_user_ptr_mut(arg0, (n * 16) as u64) {
        return u64::MAX;
    }
    let tid = scheduler::current_tid();
    let deadline = crate::pit::ticks().saturating_add(arg2);

    let mut want = [(0usize, 0u32); 32];
    {
        let _ua = crate::cpu::UserAccess::begin();
        for i in 0..n {
            unsafe {
                let p = (arg0 as *const u8).add(i * 16);
                want[i] = (*(p as *const u32) as usize, *(p.add(4) as *const u32));
            }
        }
    }

    loop {
        let mut hits = 0usize;
        let mut rev = [0u32; 32];
        for i in 0..n {
            let r = crate::pollset::readiness_of(tid, want[i].0);
            let hit = (r & want[i].1) | (r & crate::pollset::HANGUP);
            rev[i] = hit;
            if hit != 0 {
                hits += 1;
            }
        }
        let now = crate::pit::ticks();
        if hits > 0 || now >= deadline {
            let _ua = crate::cpu::UserAccess::begin();
            for i in 0..n {
                unsafe { *((arg0 as *mut u8).add(i * 16 + 8) as *mut u32) = rev[i] };
            }
            return hits as u64;
        }
        // Same sleep as the set's wait: receive from our own TID with a
        // deadline. `pollset::note_pipe` ends it early through
        // `ipc::wake_sleeper`, so this is not a poll on a timer.
        let _ = crate::ipc::sys_recv_timeout(tid, deadline - now);
    }
}
```

In `user/quark-rt/src/syscall.rs`:

```rust
pub const SYS_POLL: u64 = 78;

/// One descriptor to watch, and what it did.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd {
    pub fd: u32,
    pub events: u32,
    pub revents: u32,
    pub _pad: u32,
}

impl PollFd {
    pub const fn new(fd: usize, events: u32) -> Self {
        PollFd { fd: fd as u32, events, revents: 0, _pad: 0 }
    }
}

/// Wait on several descriptors without building a set.
///
/// A set is the better primitive when it is waited on many times; this is for
/// the caller that waits once, which is what `poll(2)` is and what libwayland
/// calls every time it dispatches.
pub fn sys_poll(fds: &mut [PollFd], timeout_ticks: u64) -> Result<usize, ()> {
    let ret = unsafe {
        syscall3(SYS_POLL, fds.as_mut_ptr() as u64, fds.len() as u64, timeout_ticks)
    };
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}
```

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: four more `ok` lines.

- [ ] **Step 5: Commit**

```bash
git add src/pollset.rs src/syscall.rs user/quark-rt/src/syscall.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
A one-shot poll beside the set

A set is the better primitive when it is waited on many times. libwayland waits
once per dispatch and calls poll(), and making it build and tear down a set for
two descriptors every frame would be three system calls where one will do.

Same readiness evaluation, no set.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: An environment

**Files:**
- Modify: `user/quark-rt/src/args.rs`, `user/quark-rt/src/spawn.rs`,
  `user/init/src/main.rs`, `user/shell/src/main.rs`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: args-page layout gains an environment section after the arguments;
  `quark_rt::args::envc()`, `args::envp(i) -> Option<&'static [u8]>`,
  `args::getenv(name: &[u8]) -> Option<&'static [u8]>`;
  `spawn::set_args_env(info, args, env, scratch) -> Result<(), ()>`.

`wl_display_connect` reads `WAYLAND_SOCKET` from the environment, and Quark has
none at all. The page format grows a section *after* the arguments, so a reader
that stops at the end of argv is unaffected.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called from `_start`:

```rust
fn test_environment() {
    println!("environment:");
    // The shell puts these in every program's environment.
    check("HOME is set", quark_rt::args::getenv(b"HOME").is_some());
    check(
        "and it is a path",
        quark_rt::args::getenv(b"HOME").map(|v| v.starts_with(b"/")) == Some(true),
    );
    check("a name nobody set is absent", quark_rt::args::getenv(b"NOPE").is_none());
    // A prefix of a real name must not match it.
    check("HOM does not match HOME", quark_rt::args::getenv(b"HOM").is_none());
    check("arguments still readable", quark_rt::args::argv(0).is_some());
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `` cannot find function `getenv` in module `args` ``.

- [ ] **Step 3: Write the minimal implementation**

In `user/quark-rt/src/args.rs`, extend the documented layout and add the
readers:

```rust
/// Program arguments and environment, passed by a spawner on a mapped page.
///
/// Layout at ARGS_PAGE_ADDR:
///   [argc: u64]
///   [arg0_len: u64] [arg0 bytes (no null terminator)]
///   ...
///   [envc: u64]
///   [env0_len: u64] [env0 bytes, "NAME=value"]
///   ...
///
/// The environment is *after* the arguments rather than before, so a program
/// built before it existed reads the same argv it always did and simply never
/// looks further.

/// Byte offset just past the last argument.
fn env_offset() -> usize {
    let base = ARGS_PAGE_ADDR as *const u8;
    let count = argc();
    let mut offset = 8usize;
    for _ in 0..count {
        let len = unsafe { *(base.add(offset) as *const u64) } as usize;
        offset += 8 + len;
    }
    offset
}

/// How many environment entries there are.
pub fn envc() -> usize {
    let off = env_offset();
    if off + 8 > 4096 {
        return 0;
    }
    unsafe { *((ARGS_PAGE_ADDR + off) as *const u64) as usize }
}

/// The Nth environment entry, as `NAME=value`.
pub fn envp(index: usize) -> Option<&'static [u8]> {
    let count = envc();
    if index >= count {
        return None;
    }
    let base = ARGS_PAGE_ADDR as *const u8;
    let mut offset = env_offset() + 8;
    for i in 0..count {
        let len = unsafe { *(base.add(offset) as *const u64) } as usize;
        offset += 8;
        if i == index {
            return Some(unsafe { core::slice::from_raw_parts(base.add(offset), len) });
        }
        offset += len;
    }
    None
}

/// The value of `name`, or `None`.
///
/// The `=` is checked as well as the prefix: without it `HOM` would match
/// `HOME=/home/root` and return `E=/home/root`.
pub fn getenv(name: &[u8]) -> Option<&'static [u8]> {
    for i in 0..envc() {
        let entry = envp(i)?;
        if entry.len() > name.len()
            && &entry[..name.len()] == name
            && entry[name.len()] == b'='
        {
            return Some(&entry[name.len() + 1..]);
        }
    }
    None
}
```

In `user/quark-rt/src/spawn.rs`, replace `set_args` with a version that also
writes the environment, keeping the old name as a wrapper so no caller breaks:

```rust
/// Write `args` and `env` into the child's argument page.
///
/// Layout: a count, then each entry as a length followed by its bytes; the
/// arguments first and the environment after them. Entries that would overflow
/// the page are dropped rather than truncated, because half an environment
/// variable is worse than a missing one.
pub fn set_args_env(
    info: &Spawned,
    args: &[&[u8]],
    env: &[&[u8]],
    scratch: &Scratch,
) -> Result<(), ()> {
    let frame = syscall::sys_phys_alloc(1)?;
    syscall::sys_map_phys(frame, scratch.args, 1)?;

    let base = scratch.args as *mut u8;
    unsafe {
        core::ptr::write_bytes(base, 0, PAGE_SIZE);

        let mut offset = 0usize;
        let mut write_section = |items: &[&[u8]], offset: &mut usize| {
            let count_at = *offset;
            *offset += 8;
            let mut written = 0u64;
            for item in items {
                if *offset + 8 + item.len() > PAGE_SIZE {
                    break;
                }
                *(base.add(*offset) as *mut u64) = item.len() as u64;
                *offset += 8;
                core::ptr::copy_nonoverlapping(item.as_ptr(), base.add(*offset), item.len());
                *offset += item.len();
                written += 1;
            }
            *(base.add(count_at) as *mut u64) = written;
        };
        write_section(args, &mut offset);
        write_section(env, &mut offset);
    }

    syscall::sys_addrspace_map(info.cr3, ARGS_PAGE_ADDR, frame, 1, 0)?;
    Ok(())
}

/// As [`set_args_env`], with no environment.
pub fn set_args(info: &Spawned, args: &[&[u8]], scratch: &Scratch) -> Result<(), ()> {
    set_args_env(info, args, &[], scratch)
}
```

In `user/shell/src/main.rs`, where it calls `spawn::set_args`, pass an
environment. Add near the top:

```rust
/// What every program the shell starts is told about the world.
///
/// Fixed for now: there is no `export`, and nothing sets a variable at run
/// time. It exists because software ported here expects these to be answerable
/// — and because a Wayland client is handed its connection this way.
const BASE_ENV: [&[u8]; 3] = [b"HOME=/home/root", b"PATH=/usr/bin", b"TERM=quark"];
```

and change the call to `spawn::set_args_env(&info, &argv, &BASE_ENV, &SCRATCH)`.
Do the same in `user/init/src/main.rs` for the programs it starts.

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run `dtest`. Expected: five more `ok` lines.
Confirm no regression: `echo hello` and `wc /etc/passwd` still work, since both
read argv out of the same page.

- [ ] **Step 5: Commit**

```bash
git add user/quark-rt/src/args.rs user/quark-rt/src/spawn.rs \
        user/init/src/main.rs user/shell/src/main.rs user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
Give a program an environment

Quark had none at all, which Phase 6 already recorded as a gap: anything
reading TMPDIR or LANG got nothing. It becomes load-bearing in Phase 8, because
wl_display_connect reads WAYLAND_SOCKET and takes it as an already-connected
descriptor — which is what lets libwayland be used unpatched.

The environment goes *after* the arguments on the page a spawner maps, so a
program built before it existed reads the same argv it always did and never
looks further.

getenv checks the `=` as well as the prefix. Without that, HOM matches
HOME=/home/root and returns E=/home/root.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: The environment in C, both libraries

**Files:**
- Create: `user/libc/src/env.c`
- Modify: `user/libc/include/stdlib.h`, `user/libc/Makefile`,
  `user/linux-abi/src/start.c`, `user/cwc/` (nothing — verify only)

**Interfaces:**
- Consumes: Task 10's page layout.
- Produces: `char **environ`; `char *getenv(const char *)`,
  `int setenv(const char *, const char *, int)`, `int unsetenv(const char *)`
  in `user/libc`; a populated `envp` in the block `__quark_start_args` builds
  for musl.

musl's `unsetenv` writes into the `__environ` **array** — so the array must be
writable even though the strings it points at need not be. The block
`start.c` builds is a static, which is writable; the strings stay on the
read-only page. That is why this works without making the page writable.

- [ ] **Step 1: Write the failing test**

Create `user/dtest/env.c` — a C program built against `user/libc`, registered
in the Makefile beside `cwc`:

```c
/* The environment, from C. Two libraries answer this: Quark's own, and musl
   through the translation layer. This is built against the first. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int passed, failed;

static void check(const char *what, int ok) {
    if (ok) { passed++; printf("  ok    %s\n", what); }
    else    { failed++; printf("  FAIL  %s\n", what); }
}

int main(void) {
    printf("environment (C):\n");
    check("HOME is set", getenv("HOME") != 0);
    check("HOME is a path", getenv("HOME") && getenv("HOME")[0] == '/');
    check("an unset name is null", getenv("NOPE") == 0);
    check("a prefix does not match", getenv("HOM") == 0);

    check("setenv a new name", setenv("QUARK_T", "1", 1) == 0);
    check("and read it back", getenv("QUARK_T") && !strcmp(getenv("QUARK_T"), "1"));
    check("setenv without overwrite keeps the old", setenv("QUARK_T", "2", 0) == 0
          && !strcmp(getenv("QUARK_T"), "1"));
    check("setenv with overwrite replaces it", setenv("QUARK_T", "2", 1) == 0
          && !strcmp(getenv("QUARK_T"), "2"));
    check("unsetenv removes it", unsetenv("QUARK_T") == 0 && getenv("QUARK_T") == 0);

    printf("[envtest] %d passed, %d failed\n", passed, failed);
    return failed == 0 ? 0 : 1;
}
```

- [ ] **Step 2: Run it to verify it fails**

Build. Expected: FAIL — `stdlib.h` declares no `getenv`.

- [ ] **Step 3: Write the minimal implementation**

Create `user/libc/src/env.c`:

```c
/* The environment.
 *
 * The strings live on the page a spawner maps, which is read-only, and that is
 * fine: nothing here writes through them. `environ` is an array of pointers in
 * our own memory, and setenv allocates a new string rather than editing one in
 * place. That is also what makes musl work over the same page — its unsetenv
 * shuffles the array and never touches a string.
 */

#include <quark/layout.h>
#include <stdlib.h>

#define ARGS_PAGE  QUARK_ARGS_PAGE
#define PAGE_BYTES 4096
#define MAX_ENV    64

static char *slots[MAX_ENV + 1];
char **environ = slots;
static int ready;
/* Storage for names set at run time. A program that sets more than this many
   variables gets a refusal rather than a corrupted array. */
static char pool[2048];
static unsigned long pool_used;

static unsigned long read_word(const unsigned char *p) {
    return *(const unsigned long *)p;
}

static int same_name(const char *entry, const char *name, unsigned long n) {
    for (unsigned long i = 0; i < n; i++) {
        if (entry[i] != name[i]) {
            return 0;
        }
    }
    return entry[n] == '=';
}

static unsigned long length(const char *s) {
    unsigned long n = 0;
    while (s[n]) {
        n++;
    }
    return n;
}

/* Point `slots` at the entries on the args page.
 *
 * The page holds lengths and bytes with no terminators, and `getenv` returns a
 * C string, so each entry is copied into the pool with a null on the end. The
 * page itself stays untouched and read-only. */
static void init(void) {
    if (ready) {
        return;
    }
    ready = 1;

    const unsigned char *page = (const unsigned char *)ARGS_PAGE;
    unsigned long off = 0;
    unsigned long argc = read_word(page);
    off += sizeof(unsigned long);
    for (unsigned long i = 0; i < argc; i++) {
        unsigned long len = read_word(page + off);
        off += sizeof(unsigned long) + len;
        if (off > PAGE_BYTES) {
            return;
        }
    }
    if (off + sizeof(unsigned long) > PAGE_BYTES) {
        return;
    }
    unsigned long envc = read_word(page + off);
    off += sizeof(unsigned long);

    int n = 0;
    for (unsigned long i = 0; i < envc && n < MAX_ENV; i++) {
        if (off + sizeof(unsigned long) > PAGE_BYTES) {
            break;
        }
        unsigned long len = read_word(page + off);
        off += sizeof(unsigned long);
        if (off + len > PAGE_BYTES || pool_used + len + 1 > sizeof pool) {
            break;
        }
        char *dst = pool + pool_used;
        for (unsigned long j = 0; j < len; j++) {
            dst[j] = (char)page[off + j];
        }
        dst[len] = '\0';
        pool_used += len + 1;
        off += len;
        slots[n++] = dst;
    }
    slots[n] = 0;
}

char *getenv(const char *name) {
    init();
    unsigned long n = length(name);
    if (n == 0) {
        return 0;
    }
    for (int i = 0; environ[i]; i++) {
        if (same_name(environ[i], name, n)) {
            return environ[i] + n + 1;
        }
    }
    return 0;
}

int setenv(const char *name, const char *value, int overwrite) {
    init();
    unsigned long n = length(name);
    unsigned long v = length(value);
    if (n == 0) {
        return -1;
    }

    int at = -1;
    int end = 0;
    for (; environ[end]; end++) {
        if (at < 0 && same_name(environ[end], name, n)) {
            at = end;
        }
    }
    if (at >= 0 && !overwrite) {
        return 0;
    }
    if (pool_used + n + v + 2 > sizeof pool) {
        return -1;
    }
    if (at < 0 && end >= MAX_ENV) {
        return -1;
    }

    char *entry = pool + pool_used;
    for (unsigned long i = 0; i < n; i++) {
        entry[i] = name[i];
    }
    entry[n] = '=';
    for (unsigned long i = 0; i < v; i++) {
        entry[n + 1 + i] = value[i];
    }
    entry[n + 1 + v] = '\0';
    pool_used += n + v + 2;

    if (at >= 0) {
        environ[at] = entry;
    } else {
        environ[end] = entry;
        environ[end + 1] = 0;
    }
    return 0;
}

int unsetenv(const char *name) {
    init();
    unsigned long n = length(name);
    if (n == 0) {
        return -1;
    }
    int w = 0;
    for (int r = 0; environ[r]; r++) {
        if (same_name(environ[r], name, n)) {
            continue;
        }
        environ[w++] = environ[r];
    }
    environ[w] = 0;
    return 0;
}
```

In `user/libc/include/stdlib.h`:

```c
extern char **environ;
char *getenv(const char *name);
int setenv(const char *name, const char *value, int overwrite);
int unsetenv(const char *name);
```

In `user/libc/Makefile`, add `src/env.o` to `OBJS`.

In `user/linux-abi/src/start.c`, build a real environment instead of the empty
one. Extend the static block and copy the page's environment entries after the
argv terminator:

```c
#define MAX_ENV    32
#define ENV_BYTES  1024

/* argc, argv and its terminator, envp and its terminator, and one auxiliary
   entry. */
static unsigned long start_block[1 + MAX_ARGS + 1 + MAX_ENV + 1 + 4];
static char argv_bytes[ARGS_BYTES];
static char env_bytes[ENV_BYTES];
```

After the loop that fills argv and writes its NULL terminator, read the
environment count at the current page offset and fill the same way, then the
NULL terminator, then the auxv entries. The block is a static and therefore
writable, which is what musl's `unsetenv` requires: it shuffles this array,
though it never writes through the pointers.

- [ ] **Step 4: Run it to verify it passes**

Build, boot, run the C environment test. Expected: nine `ok` lines.
Then confirm musl sees it too — build a small musl program with
`x86_64-quark-musl-gcc` that prints `getenv("HOME")`, put it in the image, and
run it.

- [ ] **Step 5: Commit**

```bash
git add user/libc/src/env.c user/libc/include/stdlib.h user/libc/Makefile \
        user/linux-abi/src/start.c user/dtest Makefile
git commit -m "$(cat <<'EOF'
The environment, from C, in both libraries

The strings stay on the read-only page a spawner maps, and nothing writes
through them: `environ` is an array of pointers in our own memory, and setenv
allocates rather than editing in place.

That is also what makes musl work over the same page. Its unsetenv shuffles the
`__environ` array — so the array has to be writable — but it never touches a
string, and the block start.c builds is a static.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: Answer Linux's numbers, and write the ABI down

**Files:**
- Modify: `user/linux-abi/src/syscall.c`, `docs/abi.md`, `src/syscall.rs`,
  `user/quark-rt/src/syscall.rs`

**Interfaces:**
- Consumes: every earlier task.
- Produces: `ABI_VERSION_MINOR = 6`; Linux `memfd_create` (319),
  `socketpair` (53), `sendmsg` (46), `recvmsg` (47), `poll` (7),
  `ppoll` (271), `epoll_create1` (291), `epoll_ctl` (233), `epoll_wait` (232)
  answered by the translation layer.

- [ ] **Step 1: Write the failing test**

Add to `user/dtest/src/main.rs`, called last from `_start`:

```rust
fn test_abi_version() {
    println!("abi:");
    // Returns (major, minor) already split, not a packed word.
    let (major, minor) = syscall::sys_abi_version();
    check("major is 1", major == 1);
    check("minor is at least 6", minor >= 6);
}
```

- [ ] **Step 2: Run it to verify it fails**

Build, boot, run `dtest`. Expected: `FAIL  minor is at least 6`.

- [ ] **Step 3: Write the minimal implementation**

In `src/syscall.rs`:

```rust
pub const ABI_VERSION_MINOR: u64 = 6;
```

In `docs/abi.md`: change the header to **Version 1.6**, add a row to
"What each minor added":

```markdown
| 1.6 | `SYS_MMAP_FD` (42), `SYS_MEMFD_CREATE` (53), `SYS_FD_CLOSE` (71), `SYS_SOCKETPAIR` (72), `SYS_FD_SEND` (73), `SYS_FD_RECV` (74), `SYS_POLLSET_CREATE` (75), `SYS_POLLSET_CTL` (76), `SYS_POLLSET_WAIT` (77), `SYS_POLL` (78) — a bidirectional stream, descriptor passing, memory named by a descriptor, and waiting on more than one thing at once. |
```

and a row per call in the "File descriptors and pipes (0x40)", "Shared memory
(0x30)" and "Memory (0x20)" tables, in the same three-column style the
existing rows use, giving arguments, returns and the capability required
(none, for all ten). Say in the fd section's prose that a task's descriptor
table is now **32** entries rather than eight.

In `user/linux-abi/src/syscall.c`, add the Linux numbers and their arms:

```c
#define LX_poll              7
#define LX_socketpair       53
#define LX_sendmsg          46
#define LX_recvmsg          47
#define LX_epoll_wait      232
#define LX_epoll_ctl       233
#define LX_ppoll           271
#define LX_epoll_create1   291
#define LX_memfd_create    319
```

```c
    case LX_memfd_create:
        /* The name and flags are Linux's bookkeeping; the size comes from a
           later ftruncate, which this has no equivalent of. musl's callers
           always follow with one, so the pages are allocated then. */
        return __quark_memfd(a1, a2);

    case LX_socketpair:
        /* Only AF_UNIX SOCK_STREAM, which is what a Wayland connection is. */
        return __quark_socketpair(a1, a2, a3, (int *)a4);

    case LX_sendmsg:
        return __quark_sendmsg(a1, (const void *)a2, a3);
    case LX_recvmsg:
        return __quark_recvmsg(a1, (void *)a2, a3);

    case LX_poll:
        return __quark_poll((void *)a1, a2, a3);
    case LX_ppoll:
        return __quark_ppoll((void *)a1, a2, (const void *)a3);

    case LX_epoll_create1:
        return __quark_epoll_create();
    case LX_epoll_ctl:
        return __quark_epoll_ctl(a1, a2, a3, (void *)a4);
    case LX_epoll_wait:
        return __quark_epoll_wait(a1, (void *)a2, a3, a4);
```

Implement those in a new `user/linux-abi/src/net.c` — the translation between
Linux's `struct msghdr` with its `cmsghdr` ancillary data and Quark's
`sys_fd_send`/`sys_fd_recv`, and between Linux's `struct pollfd` and Quark's.
The `SCM_RIGHTS` control message is the one shape that matters: a `cmsghdr`
with `cmsg_level == SOL_SOCKET` and `cmsg_type == SCM_RIGHTS`, whose data is
an array of `int`. Take the first, pass it, and refuse more than one — Wayland
sends one descriptor per message and refusing is better than silently dropping
the rest.

Register `src/net.o` in `user/linux-abi/Makefile`'s `OBJS`.

- [ ] **Step 4: Run it to verify it passes**

```bash
make            # check-abi must report the two files agree
```

Then boot and run `dtest`; expected: two more `ok` lines and `0 failed` across
every section.

Then the real check — a musl program using the new calls:

```c
/* /tmp/sp.c, built with x86_64-quark-musl-gcc */
#include <stdio.h>
#include <sys/socket.h>
#include <poll.h>
int main(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) { printf("socketpair failed\n"); return 1; }
    write(sv[0], "hi", 2);
    struct pollfd p = { .fd = sv[1], .events = POLLIN };
    printf("poll says %d\n", poll(&p, 1, 100));
    char b[4] = {0};
    read(sv[1], b, 2);
    printf("got %s\n", b);
    return 0;
}
```

Expected on the console: `poll says 1` then `got hi`.

- [ ] **Step 5: Commit**

```bash
git add src/syscall.rs docs/abi.md user/linux-abi/src/syscall.c \
        user/linux-abi/src/net.c user/linux-abi/Makefile user/dtest/src/main.rs
git commit -m "$(cat <<'EOF'
ABI 1.6: streams, passing, memory descriptors and waiting

Ten calls, and the document that is supposed to be enough to build against
without reading kernel source.

The translation layer answers Linux's spellings of them, which is what makes
libwayland buildable: socketpair, sendmsg and recvmsg with SCM_RIGHTS,
memfd_create, poll, and the epoll trio. SCM_RIGHTS carrying more than one
descriptor is refused rather than silently truncated — Wayland sends one per
message, and a caller that sends two and loses one would find out somewhere
else entirely.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: All of it, between two address spaces

**Files:**
- Create: `user/dchild/Cargo.toml`, `user/dchild/src/main.rs`
- Modify: `user/dtest/src/main.rs`, `Makefile`

**Interfaces:**
- Consumes: every earlier task.
- Produces: nothing later depends on this; it is the acceptance criterion.

Tasks 1–12 exercise all of this inside one process, where a descriptor is a
number in one table and a region is mapped once. Every one of these primitives
exists to be used between two tasks, and the bugs that only appear there — a
reference not taken across `SYS_FD_DUP`, a region the receiver was never
admitted to, a wake that goes to the wrong task — are exactly the ones this
plan would otherwise ship. So this is a task and not a note.

- [ ] **Step 1: Write the failing test**

Create `user/dchild/` with the same three files `user/dtest` has before its
source: `Cargo.toml` (with `name = "dchild"`), `.cargo/config.toml`, and a
`linker.ld` symlink to `../linker.ld`. None is optional — without the config it
links below `USER_MIN_ADDR` and the kernel refuses it, and without the symlink
it does not link at all.

Create `user/dchild/src/main.rs`:

```rust
#![no_std]
#![no_main]

//! The other half of `dtest`'s cross-process check.
//!
//! Started by `dtest` with one end of a socketpair already at descriptor 3.
//! Allocates memory, writes a witness into it, and sends the descriptor back —
//! which is the whole of what a Wayland client does with `wl_shm`, minus the
//! drawing.

use quark_rt::manifest::CapReq;
use quark_rt::{println, syscall};

quark_rt::manifest!([CapReq::phys_alloc(16)]);

const CONN: usize = 3;
const MINE: usize = 0x97_0000_0000;
pub const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    // Wait for the parent's byte before answering, so the test proves the
    // stream carries data in both directions between address spaces.
    let mut buf = [0u8; 8];
    let n = syscall::sys_fd_read(CONN, &mut buf);
    if n != 4 || &buf[..4] != b"go!\n" {
        println!("[dchild] bad greeting: {} bytes", n);
        syscall::sys_exit_code(2);
    }

    let Ok(mem) = syscall::sys_memfd_create(2) else {
        println!("[dchild] no memory");
        syscall::sys_exit_code(3);
    };
    if syscall::sys_mmap_fd(mem, MINE).is_err() {
        println!("[dchild] cannot map my own memory");
        syscall::sys_exit_code(4);
    }
    unsafe { core::ptr::write_volatile(MINE as *mut u64, WITNESS) };

    if syscall::sys_fd_send(CONN, b"here", Some(mem)) != Ok(4) {
        println!("[dchild] send failed");
        syscall::sys_exit_code(5);
    }
    println!("[dchild] sent");
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[dchild] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
```

Register it in `Makefile` the way Task 1 registered `dtest`: `DCHILD_DIR`,
`DCHILD_ELF`, a `FORCE` rule, an entry in `user:`, and `dchild:DCHILD` in
`USR_PROGRAMS`.

Add to `user/dtest/src/main.rs`. The spawn is modelled on `user/wm`'s
`start_session` — read the image through the VFS, `spawn::load`, grant, start:

```rust
use quark_rt::{nameserver, spawn, vfs};

const CHILD_IMAGE: usize = 0x98_0000_0000;
const THEIR_MEM: usize = 0x99_0000_0000;
const WITNESS: u64 = 0x0D15_EA5E_D15C_0DE5;

static SPAWN_SCRATCH: spawn::Scratch = spawn::Scratch {
    elf: 0x9A_0000_0000,
    stack: 0x9B_0000_0000,
    args: 0x9C_0000_0000,
};

/// Read `/usr/bin/dchild` and start it. Returns its TID.
fn start_child() -> Option<spawn::Spawned> {
    let vfs_tid = nameserver::lookup_retry(b"vfs", 20)?;
    // Lowercase for ext2, uppercase with .ELF for FAT32 — the two spellings
    // the shell already tries.
    let (handle, size, _) = match vfs::open(vfs_tid, b"/usr/bin/dchild") {
        Ok(h) => h,
        Err(_) => vfs::open(vfs_tid, b"/usr/bin/DCHILD.ELF").ok()?,
    };
    let size = size as usize;
    let pages = (size + 4095) / 4096;
    for p in 0..pages {
        let frame = syscall::sys_phys_alloc(1).ok()?;
        syscall::sys_map_phys(frame, CHILD_IMAGE + p * 4096, 1).ok()?;
        let want = 4096.min(size - p * 4096) as u32;
        vfs::read(vfs_tid, handle, frame, (p * 4096) as u32, want).ok()?;
    }
    let _ = vfs::close(vfs_tid, handle);

    let image = unsafe { core::slice::from_raw_parts(CHILD_IMAGE as *const u8, size) };
    let info = spawn::load(image, &SPAWN_SCRATCH).ok()?;
    quark_rt::manifest::grant_image(info.tid, image, 12);
    // It needs to be able to answer us and to reach the nameserver.
    let _ = syscall::sys_cap_grant(info.tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);
    // And somewhere for its println to go.
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);
    Some(info)
}

fn test_across_address_spaces() {
    println!("across address spaces:");
    let (mine, theirs) = match syscall::sys_socketpair() {
        Ok(p) => p,
        Err(()) => { check("a pair", false); return; }
    };
    let Some(info) = start_child() else {
        check("start /usr/bin/dchild", false);
        return;
    };
    check("start /usr/bin/dchild", true);

    // Hand the child its end, then drop ours. If an end were a flag rather
    // than a count, this would tell the child's peer the end had gone.
    check("give the child descriptor 3", syscall::sys_fd_dup(info.tid, 3, theirs).is_ok());
    check("drop our copy of it", syscall::sys_fd_close(theirs).is_ok());
    check("the stream is still alive", syscall::sys_fd_write(mine, b"go!\n") == 4);

    if info.start().is_err() {
        check("the child runs", false);
        return;
    }
    check("the child runs", true);

    // Wait for its answer with the set, which is what makes this the whole
    // phase and not three quarters of it.
    let set = match syscall::sys_pollset_create() {
        Ok(s) => s,
        Err(()) => { check("a set to wait on", false); return; }
    };
    let _ = syscall::sys_pollset_add(set, mine, syscall::POLL_READABLE, 7);
    let mut ready = [syscall::Ready::empty(); 2];
    check(
        "the set wakes for the child's reply",
        syscall::sys_pollset_wait(set, &mut ready, 500) == Ok(1) && ready[0].token == 7,
    );

    let mut buf = [0u8; 8];
    let got = syscall::sys_fd_recv(mine, &mut buf, Some(25));
    check("bytes and a descriptor arrived", got == Ok((4, true)));
    check("map memory the other task allocated", syscall::sys_mmap_fd(25, THEIR_MEM).is_ok());
    check(
        "and read what it wrote there",
        unsafe { core::ptr::read_volatile(THEIR_MEM as *const u64) } == WITNESS,
    );

    let _ = syscall::sys_fd_close(25);
    let _ = syscall::sys_fd_close(set);
    let _ = syscall::sys_fd_close(mine);
}
```

Call it last from `_start`, before the summary.

- [ ] **Step 2: Run it to verify it fails**

Build, boot, run `dtest`.
Expected: FAIL at `start /usr/bin/dchild`, because `dchild` is not in the image
until the Makefile registration and `make hd` have both happened. Once it is,
the first real failure to expect is `map memory the other task allocated` —
that is the one that needs `add_access` on receive, which Task 6 wired.

- [ ] **Step 3: Write the minimal implementation**

There should be nothing to write. Every mechanism this exercises was built in
Tasks 1–12; the work here is finding what does not survive contact with a
second address space. The likely three, in the order they will bite:

1. **A reference not taken.** `SYS_FD_DUP` must call `pipe::retain_fd`, which
   for a `StreamEnd` is `stream::retain_end` — otherwise `sys_fd_close(theirs)`
   in the parent takes the end away from the child.
2. **A region the receiver cannot map.** `SYS_FD_RECV` must call
   `shmem::add_access(handle, tid)` for a `MemFd`, or the child's memory is
   refused to the parent that was just handed it.
3. **A wake to the wrong task.** `pollset::note_pipe` looks up watches by the
   *set owner's* descriptor table. A set owned by the parent watching a stream
   the child writes to must resolve through the parent's table, not the
   writer's.

Fix what fails, in the file that owns it, and add nothing new.

- [ ] **Step 4: Run it to verify it passes**

Build, `make hd`, boot, run `dtest`.
Expected: ten more `ok` lines, `[dchild] sent` on the console before them, and
`0 failed` for every section.

- [ ] **Step 5: Commit**

```bash
git add user/dchild user/dtest/src/main.rs Makefile
git commit -m "$(cat <<'EOF'
Prove it between two address spaces

Everything up to here was tested inside one process, where a descriptor is a
number in one table and a region is mapped once. All of it exists to be used
between two tasks, and the bugs that only appear there are the ones this would
otherwise have shipped: a reference not taken across sys_fd_dup, a region the
receiver was never admitted to, a wake resolved through the wrong task's
descriptor table.

dchild is handed one end of a socketpair at descriptor 3, allocates memory,
writes a witness into it and sends the descriptor back. That is what a Wayland
client does with wl_shm, minus the drawing.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>
EOF
)"
```

---

## Acceptance

Phase 10 is done when `dtest` reports `0 failed` on a booted machine and the
roadmap's own criterion holds:

> two tasks hold a socketpair, one sends a descriptor for a region of memory
> across it, the other maps it and reads what the first wrote, a third task
> waits on a descriptor set holding both ends and wakes for exactly the one
> that became ready, and a program started with `FOO=bar` in its environment
> can read it back with `getenv`.

Tasks 1–12 cover every clause within one process; **Task 13 is the same thing
between two**, and it is where a bug in any of this would actually hide.

## Notes for whoever executes this

- **`make` builds the tree; `make hd` in `../explosion` assembles the image.**
  Only `make run` passes `-serial stdio`; for scripted runs use
  `-serial file:` and drive the console over QMP.
- **`tools/check-abi.sh` runs first in every build** and fails on any
  disagreement between `src/syscall.rs` and `user/quark-rt/src/syscall.rs`. If
  the build stops with `abi: MISMATCH`, a number went into one file only.
- **Interrupts and locks:** every table in this plan is guarded the way
  `src/shmem.rs` and `src/pipe.rs` already guard theirs — `irq_save` around
  the critical section, and never holding it across `pmm::alloc`, which
  re-enables interrupts on release.
- **Do not hold a `UserAccess` guard across a block or a yield.** RFLAGS.AC
  travels with the task's saved flags, so a guard held across a reschedule
  leaves the SMAP window open in whatever runs next.
- **There is no `sleep` in the kernel.** A task sleeps by calling
  `ipc::sys_recv_timeout` on its own TID: nobody can send to that, so only the
  deadline ends it, and `quark_rt::syscall::sleep_ticks` is exactly this.
  Reuse it rather than adding a second timeout sweep.
- **`scheduler::get_task_mut` is an `unsafe fn`** returning a `&'static mut`
  into the task table. Every call sits in an `unsafe` block, and the reference
  must not be held across anything that could reschedule.
- **`validate_user_ptr{,_mut}` are private to `src/syscall.rs`**, not in
  `paging`. Inside a dispatch arm, call them unqualified.
- If a task's test passes on the first run before the implementation exists,
  the test is wrong. Say so and fix the test rather than moving on.
