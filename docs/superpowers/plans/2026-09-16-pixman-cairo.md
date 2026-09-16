# pixman and cairo implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.
> The user prefers inline execution to subagents.

**Goal:** A Wayland client on Quark draws with cairo, and pixman's own test
suite passes on Quark in full.

**Architecture:** Two ports on top of the musl toolchain — pixman with its
generic C path, then cairo with the image backend only — each built by a script
in `explosion/toolchain` and installed into the musl sysroot. What stood in the
way was not the ports: a spike running pixman's test suite on Quark found five
platform bugs, four already fixed in the working tree when this plan was
written. Those are Tasks 1–5, recorded as tasks so that each lands as its own
commit with its own proof. Tasks 6–9 are the ports; Task 10 writes it down.

**Tech stack:** pixman 0.44.2, cairo 1.18.4, meson 1.12 and ninja from
`~/.local/bin`, `x86_64-quark-musl-gcc` (gcc 15.2.0), musl 1.2.5, the Quark
kernel and `quark-rt`.

**Spec:** `ROADMAP.md` Phase 11 (at `~/src/osdev/ROADMAP.md`).

## Global constraints

- Sources live in `~/opt/src` (`pixman-0.44.2`, `cairo-1.18.4`), which survives
  a reboot. Nothing goes in `/tmp` that a later task depends on.
- Meson builds pass `-Ddefault_library=static -Db_staticpic=false`. The second
  is not a preference; `build-wayland.sh` says why.
- pixman's SIMD paths (`mmx`, `sse2`, `ssse3` and the rest) stay **disabled**:
  the generic C path is the port, and enabling SIMD is a separate, measurable
  change.
- cairo is the **image backend only**. Every font, PNG, PDF/PS/SVG, X and
  script feature is off. Text arrives in Phase 13 with freetype.
- A musl program links `liblinux-abi.a` by path through the specs file, so
  ninja cannot see it change. After any change to `user/linux-abi`, relink by
  deleting the executables and running ninja again.
- Verification is boot-in-QEMU: `explosion/tools/boot-test.sh <keys> <ppm>`,
  where `<keys>` is a script of `sleep`/`type`/`key`/`move`/`click`/`shot`/
  `quit` lines. User-space output goes to the framebuffer, so the screendump is
  the result; serial carries only kernel messages, including `[UFAULT ...]`.
- `docs/abi.md` is the ABI contract; a change to what a spawner puts on the
  argument page or what `SYS_WAIT` reports is documented there.
- Commits end with the attribution lines the session supplies.

---

### Task 1: The compiler's own headers are on the include path

**Files:**
- Modify: `explosion/toolchain/build-musl.sh` (the specs heredoc)
- Modify: `~/opt/cross/x86_64-quark/musl/lib/musl-quark.specs` (installed copy)

**Interfaces:** none.

The specs file's `-nostdinc` removed gcc's private include directory and never
put it back, so `xmmintrin.h`, `cpuid.h` and `stdatomic.h` could not be found.
musl-gcc on Linux lists `-isystem include%s` straight after musl's own
directory; ours had Quark's `quark/*.h` there instead. pixman's test PRNG was
the first thing to include an intrinsics header.

- [x] **Step 1: Reproduce**

`ninja -C build-quark` in pixman with `-Dtests=enabled` failed:
`utils-prng.c:31:10: fatal error: xmmintrin.h: No such file or directory`.

- [x] **Step 2: Fix both copies**

Order becomes musl's, then the compiler's, then Quark's platform headers:

```
*cpp_options:
-nostdinc -isystem $PREFIX/include -isystem include%s -isystem $QUARK_SRC/user/libc/include %(old_cpp_options)

*cc1:
%(cc1_cpu) -nostdinc -isystem $PREFIX/include -isystem include%s -isystem $QUARK_SRC/user/libc/include
```

- [x] **Step 3: Verify**

A file including `xmmintrin.h`, `cpuid.h` and `stdatomic.h` compiles, and
`echo '#include <stddef.h>' | x86_64-quark-musl-gcc -E -x c -` still resolves
`stddef.h` to musl's.

- [x] **Step 4: Commit** in `explosion`.

---

### Task 2: Each task has its own floating-point state

**Files:**
- Create: `quark/src/fpu.rs`
- Modify: `quark/src/task.rs` (`Task::fpu`), `quark/src/scheduler.rs`
  (both task literals, `switch_to`), `quark/src/main.rs` (`mod fpu`,
  `fpu::init()` after the heap)
- Test: `quark/user/dtest/src/main.rs` (`test_fpu`)

**Interfaces:**
- Produces: `fpu::FpuState` (512 bytes, `align(64)`), `fpu::init()`,
  `fpu::clean() -> FpuState`, `unsafe fpu::save(*mut FpuState)`,
  `unsafe fpu::restore(*const FpuState)`.

`CpuContext` saved only the callee-saved general registers. There was one set of
x87/SSE registers for every task, so a task preempted mid-calculation resumed
with another task's values — and could read them.

- [x] **Step 1: Write the failing test** — `test_fpu` in dtest: the main task
loads a pattern into all sixteen XMM registers and MXCSR with `asm!`, releases
a worker that loads a different pattern, blocks until it has, and checks its
own pattern survived. dtest is soft-float, so no Rust statement touches those
registers and the result is deterministic.

- [x] **Step 2: Run it** — before the fix: `FAIL every SSE register survives
another task using them`, `FAIL and so does MXCSR`, `FAIL none of them is the
other task's`; dtest `128 passed, 3 failed`.

- [x] **Step 3: Implement** — `fpu::init()` runs `fninit`, loads MXCSR `0x1F80`
and captures the clean state with `fxsave64`. Every task is created with
`fpu: crate::fpu::clean()`. In `switch_to`, before `context_switch`:

```rust
crate::fpu::save(&raw mut TASKS[current_tid].as_mut().unwrap().fpu);
crate::fpu::restore(&raw const TASKS[next_tid].as_ref().unwrap().fpu);
```

Loading the incoming task's state before the switch is safe because the kernel
is soft-float, and it means a task that has never run starts clean without its
entry trampoline knowing. FXSAVE is only enough while CR4.OSXSAVE is clear;
`fpu.rs` says so.

- [x] **Step 4: Verify** — dtest `131 passed, 0 failed`.

- [x] **Step 5: Commit** in `quark`.

---

### Task 3: A fault kills the program, not the machine

**Files:**
- Modify: `quark/src/idt.rs` (`exception_handler`, `signal_for`)
- Modify: `quark/user/qsh/src/main.rs` (`set_status`)
- Modify: `quark/docs/abi.md` (`SYS_WAIT`)

**Interfaces:**
- Produces: a task killed by an exception exits with `-signal`, using Linux's
  numbers: SIGILL 4, SIGTRAP 5, SIGBUS 7, SIGFPE 8, SIGSEGV 11.

Every user-mode exception other than a page fault fell through to the kernel's
fatal path and halted the machine. musl's `abort()` executes a privileged `hlt`
on purpose, so a failed `assert` in any C program stopped everything. The
page-fault kill used `scheduler::exit()`, which reports success.

- [x] **Step 1: Reproduce** — `region-test` aborted; the serial log showed
`KFAULT vec=13 ... cs=0x33` and the screen froze with `System halted.`

- [x] **Step 2: Implement** — after the page-fault branch:

```rust
if from_user {
    let sig = signal_for(vec);
    // log [UFAULT vec= tid= rip= sig=] to serial, one line to the console
    unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
    scheduler::exit_with(-sig);
}
```

and the page-fault kill becomes `scheduler::exit_with(-SIGSEGV)`. `qsh`'s
`set_status` names the signal for a negative status: `-11` prints
`name: segmentation fault`.

- [x] **Step 3: Verify** — `tlstest` (which crashed before Task 4) printed
`tlstest: segmentation fault`, and `echo still here` ran afterwards. Serial:
`[UFAULT vec=13 tid=11 rip=0x8000000490 sig=11]`.

- [x] **Step 4: Document** — in `docs/abi.md`, after the `SYS_WAIT` row's
table, a paragraph: a negative exit code means the task was killed, and its
magnitude is the Linux signal number of the exception.

- [x] **Step 5: Commit** in `quark`.

---

### Task 4: musl finds its thread-local template

**Files:**
- Modify: `quark/user/quark-rt/src/spawn.rs` (`MAX_PHDRS`, `PHDRS_AT`,
  `Spawned::{phdrs, phnum, EMPTY}`, `load`, `set_args_env`, `write_phdrs`)
- Modify: `quark/user/libc/include/quark/layout.h` (`QUARK_PHDRS_AT` and
  friends)
- Modify: `quark/user/linux-abi/src/start.c` (`AT_PHDR`, `AT_PHENT`,
  `AT_PHNUM`)
- Modify: `quark/user/qsh/src/main.rs` (`Spawned::EMPTY`)
- Create: `explosion/toolchain/tests/tlstest.c`,
  `explosion/toolchain/build-tests.sh`
- Modify: `quark/docs/abi.md` (the argument page)

**Interfaces:**
- Produces: `spawn::PHDRS_AT` (= 3184), `spawn::MAX_PHDRS` (= 16),
  `spawn::Spawned::EMPTY`. The argument page's last 912 bytes hold
  `phentsize: u64, phnum: u64` then the program header table; argv and the
  environment stop before `PHDRS_AT`.

musl finds a static program's `PT_TLS` through `AT_PHDR`. The start block had
only `AT_PAGESZ`, so musl allocated TLS blocks with no room for the program's
thread-locals, which on x86-64 sit below the thread pointer and therefore
overwrote whatever preceded the block.

- [x] **Step 1: Write the failing test** — `tlstest.c`: `getauxval(AT_PHDR)` and
`AT_PHNUM` non-zero, a `PT_TLS` among the headers, a `.tdata` variable holding
its initial value, a 384-byte `.tbss` array inside the TLS block, and a thread
with its own copy. Built by `build-tests.sh` into the staging directory.

- [x] **Step 2: Run it** — first three checks `FAIL`, then a `movaps` fault
writing the misaligned array.

- [x] **Step 3: Implement** — `load` copies the program header table (all of it
or none, at most 16 entries); `set_args_env` stops argv/env at `PHDRS_AT` and
calls `write_phdrs`, which also rewrites any `PT_PHDR`'s `p_vaddr` to the
copy's address so that musl's computed load base is zero. `start.c` reads the
trailer and, when `phnum` is non-zero, at most 16, and `phent == 56`, adds

```c
start_block[i++] = AT_PHDR;  start_block[i++] = ARGS_PAGE + QUARK_PHDRS_AT + 16;
start_block[i++] = AT_PHENT; start_block[i++] = phent;
start_block[i++] = AT_PHNUM; start_block[i++] = phnum;
```

- [x] **Step 4: Verify** — `tlstest`: `tls: 0 failed`.

- [x] **Step 5: Document** — `docs/abi.md` gains a short section on the
argument page: its three parts and the offset of the header table, noting that
a spawner that predates it leaves the trailer zero and a program that predates
it never reads that far.

- [x] **Step 6: Commit** in `quark` and in `explosion`.

---

### Task 5: Constructors run

**Files:**
- Modify: `explosion/toolchain/build.sh` (`--enable-initfini-array`)
- Modify: `quark/user/linker.ld` (`.init_array`, `.fini_array`, the `.ctors`
  assertion); install it with `make -C quark/user/libc install-sysroot`
- Modify: `quark/user/wmtype/linker.ld` (becomes a symlink to `../linker.ld`)
- Modify: `quark/user/libc/src/start.c` (run `.init_array` before `main`)
- Create: `explosion/toolchain/tests/ctortest.c`

**Interfaces:**
- Produces: `__init_array_start`, `__init_array_end`, `__fini_array_start`,
  `__fini_array_end` in every Quark program.

gcc had been configured without `--enable-initfini-array`, so every constructor
went into `.ctors`, which only `crtbegin.o`/`crtend.o` run — and nothing here
links them. The link script had no `.init_array` either, and musl declares its
bounds weak, so it silently ran nothing. pixman builds its implementation table
in a constructor: every composite reported "No composite function found".

- [x] **Step 1: Rebuild gcc** — in `~/src/build-gcc-quark`, configured as
`build.sh` does plus `--enable-initfini-array`; `make all-gcc`,
`make all-target-libgcc`, `make install-gcc install-target-libgcc`. The
previous binaries are in `~/opt/cross-backup-20260916`. A constructor now
compiles into `.init_array`.

- [x] **Step 2: The link script** — `.init_array` and `.fini_array` with the
four `PROVIDE_HIDDEN` bounds, and `.ctors` collected only so that

```
ASSERT(__legacy_ctors_end == __legacy_ctors_start,
       "object has .ctors/.dtors: rebuild it with a compiler configured --enable-initfini-array")
```

turns a stale object into a link error. Both rust-lld and GNU ld accept it.

- [x] **Step 3: Write the failing test** — `ctortest.c`:

```c
#include <stdio.h>

static int order[3];
static int n;

__attribute__((constructor(101))) static void first(void)  { order[n++] = 1; }
__attribute__((constructor(102))) static void second(void) { order[n++] = 2; }
__attribute__((constructor))      static void plain(void)  { order[n++] = 3; }

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int ran = n == 3;
    int prioritised = order[0] == 1 && order[1] == 2;
    printf("ctor: all ran %s, in priority order %s\n",
           ran ? "ok" : "FAIL", prioritised ? "ok" : "FAIL");
    return ran && prioritised ? 0 : 1;
}
```

Build it with `build-tests.sh`, stage it, run it. It should pass already —
this task's compiler and script changes are in place — so run it against the
**backup** compiler first to see it fail:

```bash
PATH=~/opt/cross-backup-20260916/bin:$PATH x86_64-quark-musl-gcc -o /tmp/ctor-old ctortest.c
```

Expected: the link fails with the assertion's message, which is the point of
the assertion. (Before Step 2 it linked and printed `all ran FAIL`.)

- [x] **Step 4: Quark's own C library** — `user/libc/src/start.c` runs the
array before `main`:

```c
extern void (*const __init_array_start[])(void) __attribute__((weak));
extern void (*const __init_array_end[])(void) __attribute__((weak));

static void run_constructors(void) {
    for (void (*const *f)(void) = __init_array_start; f < __init_array_end; f++) {
        (*f)();
    }
}
```

called first thing in `_start`. Nothing built against this library has a
constructor today; the point is that the next one does not silently not run.

- [x] **Step 5: Rebuild pixman** — it was compiled by the old gcc, so its
constructor is in `.ctors`. With the new script its link now *fails*, which is
the assertion working. Rebuild:

```bash
cd ~/opt/src/pixman-0.44.2 && ninja -C build-quark -t clean && ninja -C build-quark
x86_64-quark-readelf -SW build-quark/test/region-test | grep -E "init_array|ctors"
```

Expected: an `.init_array` of 8 bytes and a `.ctors` of 0.

- [x] **Step 6: Verify** — `ctortest` prints `all ran ok, in priority order ok`;
`region-test` exits 0; dtest still `131 passed`.

- [x] **Step 7: Commit** in `quark` and in `explosion`.

---

### Task 6: Loading a program without leaking it, and a test runner

**Files:**
- Modify: `quark/user/quark-rt/src/spawn.rs` (`load_path`, `MAX_IMAGE_PAGES`)
- Modify: `quark/user/qsh/src/main.rs` (`cmd_spawn` uses `load_path`)
- Modify: `quark/user/dtest/src/main.rs` (`load_child` uses `load_path`; a new
  check that the staging range is released)
- Create: `quark/user/runtests/{Cargo.toml,.cargo/config.toml,src/main.rs}`,
  `quark/user/runtests/linker.ld` → `../linker.ld`
- Modify: `quark/Makefile` (`RUNTESTS_DIR`, `RUNTESTS_ELF`, the `user`
  prerequisites, `USR_PROGRAMS`)
- Modify: `explosion/Makefile` (`TEST_SUITES`)

**Interfaces:**
- Consumes: Task 4's `spawn::Spawned`.
- Produces:
  `pub fn load_path(vfs_tid: usize, path: &[u8], image_at: usize, scratch: &Scratch, grant: impl FnOnce(&[u8], usize)) -> Result<Spawned, ()>`,
  and a program `runtests <list>` that exits 0 only if every listed test did.

`qsh` stages each program it runs at one address and never frees the frames, so
a session that runs thirty half-megabyte test binaries leaks a hundred pages
each time. A runner that waits for each test and prints one summary line is
what makes a thirty-test result trustworthy in a fifty-line screendump.

- [x] **Step 1: Write the failing tests** — in dtest, after `load_child`
returns, the range it staged the image at must be free again. `sys_mmap`
refuses to map over a page that is already mapped, which makes that
observable:

```rust
check(
    "loading a program gives its staging memory back",
    syscall::sys_mmap(CHILD_IMAGE, 1).is_ok(),
);
let _ = syscall::sys_munmap(CHILD_IMAGE, 1);
```

And a list file exercising both outcomes of a runner,
`explosion/toolchain/tests/selftest.tests`:

```
# Expected: 2 passed, 1 failed
echo hello
echo world
tlstest-does-not-exist
```

- [x] **Step 2: Run them to verify they fail** — dtest reports
`FAIL loading a program gives its staging memory back`, because `load_child`
leaves its pages mapped; and `runtests` does not exist, so `qsh` prints
`runtests: not found`.

- [x] **Step 3: `load_path`** in `spawn.rs`:

```rust
/// The largest program `load_path` will read: four megabytes.
pub const MAX_IMAGE_PAGES: usize = 1024;

/// Read a program from the filesystem, load it, and give back the memory it
/// was read into.
///
/// `image_at` is a free range of `MAX_IMAGE_PAGES` pages in the caller.
/// `grant` sees the image before it is released, which is when a spawner
/// reads the program's manifest.
pub fn load_path(
    vfs_tid: usize,
    path: &[u8],
    image_at: usize,
    scratch: &Scratch,
    grant: impl FnOnce(&[u8], usize),
) -> Result<Spawned, ()> {
    let (handle, size, _) = crate::vfs::open(vfs_tid, path).map_err(|_| ())?;
    let size = size as usize;
    let pages = size.div_ceil(PAGE_SIZE);
    if pages == 0 || pages > MAX_IMAGE_PAGES {
        let _ = crate::vfs::close(vfs_tid, handle);
        return Err(());
    }
    let mut frames = [0usize; MAX_IMAGE_PAGES];
    let mut held = 0;
    let mut ok = true;
    for p in 0..pages {
        let Ok(frame) = syscall::sys_phys_alloc(1) else { ok = false; break };
        frames[p] = frame;
        held = p + 1;
        let at = image_at + p * PAGE_SIZE;
        let want = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        if syscall::sys_map_phys(frame, at, 1).is_err()
            || crate::vfs::read(vfs_tid, handle, frame, (p * PAGE_SIZE) as u32, want).is_err()
        {
            ok = false;
            break;
        }
    }
    let _ = crate::vfs::close(vfs_tid, handle);

    let result = if ok {
        let image = unsafe { core::slice::from_raw_parts(image_at as *const u8, size) };
        let loaded = load(image, scratch);
        if let Ok(info) = loaded {
            grant(image, info.tid);
        }
        loaded
    } else {
        Err(())
    };

    // The child has its own frames now; these were only ever a copy.
    for p in 0..held {
        let _ = syscall::sys_munmap(image_at + p * PAGE_SIZE, 1);
        let _ = syscall::sys_phys_free(frames[p], 1);
    }
    result
}
```

- [x] **Step 4: `qsh` and dtest use it** — `qsh`'s `cmd_spawn` replaces its
read loop with
`spawn::load_path(vfs_tid, path, FILE_BUF_BASE, &SPAWN_SCRATCH, grant_caps_from_manifest)`,
trying the same path spellings it already tries in the same order. dtest's
`load_child` becomes
`spawn::load_path(vfs_tid, b"/usr/bin/dchild", CHILD_IMAGE, &SPAWN_SCRATCH, |img, tid| quark_rt::manifest::grant_image(tid, img, 12))`
followed by the endpoint grant and descriptor wiring it already does.

- [x] **Step 5: `runtests`** — a `no_std` program with the manifest
`[CapReq::task_mgmt(0), CapReq::phys_alloc(64)]`, the same as `qsh`. It reads
the list named by `argv[1]` (at most one page) through `vfs::open`/`vfs::read`
into a frame mapped at `0x9A_0000_0000`, and for each line that is not empty
and does not start with `#`:

```rust
let (name, args) = split_first_word(line);
let mut path = [0u8; 64];
let n = join(b"/usr/bin/", name, &mut path);
let info = match spawn::load_path(vfs, &path[..n], IMAGE_AT, &SCRATCH, grant) {
    Ok(i) => i,
    Err(()) => { report(name, Outcome::Missing); failed += 1; continue; }
};
let _ = syscall::sys_fd_dup(info.tid, 1, 1);
let _ = syscall::sys_fd_dup(info.tid, 2, 2);
let argv = split_words(name, args);          // at most 16
let _ = spawn::set_args_env(&info, &argv, &ENV, &SCRATCH);
if info.start().is_err() { report(name, Outcome::Missing); failed += 1; continue; }
let code = loop {
    match syscall::sys_wait() {
        Ok((tid, code)) if tid == info.tid => break code,
        Ok(_) => continue,                    // not ours
        Err(()) => break i32::MIN,           // no children: it vanished
    }
};
if code == 0 { passed += 1 } else { failed += 1 }
report(name, Outcome::from(code));
```

with these helpers, in the same file:

```rust
/// The first space-separated word, and whatever follows the spaces after it.
fn split_first_word(line: &[u8]) -> (&[u8], &[u8]) {
    let end = line.iter().position(|&b| b == b' ').unwrap_or(line.len());
    let mut rest = end;
    while rest < line.len() && line[rest] == b' ' {
        rest += 1;
    }
    (&line[..end], &line[rest..])
}

/// `prefix` then `name` into `out`; the length written, or 0 if it does not fit.
fn join(prefix: &[u8], name: &[u8], out: &mut [u8]) -> usize {
    let n = prefix.len() + name.len();
    if n > out.len() {
        return 0;
    }
    out[..prefix.len()].copy_from_slice(prefix);
    out[prefix.len()..n].copy_from_slice(name);
    n
}

/// `name` followed by each space-separated word of `args`, at most sixteen.
fn split_words<'a>(name: &'a [u8], args: &'a [u8], out: &mut [&'a [u8]; 16]) -> usize {
    out[0] = name;
    let mut n = 1;
    for w in args.split(|&b| b == b' ').filter(|w| !w.is_empty()) {
        if n == out.len() {
            break;
        }
        out[n] = w;
        n += 1;
    }
    n
}

enum Outcome {
    Passed,
    Exit(i32),
    Signal(i32),
    Missing,
}

impl Outcome {
    fn from(code: i32) -> Outcome {
        match code {
            0 => Outcome::Passed,
            i32::MIN => Outcome::Missing,
            c if c < 0 => Outcome::Signal(-c),
            c => Outcome::Exit(c),
        }
    }
}

fn report(name: &[u8], o: Outcome) {
    let name = core::str::from_utf8(name).unwrap_or("?");
    match o {
        Outcome::Passed => println!("  ok    {}", name),
        Outcome::Exit(c) => println!("  FAIL  {} (exit {})", name, c),
        Outcome::Signal(s) => println!("  FAIL  {} (signal {})", name, s),
        Outcome::Missing => println!("  FAIL  {} (not found)", name),
    }
}
```

In the loop, `split_words` is called as
`let mut argv = [&b""[..]; 16]; let argc = split_words(name, args, &mut argv);`
and `set_args_env` gets `&argv[..argc]`. The last line is `runtests: <passed> passed, <failed> failed`,
and the exit status is 0 only if nothing failed. `grant` is `qsh`'s
`grant_caps_from_manifest`, copied; `ENV` is `qsh`'s `BASE_ENV`; `SCRATCH` uses
`elf: 0x9B_0000_0000, stack: 0x9C_0000_0000, args: 0x9D_0000_0000`.

- [x] **Step 6: Staging** — `explosion/Makefile` gains `TEST_SUITES ?=` next to
`WAYLAND_CLIENTS`, and in the `stage` recipe, after the Wayland block:

```make
	@for d in $(TEST_SUITES); do \
		n=0; \
		for f in $$d/*; do \
			[ -f "$$f" ] || continue; \
			b=`basename $$f`; \
			case "$$b" in \
			*.tests) cp "$$f" $(STAGE)/etc/$$b ;; \
			*) [ -x "$$f" ] || continue; \
			   cp "$$f" $(STAGE)/usr/bin/$$b; \
			   x86_64-quark-strip $(STAGE)/usr/bin/$$b 2>/dev/null || true; \
			   n=$$((n + 1)) ;; \
			esac; \
		done; \
		echo "tests: staged $$n programs from $$d"; \
	done
```

`build-tests.sh` copies `tests/*.tests` into its output directory, and takes a
test's link flags from an optional first line of the form
`// LINK: -lcairo -lpixman-1 -lm`:

```sh
for src in "$HERE"/tests/*.c; do
    name=$(basename "$src" .c)
    flags=$(sed -n '1s|^// LINK: ||p' "$src")
    echo "==> $name"
    x86_64-quark-musl-gcc -O2 -o "$OUT/$name" "$src" $flags
done
cp "$HERE"/tests/*.tests "$OUT"/ 2>/dev/null || true
```

- [x] **Step 7: Verify** — dtest passes its new check. Stage
`build-tests.sh`'s output with `TEST_SUITES`, boot, run
`runtests /etc/selftest.tests`. Expected: `ok echo`, `ok echo`,
`FAIL tlstest-does-not-exist (not found)`, `runtests: 2 passed, 1 failed`,
and `qsh` reports `runtests: exit 1`.

- [x] **Step 8: Commit** in `quark` and in `explosion`.

---

### Task 7: pixman, and its whole test suite

**Files:**
- Create: `explosion/toolchain/build-pixman.sh`
- Create: `explosion/toolchain/pixman.tests`

**Interfaces:**
- Consumes: Task 6's `runtests` and `TEST_SUITES`.
- Produces: `libpixman-1.a`, `pixman.h` and `pixman-1.pc` in
  `~/opt/cross/x86_64-quark/musl`; a directory of test programs and
  `pixman.tests`.

- [x] **Step 1: The list** — `pixman.tests`, every test that passes on the host
with the same configuration, each with its default iteration count so that the
fuzzers compare against their built-in checksums:

```
a1-trap-test
affine-test
alpha-loop
alphamap
blitters-test
combiner-test
composite-traps-test
cover-test
fetch-test
filter-reduction-test
glyph-test
gradient-crash-test
infinite-loop
matrix-test
oob-test
pdf-op-test
pixel-test
prng-test
radial-invalid
region-contains-test
region-test
region-translate-test
rotate-test
scaling-crash-test
scaling-helpers-test
scaling-test
solid-test
stress-test
thread-test
tolerance-test
trap-crasher
```

Left out, and why, in a comment at the top of the file:
`fence-image-self-test` needs `mprotect(PROT_NONE)` to fault and a SIGSEGV
handler to catch it, and Quark has neither; `check-formats` is a tool that needs
arguments; the `*-bench` programs and `radial-perf-test` measure rather than
check. On the host these 31 take 77 seconds.

- [x] **Step 2: Run it to verify it fails** — with the tests staged from the
spike's build (before Task 5), `runtests /etc/pixman.tests` reports failures,
among them `pixel-test` and `region-test`.

- [x] **Step 3: `build-pixman.sh`**:

```sh
#!/bin/sh
# Build pixman for Quark: the generic C path, a static library, and the test
# suite, which is how this port is checked.
#
#     ./build-pixman.sh /path/to/pixman-0.44.2 [test-outdir]
set -e
SRC=${1:?usage: build-pixman.sh <pixman-src> [test-outdir]}
OUT=${2:-$PWD/pixman-tests}
HERE=$(cd "$(dirname "$0")" && pwd)
PREFIX=${PREFIX:-$HOME/opt/cross/x86_64-quark/musl}
CROSS=$(mktemp)
sed -e "s|@WAYLAND_SCANNER@|/bin/false|" -e "s|@HOME@|$HOME|g" \
    "$HERE/meson-cross-quark.ini" > "$CROSS"

cd "$SRC"
rm -rf build-quark
meson setup build-quark --cross-file "$CROSS" --prefix="$PREFIX" \
    -Ddefault_library=static -Db_staticpic=false \
    -Dmmx=disabled -Dsse2=disabled -Dssse3=disabled -Dvmx=disabled \
    -Dloongson-mmi=disabled -Darm-simd=disabled -Dneon=disabled \
    -Da64-neon=disabled -Dmips-dspr2=disabled -Drvv=disabled \
    -Dopenmp=disabled -Dgtk=disabled -Dlibpng=disabled \
    -Ddemos=disabled -Dtests=enabled
ninja -C build-quark
ninja -C build-quark install
rm -f "$CROSS"

mkdir -p "$OUT"
grep -v '^#' "$HERE/pixman.tests" | while read -r t; do
    [ -n "$t" ] && cp "build-quark/test/$t" "$OUT/"
done
cp "$HERE/pixman.tests" "$OUT/"
echo "pixman installed into $PREFIX; tests in $OUT"
```

- [x] **Step 4: Run it to verify it passes** — `build-pixman.sh`, stage the
output with `make hd TEST_SUITES=<dir>`, boot, run
`runtests /etc/pixman.tests`, and give it three minutes. Expected:
`runtests: 31 passed, 0 failed`.

Stripped, the 31 programs are about 15 MB. If the root filesystem runs out of
room, the Makefile's missing-file check names what did not arrive; raise the
size on the command line with `ROOTFS_SIZE_KB=65536` rather than dropping
tests.

- [x] **Step 5: Commit** in `explosion`.

**What Step 4 actually found.** The first run gave 27 of 31, and the four
failures were memory, three more platform bugs that only a suite of programs
run back to back could expose. A spawner lent each child its pages and kept
them until it exited; a dead task was reaped only when the machine went idle,
which a suite never lets it do; and a reaped task's dead threads kept naming it
as their parent, so a recycled slot hid them. Fixed in quark by
`sys_addrspace_give` (ABI 1.11), reaping in `sys_wait`, and orphaning on reap
(dtest 145/145). The last failure, `stress-test`, asks for a 2.7 GB mask that
Quark refuses because nothing is demand-paged, and linux-abi kept the part of
the mapping it had made before the refusal, which wedged every later `mmap`
(`mmaptest`). `wm` also stopped keeping the frames it read clients into. With
all of that, `runtests: 31 passed, 0 failed`.

---

### Task 8: cairo, the image backend

**Files:**
- Create: `explosion/toolchain/build-cairo.sh`
- Create: `explosion/toolchain/tests/cairotest.c`, `explosion/toolchain/tests/cairo.tests`

**Interfaces:**
- Consumes: Task 7's installed pixman.
- Produces: `libcairo.a`, `cairo.h` and `cairo.pc` in the musl prefix;
  `cairotest`.

- [x] **Step 1: Write the test** — `cairotest.c` draws a fixed scene into a
200×200 `CAIRO_FORMAT_ARGB32` image surface, then prints and checks a CRC-32 of
the pixels:

```c
// LINK: -lcairo -lpixman-1 -lm
#include <cairo.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>

static uint32_t crc32(const unsigned char *p, size_t n) {
    uint32_t c = 0xFFFFFFFFu;
    for (size_t i = 0; i < n; i++) {
        c ^= p[i];
        for (int k = 0; k < 8; k++) {
            c = (c >> 1) ^ (0xEDB88320u & (uint32_t)-(int32_t)(c & 1));
        }
    }
    return ~c;
}

/* Filled in from a host build of the same cairo and pixman: see build-cairo.sh. */
#define EXPECTED 0x00000000u

int main(void) {
    cairo_surface_t *s = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, 200, 200);
    cairo_t *cr = cairo_create(s);

    cairo_pattern_t *g = cairo_pattern_create_linear(0, 0, 200, 200);
    cairo_pattern_add_color_stop_rgb(g, 0, 0.1, 0.2, 0.5);
    cairo_pattern_add_color_stop_rgb(g, 1, 0.9, 0.4, 0.1);
    cairo_set_source(cr, g);
    cairo_paint(cr);
    cairo_pattern_destroy(g);

    cairo_set_source_rgba(cr, 1, 1, 1, 0.7);
    cairo_arc(cr, 100, 100, 60, 0, 2 * M_PI);
    cairo_set_line_width(cr, 7.5);
    cairo_stroke(cr);

    cairo_save(cr);
    cairo_translate(cr, 100, 100);
    cairo_rotate(cr, M_PI / 7);
    cairo_rectangle(cr, -30, -30, 60, 60);
    cairo_set_source_rgb(cr, 0.2, 0.8, 0.3);
    cairo_fill(cr);
    cairo_restore(cr);

    cairo_move_to(cr, 10, 190);
    cairo_curve_to(cr, 60, 20, 140, 20, 190, 190);
    cairo_set_source_rgb(cr, 0.9, 0.1, 0.5);
    cairo_set_line_width(cr, 3);
    cairo_stroke(cr);

    cairo_surface_flush(s);
    uint32_t sum = crc32(cairo_image_surface_get_data(s),
                         (size_t)cairo_image_surface_get_stride(s) * 200);
    printf("cairo: checksum %08X, expected %08X\n", sum, EXPECTED);
    cairo_destroy(cr);
    cairo_surface_destroy(s);
    return sum == EXPECTED ? 0 : 1;
}
```

`cairo.tests` holds the one line `cairotest`.

- [x] **Step 2: Run it to verify it fails** — it does not link: there is no
`libcairo.a`.

- [x] **Step 3: `build-cairo.sh`** — the same shape as `build-pixman.sh`, with

```sh
meson setup build-quark --cross-file "$CROSS" --prefix="$PREFIX" \
    -Ddefault_library=static -Db_staticpic=false \
    -Dfreetype=disabled -Dfontconfig=disabled -Dpng=disabled \
    -Dzlib=disabled -Dtee=disabled -Dxcb=disabled -Dxlib=disabled \
    -Dquartz=disabled -Ddwrite=disabled -Dlzo=disabled \
    -Dglib=disabled -Dspectre=disabled -Dsymbol-lookup=disabled \
    -Dtests=disabled
```

and then a **host** build of the same configuration against the host pixman
from Task 7's spike (`PKG_CONFIG_PATH=<pixman>/build-host/meson-uninstalled`),
used only to compute `EXPECTED`: build `cairotest.c` against it with the host
compiler, run it, and write the checksum it prints into the `#define`.

If configuring for Quark fails naming a dependency, disable the feature that
dependency belongs to, and record in the script why it is off.

- [x] **Step 4: Run it to verify it passes** — build `cairotest` with
`build-tests.sh`, stage, boot, `runtests /etc/cairo.tests`. Expected: the
printed checksum equals `EXPECTED`.

If it does not, the likely cause is `libm`: cairo takes sines and cosines for
arcs, and musl's and glibc's can differ in the last bit. To tell, have
`cairotest` write the pixels to `/tmp/cairo.ppm` when given an argument, run it
with one, and extract the file after QEMU exits:

```bash
dd if=hdimage.bin of=/tmp/root.img bs=512 skip=73762 count=67584
debugfs -R "dump /tmp/cairo.ppm /tmp/quark-cairo.ppm" /tmp/root.img
```

Compare it with the host's rendering pixel by pixel. Differences of one or two
levels along antialiased edges are `libm`; replace `EXPECTED` with the Quark
checksum and say so in the file. Anything larger is a bug to chase before this
task is done.

- [x] **Step 5: Commit** in `explosion`.

**What happened.** Configuring for Quark failed on `pixman.h`: the cross file's
`sys_root` made pkg-config prefix it to pixman's include path, and nothing is
installed under the sysroot, so it is gone from the cross file. cairo also
builds at `-O0` unless told otherwise; both builds use `debugoptimized`. The
checksum matched the host's on the first run, `F0D42355`, so the `libm`
fallback was not needed.

---

### Task 9: A Wayland client that draws with cairo

**Files:**
- Create: `explosion/toolchain/wlcairo.c`
- Modify: `explosion/toolchain/build-weston-client.sh` (build `wlcairo` when
  `libcairo.a` is installed)

**Interfaces:**
- Consumes: Task 8's cairo, the compositor from Phase 8.

- [x] **Step 1: Write the client** — copy `wlprobe.c`, remove the keyboard,
pointer and decoration parts and every `printf` but the errors, set the title
to `"wlcairo"`, and replace `paint(n, tick)` with:

```c
#include <cairo.h>
#include <math.h>

/* The same scene cairotest checksums, with the square turning. XRGB8888 is
   what the pool's buffers are, and cairo's ARGB32 has the same layout, so the
   buffer is drawn into in place — nothing is copied. */
static void paint(int n, int tick) {
    cairo_surface_t *s = cairo_image_surface_create_for_data(
        pixels[n], CAIRO_FORMAT_ARGB32, W, H, STRIDE);
    cairo_t *cr = cairo_create(s);

    cairo_pattern_t *g = cairo_pattern_create_linear(0, 0, W, H);
    cairo_pattern_add_color_stop_rgb(g, 0, 0.1, 0.2, 0.5);
    cairo_pattern_add_color_stop_rgb(g, 1, 0.9, 0.4, 0.1);
    cairo_set_source(cr, g);
    cairo_paint(cr);
    cairo_pattern_destroy(g);

    cairo_set_source_rgba(cr, 1, 1, 1, 0.7);
    cairo_arc(cr, W / 2.0, H / 2.0, H / 3.0, 0, 2 * M_PI);
    cairo_set_line_width(cr, 7.5);
    cairo_stroke(cr);

    cairo_save(cr);
    cairo_translate(cr, W / 2.0, H / 2.0);
    cairo_rotate(cr, tick * M_PI / 90);
    cairo_rectangle(cr, -40, -40, 80, 80);
    cairo_set_source_rgb(cr, 0.2, 0.8, 0.3);
    cairo_fill(cr);
    cairo_restore(cr);

    cairo_destroy(cr);
    cairo_surface_destroy(s);
}
```

- [x] **Step 2: Build it** — in `build-weston-client.sh`, after `wlclip`:

```sh
if [ -f "$HOME/opt/cross/x86_64-quark/musl/lib/libcairo.a" ]; then
    echo "==> wlcairo"
    x86_64-quark-musl-gcc -O2 -o "$OUT/wlcairo" "$HERE/wlcairo.c" \
        "$OUT/xdg-shell-protocol.c" $INC -lcairo -lpixman-1 -lm $LIB
fi
```

- [x] **Step 3: Verify** — stage, boot, `wm wlcairo`, screendump twice a second
apart. Expected: a window titled `wlcairo` holding the gradient, the ring and
the curve, with the green square at a different angle in each screendump.

- [x] **Step 4: Commit** in `explosion`.

---

### Task 10: Write it down

**Files:**
- Modify: `~/src/osdev/ROADMAP.md` (Phase 11)
- Modify: `quark/CLAUDE.md` (invariants)
- Modify: `quark/docs/superpowers/plans/2026-09-16-pixman-cairo.md` (tick)

- [x] **Step 1: `CLAUDE.md`** — three invariants: a task's floating-point
state is saved on every switch and FXSAVE stops being enough the moment
OSXSAVE is set; the argument page carries the program header table and a C
program's thread-locals depend on it; a fault in ring 3 kills the task and
never the machine.

- [x] **Step 2: `ROADMAP.md`** — Phase 11 marked done, with what the spike found
and in what order, and the one thing it deferred: SIMD in pixman.

- [x] **Step 3: Commit and push** all three repositories.

---

## Deliberately not in this plan

pixman's SIMD paths, which are a performance change to measure rather than part
of the port. cairo's text: the toy font API needs a font backend and the only
one Quark can have is freetype, which is Phase 13. cairo's own test suite, which
compares against PNG reference images and so needs libpng and zlib. `mprotect`,
which `fence-image-self-test` would need, and signal delivery.

## Acceptance

pixman's test suite reports 31 passed on Quark; `cairotest`'s checksum matches
the host's (or differs only by `libm` rounding, shown by a pixel comparison);
and `wlcairo` animates in a window. — *All three met*: 31 passed, `F0D42355` on
both, and the square turns between screendumps. CLAUDE.md gained the ownership,
reaping and constructor invariants as well as the three Task 10 named.
