# Phase 15 — `fork`, `exec`, a pty, and a terminal on Quark

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task by task, inline, on `main`. Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `weston-terminal`, built from its own source and unpatched, runs under
`wm` with `qsh` in it — which means Quark grows `fork`, `execve`, a
pseudo-terminal and the parts of a process a Unix program assumes.

**Architecture:** Three holes, filled in the order a program falls into them.
`fork` is a kernel call: the child is a task in a copy of the caller's address
space that returns 0 from the same syscall, which needs the whole user register
frame rather than the six registers a syscall returns through. `execve` is a
kernel call over a job user space does: the C layer loads the ELF into an
address space it made, and the kernel swaps the calling task into it — Quark
already loads programs in user space and this keeps it that way. A pty is a new
kind of kernel descriptor, because a pipe already is one and the compositor
`epoll`s the master: readiness the kernel cannot see is readiness `epoll`
cannot report.

**Tech stack:** Rust (`no_std`) for the kernel, `user/qsh` and `quark-rt`; C for
`user/linux-abi` and the ported clients; weston 13.0.0's `clients/window.c`
toytoolkit and `clients/terminal.c`, built with the existing cross toolchain.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 15 — `weston-terminal`". Its
acceptance sentence is: *done when a terminal window is open on Quark with
`qsh` running in it, and text can be selected with the mouse and pasted into
another client.*

---

## The decision the roadmap asked for

The roadmap says to choose between a pty server with `spawn`, and `fork` and
`exec` for real, **from a spike rather than from taste**. The spike:

- `clients/terminal.c:3091` calls `forkpty(&master, NULL, NULL, NULL)` and the
  child calls `execl(path, path, NULL)` at line 3103. musl's `forkpty` is
  `openpty` + `fork` + `login_tty`, and `login_tty` is `setsid`, `ioctl
  TIOCSCTTY` and three `dup2`s.
- Between the fork and the exec the child touches only `close`, `read` on a
  pipe, and `setenv`. It opens no file and makes no Wayland call. So the child
  needs the *kernel's* descriptors to survive the fork, and nothing else.
- `terminal.c` is a tool, not an example: it is built from the toytoolkit
  (`clients/window.c`) like every other weston client, and nothing about it is
  a demonstration of `fork` that could reasonably be written another way.

So the pty-server route means patching `terminal.c`, and this tree does not
patch upstream clients — the rule that has held since Phase 8 is that if an
upstream client needs changing to work, the bug is here. **Decision: `fork` and
`execve` for real.** It is also what makes Phase 16 plausible, since a toolkit
that cannot start a subprocess is a toolkit with a class of application
missing.

Two things the decision does *not* buy, and they are written down as gaps
rather than smuggled in:

- **Copy-on-write.** `fork` copies every page the caller owns, eagerly. The
  immediate use is fork-then-exec, where copy-on-write would save all of the
  copying and none of the correctness; the frames have no reference counts and
  giving them some is its own change. A 20 MiB program forks in a few
  milliseconds, which is the whole cost.
- **Open files across `fork` and `exec`.** The kernel's descriptors — pipes,
  streams, ptys, shared memory — are the task's and are copied to the child and
  kept across an exec. The C layer's *VFS* files live in the program's own
  memory and are named by its address space, so a forked child inherits the
  numbers but not the server's permission to use them, and an exec starts with
  none. `weston-terminal` needs neither. Unix programs that pass an open file
  to a child through `fork` do not work yet, and that is the next piece of this
  hole.

## Global constraints

- Inline execution on `main`, a commit per task, pushed. No branches.
- Every task is verified by booting the image (`../explosion/tools/boot-test.sh`)
  and reading the screen; user-space output does not reach serial.
- **Nothing here patches an upstream client.** weston's own sources are built
  as they are; what is missing is added to Quark.
- New syscalls take free numbers in their subsystem's block and go in
  `docs/abi.md` with an ABI minor bump in the same commit;
  `tools/check-abi.sh` fails the build otherwise.
- The kernel invariants in `quark/CLAUDE.md` hold. In particular: user mappings
  live at or above `paging::USER_MIN_ADDR`, `paging::OWNED` decides what may be
  freed, and an owned frame is mapped in exactly one place — which is what
  makes an eager copy the honest first version.

## File structure

- `quark/src/syscall.rs` — `SYS_FORK`, `SYS_EXEC_SPACE`, `SYS_PTY_CREATE`, and
  the frame pointer the entry stub now records.
- `quark/src/scheduler.rs` — making a task that resumes from a saved user
  frame, and the exec that replaces a task's address space under it.
- `quark/src/paging.rs` — `copy_address_space`: a walk of the user half that
  copies what the caller owns and shares what it does not.
- `quark/src/task.rs` — `UserFrame` on the task, and `FdKind::PtyEnd`.
- `quark/src/pty.rs` (new) — pty pairs: two buffers, a line discipline and a
  window size.
- `quark/user/linux-abi/src/process.c` (new) — `fork`, `execve` and the ELF
  loading an exec needs, in C.
- `quark/user/linux-abi/src/pty.c` (new) — `openpty`'s half of the story:
  `/dev/ptmx`, `ptsname`, the `ioctl`s a terminal uses.
- `quark/user/linux-abi/src/syscall.c` — the numbers that reach them, and
  `timerfd`.
- `quark/user/quark-rt/src/console.rs` — stdout is descriptor 1 when there is
  one.
- `explosion/toolchain/build-weston-toytoolkit.sh` (new) — weston's `shared/`
  and `clients/window.c` as a library, then `weston-terminal`.

---

### Task 1: A task can fork

**Files:**
- Modify: `quark/src/syscall.rs` (the entry stub records the frame; `SYS_FORK`)
- Modify: `quark/src/task.rs` (`UserFrame`)
- Modify: `quark/src/scheduler.rs` (`fork_task`)
- Modify: `quark/src/paging.rs` (`copy_address_space`)
- Modify: `quark/user/linux-abi/src/syscall.c` (`LX_clone` without `CLONE_VM`,
  `LX_wait4`)
- Create: `explosion/toolchain/forktest.c`

**Interfaces:**
- Produces: `SYS_FORK` (110), no arguments, returning the child's TID to the
  parent and 0 to the child, or `u64::MAX` when there is no room. The child is
  a task of a new address space holding a copy of every page the caller owns, a
  share of every page it does not, and a copy of its descriptors, capabilities
  and band.
- Produces: `paging::copy_address_space(src_pml4) -> Option<usize>`, which
  copies the user half — PML4[1] and PML4[255] — and nothing below
  `USER_MIN_ADDR`.
- Produces: `scheduler::fork_task(frame: &UserFrame) -> Option<usize>`.
- Produces: `wait4(pid, status, options, rusage)` in the C layer over
  `SYS_WAIT`, filling `*status` with `code << 8` so that `WIFEXITED` and
  `WEXITSTATUS` read it the way musl expects.

- [x] **Step 1: The failing check.** `explosion/toolchain/forktest.c`: a
  program that prints its pid, forks, prints "child" or "parent N", and has the
  parent `waitpid` for the child and print its status. Build it with the other
  C suite programs and run it. Expected: "fork: Function not implemented",
  because `__quark_clone` refuses anything without `CLONE_VM`.

- [x] **Step 2: The frame.** ~~The syscall entry stub gains one store~~ — it
  needs none. The stub sets RSP to the task's kernel stack top and pushes
  exactly these eleven words, so the frame of a task inside a system call is
  always at `top - size_of::<UserFrame>()`, with no per-syscall cost and no
  race over a per-CPU word that a preemption could overwrite. What did have to
  change is that `enter_user_inner` published the RSP it happened to be
  running on rather than the top, which is what the scheduler publishes on
  every switch; now both say the top.

```rust
/// What the entry stub pushed, in the order it pushed it. The stub is the
/// definition; this is the same list read as a structure, so a change to one
/// is a change to the other.
#[repr(C)]
pub struct UserFrame {
    pub rsi: u64,
    pub rdi: u64,
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}
```

- [x] **Step 3: The copy.** `copy_address_space` walks PML4[1] and PML4[255] of
  the source. For each present leaf: a page with `OWNED` is a fresh frame with
  the same bytes and the same flags; a page without it is mapped at the same
  physical address with the same flags, because it is shared memory, a device,
  or a memory object's page — and an entry naming an object takes a reference
  to it (`memobj::map_ref`) so the object is not released under the child. A
  non-present entry with `MARKER` is copied as it is: a reservation is a
  promise, and the child inherits the promise rather than the page. Tables are
  allocated as the walk needs them, and a failure part-way frees what it made.

- [x] **Step 4: The child.** `scheduler::fork_task` creates a task, gives it
  the copied address space, calls `inherit_from_creator` for descriptors,
  capabilities and band, copies `fs_base`, `mem_limit` and `parent_tid`, and
  plants the parent's `UserFrame` on the child's kernel stack with `rax = 0`.
  Its context starts at a trampoline that pops the frame and `iretq`s. The
  child is *not* given the parent's `clear_tid`: that word belongs to the
  parent's thread list.

- [x] **Step 5: The layer.** `SYS_fork` (57), `SYS_vfork` (58) and a `clone`
  without `CLONE_VM` become `SYS_FORK`, and `wait4` becomes `SYS_WAIT` with the
  status word rebuilt in Linux's shape. musl calls `SYS_fork` directly on
  x86_64 and reaches `__clone` only for threads, which is why adding the
  process case to `__quark_clone` alone changed nothing.

- [x] **Step 6: Verify.** `forktest` prints the parent's pid, then both halves,
  then the child's exit status; `dtest` still passes; `hello` still runs, which
  is the check that the syscall stub's extra store broke nothing. Run
  `dtest calls`, which makes three million calls, and compare the time with the
  number in `docs/` — a store per syscall should not be visible, and if it is,
  that is worth knowing before everything else is built on it.

- [x] **Step 7: Commit.** quark: "A task can fork".

---

### Task 2: A task can exec

**Files:**
- Modify: `quark/src/syscall.rs` (`SYS_EXEC_SPACE`), `quark/src/scheduler.rs`
  (`exec_into`)
- Create: `quark/user/linux-abi/src/process.c` (the ELF loading `execve` needs)
- Modify: `quark/user/linux-abi/src/syscall.c` (`LX_execve`)
- Create: `explosion/toolchain/exectest.c`

**Interfaces:**
- Produces: `SYS_EXEC_SPACE` (111), `arg0 = cr3` of an address space the caller
  made, `arg1 = entry`, `arg2 = rsp`. The calling task keeps its TID, its
  descriptors, its capabilities and its parent, and continues in the new space
  at `entry`. The old space is freed. It does not return. The task's *space id*
  is the new space's, so servers see the program it has become rather than the
  one it was.
- Produces: descriptors are released when a task *dies* rather than when it is
  reaped. A dead task keeps its memory until its parent collects it, which is
  deliberate; a descriptor is something another task can be waiting on, and the
  oldest idiom there is — a child writes down a pipe and exits while its parent
  reads to the end — deadlocked on it.
- Produces: a descriptor sent over a stream stays deliverable after the sender
  closes its end. `close_end` discarded the queue bound for the *peer* instead
  of the one bound for itself, which nothing noticed while senders lingered
  until reaping.
- Produces: `execve(path, argv, envp)` in the C layer: read the ELF through the
  VFS, make an address space, give it the segments and a stack carrying
  `argv`/`envp` and the program's own header table at `PHDRS_AT`
  (`user/libc/include/quark/layout.h`), then `SYS_EXEC_SPACE`. On any failure
  before the last call it returns an error and the caller is untouched, which
  is what `execl` failing has to mean.

- [x] **Step 1: The failing check.** `exectest.c` prints "before", execs
  `/usr/bin/echo` with an argument, and prints "exec failed" if it returns.
  Expected: "exec failed: Function not implemented".

- [x] **Step 2: The kernel half.** `exec_into(cr3, entry, rsp)`: check the
  caller created that address space, take the old `cr3`, put the task in the
  new one, move the space id across, reset `fs_base` to 0 (the new image has
  not set a thread pointer), keep `fds`, `caps`, `cspace`, `parent_tid`, then
  enter user mode at `entry` with `rsp`. Free the old address space *after* the
  switch, since the call is running on its stack until then.

- [x] **Step 3: The loader.** `process.c` reads the ELF header and program
  headers, maps each `PT_LOAD` segment into scratch pages of its own, zeroes
  the tail between `p_filesz` and `p_memsz`, and `SYS_ADDRSPACE_GIVE`s them to
  the new space at `p_vaddr`. The stack is one more give, built like
  `quark_rt::spawn` builds one: `argc`, `argv`, `envp`, the auxiliary vector
  with `AT_PHDR`, `AT_PHENT`, `AT_PHNUM` and `AT_ENTRY`, and the argument page
  with the program headers copied to `PHDRS_AT`.

- [x] **Step 4: Verify.** `exectest` prints "before" and then `echo`'s
  argument; its pid is unchanged across the exec, which it prints either side
  to show. `forktest` still passes. A failing exec — a path that is not there,
  a file that is not an ELF — returns and the program carries on.

- [x] **Step 5: Commit.** quark: "A task can exec".

---

### Task 3: A pseudo-terminal

**Files:**
- Create: `quark/src/pty.rs`
- Modify: `quark/src/task.rs` (`FdKind::PtyEnd`), `quark/src/syscall.rs`
  (`SYS_PTY_CREATE`, and the read/write/poll paths), `quark/src/pollset.rs`
- Create: `explosion/toolchain/ptytest.c`

**Interfaces:**
- Produces: `SYS_PTY_CREATE` (208), a descriptor for a new pty's master, and
  `SYS_PTY_OPEN` (210), one for its slave by number — which is the shape
  `openpty` has: open the multiplexer, ask which pty it gave you, open that
  one. `SYS_PTY_CTL` (209) carries the `termios`, the window size and the
  number. Both ends are ordinary descriptors: inherited, closed, `dup2`ed and
  polled like any other.
- Produces: a pty is two 4 KiB rings and a `termios`. What is written to the
  master is input to the slave; what is written to the slave is output on the
  master. With `ECHO` set, input is copied back to the master as it arrives.
  With `ICANON` set, a slave's read waits for a newline and backspace erases;
  with it clear, a read takes whatever is there.
- Produces: readiness the kernel knows, so `poll` and `epoll` work on both ends
  without asking anybody.
- Produces: a window size, set and got through the layer's `ioctl`, which
  nothing in the kernel interprets — it is a fact a terminal stores for the
  program in it to read.

- [x] **Step 1: The failing check.** `ptytest.c` calls `openpty` and prints the
  two descriptors. Expected: "openpty: No such file or directory", because
  `/dev/ptmx` is not there.

- [x] **Step 2: The kernel's pty.** `pty.rs`, modelled on `pipe.rs`: a table of
  pairs, each with two rings, waiter lists for both directions, a `termios`, a
  `winsize`, and reference counts for the two ends. Closing the last master
  makes reads on the slave return end-of-file, and the other way round.

- [x] **Step 3: The line discipline.** On a write to the master: if `ECHO`,
  copy to the output ring; if `ICANON`, hold the bytes in a line buffer and
  release them to the slave's readable ring at `\n`, with `\b` and `\x7f`
  taking one back and un-echoing it. `\r` becomes `\n` when `ICRNL` is set,
  which is what a terminal sends for Return. `ONLCR` turns a slave's `\n` into
  `\r\n` on the way out, which is what makes a terminal's cursor return to the
  left.

- [x] **Step 4: Verify.** `ptytest` writes "hi\n" to the master and reads it
  from the slave; writes "there\n" to the slave and reads it from the master,
  seeing `\r\n`; turns `ECHO` off and sees nothing come back; polls both ends
  and gets the readiness it expects. `dtest` still passes.

- [x] **Step 5: Commit.** quark: "A pseudo-terminal".

---

### Task 4: `forkpty` works

**Files:**
- Create: `quark/user/linux-abi/src/pty.c`
- Modify: `quark/user/linux-abi/src/syscall.c` (`LX_ioctl` for the terminal's
  requests, `LX_setsid`), `quark/user/linux-abi/src/files.c` (`/dev/ptmx` and
  `/dev/pts/N`)

**Interfaces:**
- Produces: `openat("/dev/ptmx")` makes a pair and answers with the master;
  `TIOCGPTN` names it; `openat("/dev/pts/N")` answers with the slave the same
  pair already made. `TIOCSPTLCK` is accepted and does nothing, because nothing
  here can open a slave it was not handed.
- Produces: `ioctl` answers `TCGETS`, `TCSETS`/`TCSETSW`/`TCSETSF`,
  `TIOCGWINSZ`, `TIOCSWINSZ` and `TIOCSCTTY`; everything else is still
  `ENOTTY`, which is what it should be for a program asking a pipe about its
  window.
- Produces: `setsid` returns the caller's own id rather than an error, because
  there are no sessions here and a terminal only wants to know it succeeded.

- [x] **Step 1: The failing check.** `ptytest` gains a second half: `forkpty`,
  the child writes a line and exits, the parent reads it. Expected: it stops at
  `openpty` as before, or at `login_tty`.

- [x] **Step 2: The paths.** `/dev/ptmx` and `/dev/pts/N` in the C layer's
  open path, ahead of the VFS, the way `/dev/null` and `/dev/random` already
  are. A pty's number is its kernel table index, which is what `TIOCGPTN`
  answers and what the slave's path names.

- [x] **Step 3: Verify.** `ptytest`'s second half prints the line the child
  wrote, and the child's exit status. The parent's `read` returns 0 when the
  child has gone, which is what tells a terminal its shell has exited.

- [x] **Step 4: Commit.** quark: "forkpty".

---

### Task 5: `timerfd`

**Files:**
- Modify: `quark/src/task.rs` (`FdKind::Timer`), `quark/src/syscall.rs`
  (`SYS_TIMER_CREATE`, `SYS_TIMER_SET`), `quark/src/pollset.rs`
- Modify: `quark/user/linux-abi/src/syscall.c` (`LX_timerfd_create`,
  `LX_timerfd_settime`, `LX_timerfd_gettime`)

**Interfaces:**
- Produces: a descriptor that becomes readable when its deadline passes and
  reads as a `u64` count of expirations, which is what `window.c:7007` makes
  and what the toytoolkit's repeat and blink are built on.
- Produces: one-shot and interval timers, in the PIT's ten-millisecond steps,
  which is the resolution this machine has.

- [x] **Step 1: The failing check.** A C test that makes a timerfd for 200 ms,
  polls it, and prints how long it waited. Expected: `timerfd_create` fails
  with `ENOSYS`.

- [x] **Step 2: The timer.** A table of timers, each a deadline in ticks and an
  interval; the tick handler makes expired ones readable and wakes their
  waiters. A read returns the count and clears it, blocking when there is
  nothing unless the descriptor is non-blocking.

- [x] **Step 3: Verify.** The test waits about 200 ms and reads 1; an interval
  timer polled twice reads 1 each time; a timer that has expired twice before
  it is read reads 2.

- [x] **Step 4: Commit.** quark: "timerfd".

---

### Task 6: A program's output goes to its stdout

**Files:**
- Modify: `quark/user/quark-rt/src/console.rs`
- Modify: `quark/user/qsh/src/main.rs` (only if it needs the prompt flushed
  differently)

**Interfaces:**
- Produces: `console_write` writes to descriptor 1 when the task has one, and
  looks the console service up only when it has not. A program started with its
  standard output on a pty writes into the pty; one started the way `init`
  starts programs writes to the console exactly as before, because that is what
  `init` wires descriptor 1 to.

- [x] **Step 1: The failing check.** ~~A key script that runs `qsh` with its
  descriptors on a pty~~ — folded into Task 7, which needs the shell staged at
  `/bin/sh` before it can be run in a terminal at all. What this task checks
  on its own is that output still lands where it did: descriptor 1 is what a
  program writes to when there is one, and the console service is where a
  program with no descriptor ends up.

- [x] **Step 2: Prefer the descriptor.** `console_write` tries
  `sys_fd_write(1)` first and falls back to the nameserver lookup when the
  descriptor is not connected. The fallback stays: a driver started before the
  console exists still has to be able to say something.

- [x] **Step 3: Verify.** The harness reads `qsh`'s prompt off the pty master.
  Every other program still prints where it did: boot the image and read the
  console, `runtests /etc/libc.tests`, `dtest`.

- [x] **Step 4: Commit.** quark: "A program's output goes to its stdout".

---

### Task 7: `/bin/sh`

**Files:**
- Modify: `explosion/Makefile` (stage `qsh` as `/bin/sh`)
- Modify: `quark/user/qsh/src/main.rs` (read a line from a descriptor that is
  not the input server)

**Interfaces:**
- Produces: `/bin/sh` is `qsh`. `weston-terminal` execs `getenv("SHELL")` or
  `/bin/sh`, and there has to be something at that path for the terminal to
  have anything in it.
- Produces: `qsh` reading from a pty: it already reads descriptor 0 with
  `sys_fd_read`, so what this needs is for it not to assume the input server's
  line discipline — a pty in canonical mode gives it whole lines, and one in
  raw mode gives it bytes.
- Produces: a spawner needs no authority over anybody to build a child. A task
  the caller created and has not started is its own to fill — nobody else can
  name it, it holds nothing, and it cannot run — so `SYS_TASK_CREATE_IN`,
  `SYS_TASK_START`, `SYS_FD_DUP`, `SYS_PIPE_FD_SET` and `SYS_CAP_GRANT` accept
  that window without `TaskMgmt`, bounded the way threads are. It is strictly
  less than `fork`, which hands a child every capability the caller holds and
  asks for nothing at all.

- [x] **Step 1: The failing check.** `ptytest qsh`: fork a pty, exec
  `/bin/sh`, write "echo hello\n" to the master, read what comes back.
  Expected: exec fails, because `/bin/sh` is not a path in the image.

- [x] **Step 2: Stage it.** The image gets `/bin/sh`. Whether that is a copy or
  a symbolic link is decided by what the image's filesystem supports: ext2 and
  ext4 have links, FAT32 has none, so a copy is what works on all three.

- [x] **Step 3: Verify.** `ptytest qsh` reads the prompt, writes a command,
  reads its output, and sees the shell exit when the master closes.

- [x] **Step 4: Commit.** quark: "/bin/sh"; explosion: "Stage the shell where a
  terminal will look for it".

---

### Task 8: weston's toytoolkit builds for Quark

**Files:**
- Create: `explosion/toolchain/build-weston-toytoolkit.sh`
- Modify: `explosion/toolchain/build-weston-client.sh` (the protocol stubs the
  toytoolkit needs)

**Interfaces:**
- Produces: `libtoytoolkit.a` — `clients/window.c` and the pieces of weston's
  `shared/` it uses (`os-compatibility.c`, `xalloc.c`, `file-util.c`,
  `matrix.c`, `config-parser.c`), built against Quark's musl, cairo, xkbcommon
  and libwayland, with the protocol stubs generated from the same XML weston
  generates them from.
- Produces: whatever the build finds missing, added to Quark rather than
  patched out of weston. Expect `signalfd`, `sigaction`, `getpwuid` and
  `realpath` to be where it stops.

- [ ] **Step 1: The failing check.** Write the script, run it, and read the
  first error. Expected: a list of missing pieces rather than one.

- [ ] **Step 2: Close them one at a time**, each as its own small change to
  `user/linux-abi` or `user/libc`, with a note in the commit of what wanted it.
  A function that cannot be made to work honestly — `getpwuid` on a system with
  no password file — returns the failure the caller is required to handle
  rather than a fiction.

- [ ] **Step 3: Verify.** `libtoytoolkit.a` links. `weston-simple-shm` still
  builds and runs, which is the check that nothing added for the toolkit broke
  the client that was already working.

- [ ] **Step 4: Commit.** explosion: "The toytoolkit builds for Quark"; quark:
  whatever the layer needed.

---

### Task 9: A terminal, with a shell in it

**Files:**
- Modify: `explosion/toolchain/build-weston-toytoolkit.sh` (build
  `weston-terminal`)
- Modify: `explosion/Makefile` (stage it)

**Interfaces:**
- Produces: `wm weston-terminal` — a window with `qsh` running in it, keys
  reaching the shell and its output drawn in the window.

- [ ] **Step 1: The failing check.** Build and run it. Expected: it starts,
  and stops somewhere — the first stop is the interesting output of this step,
  not a failure of it.

- [ ] **Step 2: Whatever it is.** Each thing it stops at is a fix in Quark: a
  missing `ioctl`, a `poll` that does not report a pty, an `epoll` that cannot
  hold a timerfd, a `wl_shm_pool.resize` the compositor refuses. Keep a list in
  the commit message; that list is the real content of this phase.

- [ ] **Step 3: Verify.** A key script: `wm weston-terminal`, wait, type
  `echo hello`, Return, screenshot. The screenshot shows the command and its
  output in the window. `ls /` in it lists the root.

- [ ] **Step 4: Commit.** quark and explosion: "A terminal, with a shell in
  it".

---

### Task 10: Selecting and pasting

**Files:**
- Modify: `quark/user/wm/src/*` as the terminal's use of the protocol requires

**Interfaces:**
- Produces: dragging across the terminal's text selects it, and the selection
  is on the clipboard: `wlclip paste` in the same session prints it.

- [ ] **Step 1: The failing check.** `wm weston-terminal "wlclip paste"`: type
  something in the terminal, select it with the pointer, and see what `wlclip`
  reads. Expected: nothing, or whatever is wrong.

- [ ] **Step 2: Fix what it finds.** The terminal sets the selection with
  `wl_data_device.set_selection`, which `wm` has; what it needs that a test
  client did not is likely the serial from a real press and a
  `wl_data_source.send` while its own event loop is elsewhere.

- [ ] **Step 3: Verify.** The text typed into the terminal comes out of
  `wlclip paste`, and the primary selection too if the terminal sets one.

- [ ] **Step 4: Commit.** quark: "Selecting and pasting from the terminal".

---

### Task 11: Write it down

- [ ] `quark/CLAUDE.md`: what a process is here now — `fork` copies eagerly and
  what that does not share, `exec` keeps the task and its descriptors and
  changes the program, a pty is a kernel descriptor and why, and the gaps this
  phase leaves.
- [ ] `quark/docs/abi.md`: the new calls, with the ABI minor bumped once for
  the phase.
- [ ] `~/src/osdev/ROADMAP.md`: Phase 15 done, the decision it turned on and
  why, what it took, and what is left.
- [ ] Tick this plan; commit and push quark and explosion.

---

## Acceptance

`wm weston-terminal` opens a window with `qsh` in it; typing `echo hello` and
Return prints `hello` in the window; `ls /` lists the root; text dragged over
with the pointer can be pasted into another client with `wlclip paste`. The
terminal exits when the shell does, and the session ends with it. `dtest`, the
C library's suite, the font suites and the hostile-argument sweep still pass on
ext2 and ext4, `e2fsck` is clean, twenty `wlfuzz` seeds still leave the
compositor standing, and nothing faults on serial.
