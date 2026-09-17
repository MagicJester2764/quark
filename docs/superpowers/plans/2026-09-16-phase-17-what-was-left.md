# Phase 17 — What Phase 13 Left, and Programs That Fall Over — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This project runs plans inline (superpowers:executing-plans), on `main`, one commit per task.

**Goal:** Quark gets the file and memory features Phase 13 listed as missing — links, a working directory, record locks, random numbers and `/dev`, files that can be mapped (with memory backed on demand), a crash-safe orphan list, font caches built with the image — and its programs stop falling over when they are handed something unexpected.

**Architecture:** A program's identity becomes its address space (a *space id* the kernel assigns and never reuses), because file handles, working directories and locks belong to programs, not threads. The VFS grows the new namespace operations and serves as the *pager* for mapped files: the kernel marks unbacked pages in the page tables themselves and fills them on fault, anonymous ones with zeroes and file ones by calling the pager with a kernel frame lent for it to fill. Part B hardens every input path — typed lines, display and keyboard claims, `wm`'s session programs, IPC from strangers, Wayland from bad clients, command-line arguments — and proves it with fuzzers that run on the image.

**Tech Stack:** Rust `no_std` kernel and servers (nightly-2026-03-01), `quark-rt`, the musl-based C toolchain and `user/linux-abi`, the Rust std fork (`../rust`, branch `quark`), e2fsprogs for checking images, QEMU/KVM for every test.

**Spec:** The user's request of 2026-09-16 ("Fix all the still missing, then fix all applications such as `wm` that crash when the input they take is unexpected … After this interim phase is complete, continue to Phase 14"), the "Still missing" list at the end of Phase 13, and the scoping notes in this plan's "What scoping found" section.

## Global Constraints

- Repos are flat siblings under `~/src/osdev`: `quark`, `explosion`, `bang`, `rust` (branch `quark`). Commit on `main` (the fork: `quark`), one commit per task, push after each task. Commit messages end with the two attribution lines this session uses.
- Every task is verified by booting QEMU (`explosion/tools/boot-test.sh <keys> <ppm>`), reading the screenshots and `/tmp/quark-boot-test/serial.log` (no `UFAULT`, `UPFAULT`, `KFAULT` or `PANIC` unless a test provokes one on purpose). Filesystem work is checked with `explosion/tools/check-rootfs.sh` on **both** `make hd` (ext2) and `make hd-ext4`.
- New system calls take numbers in their subsystem's block: 107–108 (task identity), 116 (hardware: entropy), and the new block **0xC0 "Paging and memory objects"** (192–207). `ABI_VERSION_MINOR` becomes 2 (ABI 2.2), recorded in `docs/abi.md`'s "What each minor of 2 added" table in the task that adds the first of them. `tools/check-abi.sh` (run by `make`) must pass.
- New VFS requests take tags from 15 upward and are documented in `docs/vfs.md` in the same task. Numbers are never reused.
- A server never panics on input. Unknown tags, forged notices, bad handles, bad lengths and bad lends get an error reply (or are dropped when nobody waits on a reply). Death notices are believed only from sender 0, the kernel.
- A C test lives in `explosion/toolchain/tests/*.c` with a `// LINK:` or `// PKG:` first line and is listed in a `*.tests` file; a Rust check lives in `user/dtest` as a section. Where a test can run on the host (Linux), run it there first: that is what caught Phase 13's wrong element count.
- After any change to `user/linux-abi` or `user/libc/src/quark.c`: `make -C user/linux-abi install-sysroot`, then relink `suite-c` (`toolchain/build-tests.sh`), `suite-zlib` (`build-zlib.sh`), `suite-pixman` (`build-pixman.sh`), `suite-fc` (`build-fontconfig.sh`) and the clients (`build-weston-client.sh`), and commit the relinked `explosion/clients/*`.
- After any change to `quark-rt` used by std, rebuild `hello` (the Makefile does this when quark-rt's hash changes) and, if the fork changed, commit and push `../rust`.

## Working conventions

```sh
export PATH="$HOME/.local/bin:$HOME/opt/cross/bin:$PATH"
SP=/tmp/claude-1000/-home-nrupard-src-osdev-quark/1af756cd-1fd0-471d-bbf5-a2d0b8e112b2/scratchpad
SUITES="/tmp/claude-1000/suite-c /tmp/claude-1000/suite-pixman /tmp/claude-1000/suite-zlib /tmp/claude-1000/suite-fc"
cd ~/src/osdev/quark && make                       # kernel, drivers, programs; runs check-abi
make -C user/linux-abi clean && make -C user/linux-abi && make -C user/linux-abi install-sysroot
cd ~/src/osdev/explosion
sh toolchain/build-tests.sh /tmp/claude-1000/suite-c
make hd WAYLAND_CLIENTS=$PWD/clients TEST_SUITES="$SUITES" ROOT_OVERLAYS=/tmp/claude-1000/overlay-fonts
timeout 300 sh tools/boot-test.sh $SP/<name>.keys /tmp/claude-1000/<shot>.ppm
sh tools/check-rootfs.sh
```

A key script is `sleep`, `type` (with `\n`), `key esc`, `shot <ppm>`, `hmp <monitor command>` and `quit`, one per line; it logs in with `type \n`, `type root\n` after `sleep 16`.

## What scoping found

Reproduced on the Phase 13 image before planning:

- `wm wm` leaves the machine on a blank screen for good. `fb` remembers one previous owner, so console → wm → wm forgets the console; `input` refuses a second claim outright. Both need a stack of claimants.
- Every typed line is cut to 40 bytes: `serve_read` in `user/input` answers one 40-byte message and drops the rest. `echo` with 84 characters prints 35; `wm` with three client names cannot find the third.
- `wm` grants its programs no capabilities, so `hello` cannot allocate a thread stack (`sys_phys_alloc` needs `PhysAlloc`) and aborts; it passes its window index as `argv[1]`, so `wm ls` tries to open a file called `1`; it starts at most four programs and ignores the rest silently; and it has no way to give a program arguments (the shell has no quoting).
- `input`, `fb`, `nameserver`, `vfs` and `wm` act on `TAG_TASK_DIED` from anybody. The kernel sends it from sender 0; a program can send it too and make the nameserver forget `vfs`.
- The disk driver writes sectors for any program that looks it up, and the keyboard driver hands keys to anybody. A fuzzer would wreck the filesystem; a program could read the keyboard behind the input server's back.
- VFS handles belong to a TID. A thread cannot use a file its sibling opened, and locks and working directories need an owner that is the program.
- `e2fsck -p` clears an ext2 or ext4 orphan list (`Clearing orphaned inode`); `-n` reports the list's inodes as bitmap differences. The VFS must keep the list and recover it at mount.
- A host fontconfig 2.18.3 run as `fc-cache -s -f -y <stage>` writes caches that differ from the ones Quark writes in exactly three bytes: the directory's modification time. With whole-second directory times carried into the image, the host's caches are valid on Quark. It also writes `cache-9`, `-10` and `-11` as symbolic links to `cache-12`.
- The std port has no `std::fs` (every call is `unsupported`), and its random numbers fall back to zeroes when RDRAND fails. The first stays a known gap; the second is fixed here.

---

### Task 1: Typed lines of any length

Part B's first fix, done first because every later boot test types commands longer than 40 bytes.

**Files:**
- Modify: `quark/user/input/src/main.rs` (`serve_read`, the `TAG_READ` arm)
- Modify: `quark/user/quark-rt/src/stdio.rs` (`read_line_result` loops to the newline)
- Modify: `quark/user/dtest/src/main.rs` (nothing: the check is a boot test)

**Interfaces:**
- Produces: `quark_rt::stdio::read_line_result(buf) -> Result<usize, ()>` returns a whole line (up to `buf.len()`), however many reads it takes. A C program's `read(0, …)` returns what is left of the line on the next call, as a terminal does.

- [x] **Step 1: The failing check.** Boot the current image and type `echo 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 0123456789 end`. Expected today: `0123456789 0123456789 0123456789 01`.

- [x] **Step 2: Keep the rest of the line.** In `user/input/src/main.rs`, a completed line is kept with a read position; `TAG_READ` answers from it before reading more keys:

```rust
/// A line that was finished but not yet all handed over. A read gets at most
/// forty bytes — one message — and the rest waits here for the next read,
/// which is how a terminal hands a long line to a short read.
struct Pending {
    buf: [u8; LINE_BUF_SIZE],
    len: usize,
    at: usize,
}

impl Pending {
    fn take(&mut self, max: usize) -> Option<Message> {
        if self.at >= self.len {
            return None;
        }
        let n = (self.len - self.at).min(max);
        let reply = pack_read_reply(&self.buf[self.at..], n);
        self.at += n;
        Some(reply)
    }
}
```

`serve_read` copies the finished line (with its `\n`) into `Pending` and replies with `pending.take(max_bytes)`; the `TAG_READ` arm first tries `pending.take(max_bytes)` and replies with it if there is one, before deferring or reading keys. Ctrl-C clears `pending`. A claim (`TAG_INPUT_CLAIM`) leaves `pending` alone: what was typed before the claim still belongs to the reader.

- [x] **Step 3: Read to the newline.** `read_line_result` in quark-rt reads until a `\n` arrives, the buffer is full, or a read returns 0 after something was read:

```rust
pub fn read_line_result(buf: &mut [u8]) -> Result<usize, ()> {
    let mut got = 0;
    while got < buf.len() {
        let ret = syscall::sys_fd_read(0, &mut buf[got..]);
        if ret == u64::MAX {
            return if got == 0 { Err(()) } else { Ok(got) };
        }
        let n = ret as usize;
        if n == 0 {
            break;
        }
        got += n;
        if buf[got - 1] == b'\n' {
            break;
        }
    }
    Ok(got)
}
```

- [x] **Step 4: Verify.** `make`; image; boot: the long `echo` prints all eight groups and `end`; `wm weston-simple-shm weston-simple-shm weston-simple-shm` shows three windows (Esc); `runtests /etc/libc.tests` passes; `login` still works (it reads lines too).

- [x] **Step 5: Commit.** quark: "A typed line arrives whole, however long".

---

### Task 2: A program is its address space

**Files:**
- Modify: `quark/src/userspace.rs` (a space id per registered address space), `quark/src/task.rs` (`Task.space`), `quark/src/scheduler.rs` (set on start; notice when a space's last task dies), `quark/src/ipc.rs` (space watchers and `TAG_SPACE_DIED`), `quark/src/syscall.rs` (`SYS_TASK_SPACE` 107, `SYS_SPACE_WATCH` 108, `ABI_VERSION_MINOR` 2)
- Modify: `quark/user/quark-rt/src/syscall.rs`, `quark/user/quark-rt/src/ipc.rs`, `quark/user/quark-rt/src/thread.rs` (stacks from `sys_mmap`)
- Modify: `quark/user/vfs/src/handles.rs`, `quark/user/vfs/src/main.rs` (handles owned by a space)
- Modify: `quark/docs/abi.md`, `quark/docs/vfs.md`
- Create: `explosion/toolchain/tests/threadfile.c`; Modify: `explosion/toolchain/tests/libc.tests`
- Modify: `quark/user/dtest/src/main.rs` (section `spaces` gains the checks)

**Interfaces:**
- Produces (kernel and quark-rt): `SYS_TASK_SPACE = 107` (arg0 = tid → the task's space id, `u64::MAX` if none); `SYS_SPACE_WATCH = 108` (arg0 = space id → 0 or `u64::MAX`); `TAG_SPACE_DIED = 0xFFFF_0004`, sender 0, `data[0]` = space id, sent once when the last live task of that space dies. `quark_rt::syscall::sys_task_space(tid: usize) -> Result<u64, ()>`, `sys_space_watch(space: u64) -> Result<(), ()>`, `quark_rt::ipc::TAG_SPACE_DIED`.
- Produces (VFS): `OpenFile.owner` is a space id; `handles::get(handle, space)`, `handles::close(handle, space)`, `handles::close_all(space, &mut closed)`; `main.rs` gets `fn space_of(sender: usize) -> u64`.

Space ids start at 1 and count up for as long as the machine runs; they are never reused, so a notice about a dead program can never be mistaken for a live one.

- [x] **Step 1: The failing tests.** `threadfile.c` (no `LINK:`; musl has threads):

```c
/* A file one thread opened is the program's, not the thread's. */
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int fd;
static char got[8];
static int nread = -1;

static void *reader(void *arg) {
    (void)arg;
    nread = (int)pread(fd, got, 4, 0);
    return NULL;
}

int main(void) {
    fd = open("/etc/passwd", O_RDONLY);
    pthread_t t;
    int started = fd >= 0 && pthread_create(&t, NULL, reader, NULL) == 0;
    if (started) {
        pthread_join(t, NULL);
    }
    int ok = started && nread == 4 && !memcmp(got, "root", 4);
    printf("threadfile: %s\n", ok ? "ok" : "FAILED");
    return ok ? 0 : 1;
}
```

`pread` is `lseek` + `read` in the Linux layer if it is not already answered; check `grep -n pread64 user/linux-abi/src/syscall.c` and add `LX_pread64` (17) and `LX_pwrite64` (18) — read or write at an offset without moving the descriptor's position — if missing.

In dtest's `spaces` section, a thread started with `thread::spawn_with_stack` opens `/etc/passwd` through `vfs::open` and hands the handle to the main thread through an `AtomicUsize`; the main thread reads it: `check("a thread's file is the program's", …)`. Also `check("a program has one space", sys_task_space(me) == sys_task_space(thread))` and `check("a child has another", sys_task_space(child) != sys_task_space(me))`.

- [x] **Step 2: Run them.** Expected: `threadfile: FAILED` (the read gets `INVALID_HANDLE`, EBADF); dtest fails to build (`sys_task_space` does not exist).

- [x] **Step 3: The kernel.** In `userspace.rs` the registry entry becomes `(cr3, owner, refs, space)`; `register_address_space` takes the next id from `static NEXT_SPACE: AtomicU64 = AtomicU64::new(1)`; `pub fn space_of(cr3) -> u64` looks it up (0 if unregistered). `Task` gains `pub space: u64`, set wherever a task's `cr3` is set (`start_task`, `spawn_init`). `SYS_TASK_SPACE` answers `task.space` for a live task. In `ipc.rs`:

```rust
/// Watchers of a program: one entry per watched space, a bitmask of TIDs.
const MAX_SPACE_WATCHES: usize = MAX_TASKS * 2;
static mut SPACE_WATCHES: [(u64, u64); MAX_SPACE_WATCHES] = [(0, 0); MAX_SPACE_WATCHES];
/// Space deaths waiting to be received, per watcher.
static mut SPACE_DEATHS: [[u64; DEATH_QUEUE]; MAX_TASKS] = [[0; DEATH_QUEUE]; MAX_TASKS];
static mut SPACE_DEATHS_LEN: [usize; MAX_TASKS] = [0; MAX_TASKS];

pub fn sys_space_watch(watcher: usize, space: u64) -> Result<(), IpcError>
pub fn notify_space_watchers(space: u64)
```

`notify_space_watchers` queues the id for each watcher and wakes it exactly as `notify_watchers` does; the receive path that turns `DEATHS` into `TAG_TASK_DIED` also turns `SPACE_DEATHS` into `Message { sender: 0, tag: TAG_SPACE_DIED, data: [space, 0, 0, 0, 0, 0] }`. `scheduler::exit_with`, after `notify_watchers(current)`, checks whether any other task with the same `space` is still not `Dead`, and calls `notify_space_watchers(space)` if none is. A watched space with no live task left is answered at once with `Err(DeadTask)`, like `sys_task_watch`. A watcher that dies has its bits cleared from `SPACE_WATCHES` in `cleanup_task_ipc`.

- [x] **Step 4: Threads without `PhysAlloc`.** `quark-rt/src/thread.rs` maps the stack with `syscall::sys_mmap(bottom, stack_pages)` (anonymous memory needs no capability) instead of `sys_phys_alloc` + `sys_map_phys`, and unmaps it on failure.

- [x] **Step 5: The VFS.** `handles::alloc` takes the owner's space and calls `sys_space_watch(space)` instead of `sys_task_watch`; every `get_handle(handle, sender)` becomes `get_handle(handle, space_of(sender))`; the dispatch arm is:

```rust
quark_rt::ipc::TAG_SPACE_DIED if sender == 0 => client_died(msg.data[0]),
```

and `TAG_TASK_DIED` is no longer used by the VFS. `space_of` is `syscall::sys_task_space(sender).unwrap_or(0)`; a sender without a space (0) owns nothing and gets `INVALID_HANDLE`.

- [x] **Step 6: Documents.** `docs/abi.md`: rows 107 and 108 in the task table, ABI 2.2 row in "What each minor of 2 added" ("`SYS_TASK_SPACE` (107) and `SYS_SPACE_WATCH` (108): a program's identity is its address space…"), and the version line. `docs/vfs.md`: "A handle belongs to the program that opened it — every thread of it may use it — and the server closes a program's handles when its last task dies."

- [x] **Step 7: Verify.** Build; relink C suites (the layer changed only if `pread64` was added); image; boot: `runtests /etc/libc.tests` (six now), `dtest spaces`, `dtest`, `hello` (threads still start), `wm hello` (threads now start without a manifest grant — Task 13 fixes the grant too). `check-rootfs.sh`.

- [x] **Step 8: Commit.** quark: "A program is its address space"; explosion: "threadfile".

**Done.** Found on the way: a thread started with an empty CSpace and no
descriptors, so it could call no server at all — not even the VFS about a file
its program had opened. `SYS_TASK_START` into the caller's own address space
now copies the creator's capabilities and descriptors into the slots it left
empty (poll sets and sockets excepted: neither counts its holders) and its
band. A copy, not a share — recorded as a known gap. The libc VFS client looks
the server up again when a call fails, since a thread may lack a capability
its program gained after it started. `set_fd` now closes what it replaces
(dup2 semantics), a poll set can no longer be copied, and `dup2(fd, fd)` is a
no-op. dtest: 221 passed; libc.tests 6/6; e2fsck clean.

---

### Task 3: Random numbers, and `/dev`

**Files:**
- Create: `quark/src/random.rs` (ChaCha20 generator, seeding); Modify: `quark/src/main.rs` (`mod random;`, `random::init()` after `rtc::init()`), `quark/src/pit.rs` (mix the TSC into the pool on each tick), `quark/src/syscall.rs` (`SYS_GETRANDOM` 116), `quark/src/cpu.rs` (CPUID leaf 1 ECX bit 30, leaf 7 EBX bit 18)
- Modify: `quark/user/quark-rt/src/syscall.rs`, Create: `quark/user/quark-rt/src/random.rs`
- Create: `quark/user/vfs/src/devices.rs`; Modify: `quark/user/vfs/src/main.rs`, `quark/user/vfs/src/handles.rs` (`FsFileData::Device`, `FsFileData::DevDir`), `quark/user/vfs/src/protocol.rs` (`ERR_NO_SPACE = 14`)
- Modify: `quark/user/linux-abi/src/syscall.c` (`getrandom` 318), `quark/user/linux-abi/src/files.c` (`ENOSPC`, character devices in `fill_stat`), `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/include/errno.h` (`ENOSPC` exists; add nothing else)
- Modify: `rust/library/std/src/sys/random/quark.rs`
- Modify: `explosion/tools/populate-ext.sh` (`mkdir dev`), `explosion/Makefile` (FAT32 root gets `dev`)
- Create: `explosion/toolchain/tests/randtest.c`; Modify: `explosion/toolchain/tests/libc.tests`
- Modify: `quark/docs/abi.md`, `quark/docs/vfs.md`

**Interfaces:**
- Produces: `SYS_GETRANDOM = 116` (arg0 = buffer, arg1 = length, arg2 = flags, ignored → bytes written, at most 1 MiB a call, or `u64::MAX`). `quark_rt::syscall::sys_getrandom(buf: &mut [u8]) -> Result<usize, ()>`; `quark_rt::random::fill(buf: &mut [u8]) -> Result<(), ()>` (loops over short calls). VFS: `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/random`, `/dev/urandom`, and `/dev` as a directory listing them; `ERR_NO_SPACE = 14`. C: `QUARK_VFS_NO_SPACE 14`.

The generator is ChaCha20 (RFC 8439) with fast key erasure: each request draws a keystream block from the current key, uses the first 32 bytes as the next key and hands out what follows. It is seeded at boot from RDSEED, else RDRAND, plus the TSC, the PIT count and the RTC time; each timer tick folds the TSC into a 64-bit pool, and every request mixes the pool into the key first. With neither instruction, the kernel says so on serial (`[random] no RDRAND or RDSEED; seeded from timing`), because that seed is guessable.

- [x] **Step 1: The failing test.** `randtest.c` (runs on Linux as it is):

```c
/* Random bytes from the kernel, and the devices every C program expects. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/random.h>
#include <sys/stat.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

static int spread(const unsigned char *p, size_t n) {
    int seen[256] = {0}, distinct = 0, most = 0;
    for (size_t i = 0; i < n; i++) {
        seen[p[i]]++;
    }
    for (int i = 0; i < 256; i++) {
        distinct += seen[i] != 0;
        most = seen[i] > most ? seen[i] : most;
    }
    return distinct > 200 && most < 64;
}

int main(void) {
    unsigned char a[64], b[64], page[4096];
    printf("random:\n");
    check("getrandom fills a buffer", getrandom(a, sizeof a, 0) == sizeof a);
    check("and again, differently", getrandom(b, sizeof b, 0) == sizeof b && memcmp(a, b, sizeof a));
    int fd = open("/dev/urandom", O_RDONLY);
    check("/dev/urandom opens", fd >= 0);
    check("and reads a page that looks random",
          fd >= 0 && read(fd, page, sizeof page) == (ssize_t)sizeof page && spread(page, sizeof page));
    close(fd);
    struct stat st;
    check("/dev/null is a character device", stat("/dev/null", &st) == 0 && S_ISCHR(st.st_mode));
    fd = open("/dev/null", O_RDWR);
    check("/dev/null swallows writes", fd >= 0 && write(fd, "abc", 3) == 3);
    check("and reads as empty", fd >= 0 && read(fd, page, 16) == 0);
    close(fd);
    fd = open("/dev/zero", O_RDONLY);
    memset(page, 1, 32);
    int zeros = fd >= 0 && read(fd, page, 32) == 32;
    for (int i = 0; i < 32 && zeros; i++) {
        zeros = page[i] == 0;
    }
    check("/dev/zero reads zeroes", zeros);
    close(fd);
    fd = open("/dev/full", O_WRONLY);
    errno = 0;
    check("/dev/full is always full", fd >= 0 && write(fd, "x", 1) == -1 && errno == ENOSPC);
    close(fd);
    DIR *d = opendir("/dev");
    int names = 0;
    struct dirent *e;
    while (d && (e = readdir(d))) {
        names += !strcmp(e->d_name, "null") + !strcmp(e->d_name, "zero") +
                 !strcmp(e->d_name, "full") + !strcmp(e->d_name, "random") +
                 !strcmp(e->d_name, "urandom");
    }
    if (d) {
        closedir(d);
    }
    check("/dev lists all five", names == 5);
    printf("randtest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

Added to `libc.tests`. Run it on the host (`cc -O2 randtest.c && ./a.out`): all ok.

- [x] **Step 2: Run it on Quark.** Expected: `getrandom` fails (ENOSYS) and `/dev/urandom` does not open.

- [x] **Step 3: The kernel.** `random.rs`:

```rust
/// One ChaCha20 block (RFC 8439 §2.3): `out` is the keystream for `key`,
/// `counter` and `nonce`.
fn block(key: &[u8; 32], counter: u32, nonce: &[u8; 12], out: &mut [u8; 64]) {
    const C: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];
    let mut s = [0u32; 16];
    s[..4].copy_from_slice(&C);
    for i in 0..8 {
        s[4 + i] = u32::from_le_bytes(key[i * 4..i * 4 + 4].try_into().unwrap());
    }
    s[12] = counter;
    for i in 0..3 {
        s[13 + i] = u32::from_le_bytes(nonce[i * 4..i * 4 + 4].try_into().unwrap());
    }
    let mut w = s;
    fn qr(w: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
        w[a] = w[a].wrapping_add(w[b]); w[d] = (w[d] ^ w[a]).rotate_left(16);
        w[c] = w[c].wrapping_add(w[d]); w[b] = (w[b] ^ w[c]).rotate_left(12);
        w[a] = w[a].wrapping_add(w[b]); w[d] = (w[d] ^ w[a]).rotate_left(8);
        w[c] = w[c].wrapping_add(w[d]); w[b] = (w[b] ^ w[c]).rotate_left(7);
    }
    for _ in 0..10 {
        qr(&mut w, 0, 4, 8, 12); qr(&mut w, 1, 5, 9, 13);
        qr(&mut w, 2, 6, 10, 14); qr(&mut w, 3, 7, 11, 15);
        qr(&mut w, 0, 5, 10, 15); qr(&mut w, 1, 6, 11, 12);
        qr(&mut w, 2, 7, 8, 13); qr(&mut w, 3, 4, 9, 14);
    }
    for i in 0..16 {
        out[i * 4..i * 4 + 4].copy_from_slice(&w[i].wrapping_add(s[i]).to_le_bytes());
    }
}
```

`static STATE: Spinlock<State>` (`State { key: [u8; 32], counter: u32 }`) — use whatever the kernel already uses for a lock around shared state (`irq_save`/`irq_restore` as `ipc.rs` does, since this runs from syscalls and the timer). `pub fn fill(out: &mut [u8])` takes the pool into the nonce, generates blocks until `out` is full (the first block's first 32 bytes become the next key, and are not handed out), and zeroes its temporaries. A unit check at boot compares one block against RFC 8439 §2.3.2's test vector (key `00 01 … 1f`, counter 1, nonce `00 00 00 09 00 00 00 4a 00 00 00 00`: the output starts `10 f1 e7 e4 d1 3b 59 15`) and prints `[random] ChaCha20 self-test failed` and panics if it does not match — a wrong generator must not boot.

`SYS_GETRANDOM` validates the user buffer writable, fills a 4096-byte kernel buffer at a time and copies it out under `UserAccess`, as `fd_read_ipc` does.

- [x] **Step 4: quark-rt and std.** `random::fill` loops `sys_getrandom` until the buffer is full. The fork's `random/quark.rs` becomes `quark_rt::random::fill(bytes).expect("the kernel has no random numbers")` — failing loudly rather than handing out zeroes.

- [x] **Step 5: The devices.** `devices.rs` in the VFS:

```rust
#[derive(Clone, Copy, PartialEq)]
pub enum Device { Null, Zero, Full, Random, Urandom }

pub const NAMES: [(&[u8], Device); 5] = [
    (b"null", Device::Null), (b"zero", Device::Zero), (b"full", Device::Full),
    (b"random", Device::Random), (b"urandom", Device::Urandom),
];

/// `Some(None)` for `/dev` itself, `Some(Some(d))` for a device, `None` for
/// anything the filesystem answers.
pub fn lookup(path: &[u8]) -> Option<Option<Device>>
```

`lookup` strips a leading `/`, repeated and trailing slashes; `dev` alone is the directory; `dev/<name>` is a device; `dev/<anything else>` is `ERR_NOT_FOUND`. `handle_open` asks `devices::lookup` first. A device handle: `is_dir` false, mode `0o020666`, id `0xFFFF_FF00 + index`, readable and writable by everybody. `READ`: `Null` → 0 bytes; `Zero` and `Full` → zeroes; `Random`/`Urandom` → `sys_getrandom` into `CLIENT_BUF`, then `lend_out`. `WRITE`: `Full` → `ERR_NO_SPACE`, the rest accept everything. `STAT` fills the record from those values, times from `quark_rt::syscall::unix_time()`. `READDIR_BULK` on `/dev` lists the five with `DT_CHR` (2). The filesystem must have a `/dev` directory for `ls /` to show it; the image makes one.

- [x] **Step 6: The Linux layer.** `getrandom` (318) calls `SYS_GETRANDOM` in a loop until the request is met; `fill_stat` keeps `S_IFCHR` from the server's mode; `vfs_errno` maps `QUARK_VFS_NO_SPACE` to `-LX_ENOSPC` (28).

- [x] **Step 7: Verify.** `make` (check-abi); layer and suites; the fork (`hello` rebuilds); image (`dev` exists); boot: `runtests /etc/libc.tests` (randtest ok), `ls /dev`, `dtest` (a new check in `service`: two `sys_getrandom` buffers differ), `hello`. Serial shows no `no RDRAND` line under `-cpu max`. `check-rootfs.sh`.

- [x] **Step 8: Commit.** quark: "Random numbers, and /dev"; rust: "std: random bytes from the kernel"; explosion: "randtest; the root has /dev".

**Done.** The generator adds `SYS_GETRANDOM` at ABI 2.3 rather than folding
into 2.2, which was already pushed. The dtest checks are a section of their
own, `random`, rather than part of `service`. `hello` makes a `HashMap`, which
is what reaches std's random path (nothing did before). The FAT32 root gets
`/dev` too, and the Linux layer reports Linux's device numbers. A path is
checked against `/dev` after `.` and `..` are taken lexically; Task 5's
symbolic links and Task 6's relative paths must keep a link or a working
directory that leads into `/dev` finding the devices. libc.tests 7/7; dtest
227 passed; e2fsck clean; FAT32 boot lists `/dev` and passes randtest.

---

### Task 4: Hard links

**Files:**
- Modify: `quark/user/vfs/src/protocol.rs` (`TAG_LINK = 15`, `ERR_TOO_MANY_LINKS = 18`), `quark/user/vfs/src/ext2_ops.rs` (`link`), `quark/user/vfs/src/main.rs` (dispatch: `TAG_LINK` joins `handle_namespace`, transacted)
- Modify: `quark/user/quark-rt/src/vfs.rs` (`link`), `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/quark.c` (`quark_vfs_link`), `quark/user/linux-abi/src/files.c` (`__quark_link`), `quark/user/linux-abi/src/syscall.c` (`link` 86, `linkat` 265), `quark/user/linux-abi/src/abi.h`
- Modify: `quark/user/dtest/src/main.rs` (section `files`), `explosion/toolchain/tests/filetest.c`
- Modify: `quark/docs/vfs.md`

**Interfaces:**
- Produces: `TAG_LINK` — `[from_len, to_len]`, both paths lent end to end as `RENAME` lends them; errors `NOT_FOUND`, `EXISTS`, `IS_DIR` (the source is a directory), `TOO_MANY_LINKS`, `PERMISSION`, `NOT_SUPPORTED` (FAT32). `quark_rt::vfs::link(vfs: usize, from: &[u8], to: &[u8]) -> Result<(), u64>`; `int quark_vfs_link(const char *from, const char *to)`; `long __quark_link(const char *from, const char *to)`.

- [x] **Step 1: The failing test.** In `filetest.c`, the `link is not offered` check becomes a block (and `clear_leftovers` also unlinks `TESTDIR "/hard"`):

```c
    #define HARD TESTDIR "/hard"
    struct stat h1, h2;
    check("link makes a second name", link(FILE_A, HARD) == 0);
    check("for the same file", stat(FILE_A, &h1) == 0 && stat(HARD, &h2) == 0 &&
          h1.st_ino == h2.st_ino && h1.st_nlink == 2);
    fd = open(HARD, O_WRONLY);
    check("a write through one name", fd >= 0 && pwrite(fd, "L", 1, 0) == 1);
    close(fd);
    fd = open(FILE_A, O_RDONLY);
    check("shows through the other", fd >= 0 && read(fd, buf, 1) == 1 && buf[0] == 'L');
    close(fd);
    check("unlinking one name", unlink(HARD) == 0);
    check("leaves the other", stat(FILE_A, &h1) == 0 && h1.st_nlink == 1);
    check("link refuses a directory", link(TESTDIR, HARD) == -1 && errno == EPERM);
    check("and a name that is taken", link(FILE_A, FILE_B) == -1 && errno == EEXIST);
```

On the host every check passes (Linux gives `EPERM` for a directory). dtest `files` gains `vfs::link` then `vfs::stat` of both names showing one id and two links.

- [x] **Step 2: Run it.** Expected: `link makes a second name` fails with `EPERM` (today's answer).

- [x] **Step 3: The server.** `ext2_ops::link(e2, from, to, uid, gid)`: resolve `from` without following a final symbolic link (Task 5 makes that distinction; until then there are none); a directory is `ERR_IS_DIR`; `i_links_count >= 65000` is `ERR_TOO_MANY_LINKS`; `split_path(to)` resolves the new parent, which must be writable (`writable_dir`) and must not already hold the name (`ERR_EXISTS`); `ext2_dir::create_dir_entry(e2, parent_ino, &mut parent, name, ino, file_type_of(&inode))`; `i_links_count += 1`; `i_ctime = now()`; write the inode; the parent's times change as `create` changes them. FAT32 answers `NOT_SUPPORTED`.

- [x] **Step 4: Clients.** `link` and `linkat` (flags: `AT_SYMLINK_FOLLOW` 0x400 accepted, `AT_EMPTY_PATH` 0x1000 is `-EINVAL`; directory descriptors other than `AT_FDCWD` wait for Task 6 and answer `-ENOSYS` until then). The layer maps `IS_DIR` and `NOT_SUPPORTED` to `EPERM` for `link` — that is Linux's answer for both — and `TOO_MANY_LINKS` to `EMLINK` (31).

- [x] **Step 5: Verify.** Build, layer, suites, image; boot `runtests /etc/libc.tests`, `dtest files`; `check-rootfs.sh`; the same on `make hd-ext4`. fontconfig's lock now takes the `link` path: `runtests /etc/fontconfig.tests` passes and `/var/cache/fontconfig` has no `.LCK` or `.TMP-` leftovers.

- [x] **Step 6: Commit.** quark: "Hard links"; explosion: "filetest: hard links".

**Done.** Step 2's failure was not re-run: the check it replaces asserted
exactly that answer (EPERM) on Quark in every earlier run. The link count is
written before the new entry, so a failure between leaves a count one short,
which e2fsck mends, rather than an entry the count does not know. The limit is
32000 on ext2 and 65000 on ext4. filetest ok and `dtest files` 25/0 on ext2
and ext4; fontconfig.tests 4/4 and its cache directory has no lock leftovers;
e2fsck clean on both.

---

### Task 5: Symbolic links

**Files:**
- Modify: `quark/user/vfs/src/protocol.rs` (`TAG_SYMLINK = 16`, `TAG_READLINK = 17`, `OPEN_NOFOLLOW = 16`, `ERR_LOOP = 15`), `quark/user/vfs/src/ext2_dir.rs` (`resolve` replaces `resolve_path`), `quark/user/vfs/src/ext2_ops.rs` (`symlink`, `read_link`; `split_path` follows links in the parent), `quark/user/vfs/src/ext2.rs` (`is_fast_symlink`), `quark/user/vfs/src/ext4.rs` (a fast symlink has no extent root), `quark/user/vfs/src/main.rs` (dispatch; `OPEN_NOFOLLOW` handles)
- Modify: `quark/user/quark-rt/src/vfs.rs` (`symlink`, `readlink`, `lstat`, `OPEN_NOFOLLOW`), `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/quark.c`, `quark/user/linux-abi/src/files.c`, `quark/user/linux-abi/src/syscall.c` (`symlink` 88, `symlinkat` 266, `readlink` 89, `readlinkat` 267, `lstat` 6, `newfstatat` with `AT_SYMLINK_NOFOLLOW` 0x100, `O_NOFOLLOW` 0400000)
- Modify: `quark/user/ls/src/main.rs` (`name -> target`)
- Modify: `explosion/tools/populate-ext.sh` (staged symlinks become `symlink` commands)
- Create: `explosion/toolchain/tests/linktest.c`; Modify: `explosion/toolchain/tests/libc.tests`; Modify: `explosion/toolchain/tests/dirtest.c` (its `readlink` checks now expect a real answer for a link)
- Modify: `quark/docs/vfs.md`

**Interfaces:**
- Produces: `TAG_SYMLINK` — `[target_len, path_len]`, the target then the new path lent end to end; the target is stored as given (up to 4095 bytes, not resolved). `TAG_READLINK` — `[path_len]`, one buffer lent for reading *and* writing: the path first, then room; the target is written starting at offset `path_len`, and the reply is `[target_len]`; `INVALID_PATH` if the path is not a link. `OPEN_NOFOLLOW`: a final component that is a link opens the link itself, a handle that answers `STAT` (mode `S_IFLNK | 0777`, size = target length) and nothing else (`NOT_SUPPORTED`). `ERR_LOOP` after 40 links in one lookup. `READDIR_BULK` types a link `DT_LNK` (10).
- Produces (quark-rt): `vfs::symlink(vfs, target: &[u8], path: &[u8]) -> Result<(), u64>`, `vfs::readlink(vfs, path: &[u8], out: &mut [u8]) -> Result<usize, u64>`, `vfs::lstat(vfs, path: &[u8]) -> Result<Stat, u64>`. C: `int quark_vfs_symlink(const char *target, const char *path)`, `long quark_vfs_readlink(const char *path, char *out, unsigned long len)` (length or negative error).
- Produces (ext2): `ext2_dir::resolve(e2: &Ext2State, base: u32, path: &[u8], uid: u32, gid: u32, follow_last: bool) -> Result<(u32, Ext2Inode, u32), u64>` — `base` is the directory a relative path starts from (the root until Task 6); every existing caller of `resolve_path` becomes `resolve(e2, EXT2_ROOT_INO, path, uid, gid, true)`.

On disk: a target shorter than 60 bytes is a *fast* link, kept in `i_block` with `i_blocks = 0` and no `EXTENTS_FL`, even on ext4; a longer one is a *slow* link, one data block written through the ordinary write path (an extent on ext4). `e2fsck` checks both, which is the point of making both.

- [x] **Step 1: The failing test.** `linktest.c` (runs on Linux as it is):

```c
/* Symbolic links: made, read, followed where they should be, and not where
   they should not. `linktest keep` leaves one fast and one slow link in
   /tmp/linktest-kept for e2fsck to look at. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define T "/tmp/linktest"
#define LONG_TARGET T "/a-directory-name-long-enough/that-the-target-cannot-fit-in-sixty-bytes/file"

static void tidy(void) {
    unlink(T "/fast"); unlink(T "/slow"); unlink(T "/dangling"); unlink(T "/rel");
    unlink(T "/dirlink"); unlink(T "/loop-a"); unlink(T "/loop-b"); unlink(T "/renamed");
    unlink(LONG_TARGET);
    rmdir(T "/a-directory-name-long-enough/that-the-target-cannot-fit-in-sixty-bytes");
    rmdir(T "/a-directory-name-long-enough");
    unlink(T "/sub/inner"); rmdir(T "/sub");
    unlink(T "/file"); rmdir(T);
}

int main(int argc, char **argv) {
    int keep = argc > 1 && !strcmp(argv[1], "keep");
    char buf[PATH_MAX];
    struct stat st;
    printf("symbolic links:\n");
    tidy();
    mkdir(T, 0755);
    int fd = open(T "/file", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd >= 0) { write(fd, "target", 6); close(fd); }
    mkdir(T "/a-directory-name-long-enough", 0755);
    mkdir(T "/a-directory-name-long-enough/that-the-target-cannot-fit-in-sixty-bytes", 0755);
    fd = open(LONG_TARGET, O_WRONLY | O_CREAT, 0644);
    if (fd >= 0) { write(fd, "far", 3); close(fd); }
    mkdir(T "/sub", 0755);
    fd = open(T "/sub/inner", O_WRONLY | O_CREAT, 0644);
    if (fd >= 0) close(fd);

    check("a short link", symlink(T "/file", T "/fast") == 0);
    check("reads back", readlink(T "/fast", buf, sizeof buf) == (ssize_t)strlen(T "/file") &&
          !memcmp(buf, T "/file", strlen(T "/file")));
    check("a long link", symlink(LONG_TARGET, T "/slow") == 0);
    check("reads back too", readlink(T "/slow", buf, sizeof buf) == (ssize_t)strlen(LONG_TARGET) &&
          !memcmp(buf, LONG_TARGET, strlen(LONG_TARGET)));
    check("readlink stops at the buffer", readlink(T "/slow", buf, 4) == 4);
    check("stat follows", stat(T "/fast", &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 6);
    check("lstat does not", lstat(T "/fast", &st) == 0 && S_ISLNK(st.st_mode) &&
          st.st_size == (off_t)strlen(T "/file"));
    fd = open(T "/slow", O_RDONLY);
    check("open follows", fd >= 0 && read(fd, buf, 3) == 3 && !memcmp(buf, "far", 3));
    close(fd);
    check("a relative link", symlink("sub/inner", T "/rel") == 0 && stat(T "/rel", &st) == 0);
    check("a link to a directory", symlink("sub", T "/dirlink") == 0 &&
          stat(T "/dirlink/inner", &st) == 0);
    DIR *d = opendir(T "/dirlink");
    check("opens as that directory", d != NULL);
    if (d) closedir(d);
    check("a dangling link", symlink(T "/nowhere", T "/dangling") == 0);
    check("stat says it leads nowhere", stat(T "/dangling", &st) == -1 && errno == ENOENT);
    check("lstat still sees it", lstat(T "/dangling", &st) == 0 && S_ISLNK(st.st_mode));
    symlink("loop-b", T "/loop-a");
    symlink("loop-a", T "/loop-b");
    errno = 0;
    check("a loop is an error", open(T "/loop-a", O_RDONLY) == -1 && errno == ELOOP);
    errno = 0;
    check("O_NOFOLLOW refuses a link", open(T "/fast", O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP);
    check("a link cannot be made over a name", symlink("x", T "/file") == -1 && errno == EEXIST);
    check("readlink of a file is EINVAL", readlink(T "/file", buf, sizeof buf) == -1 && errno == EINVAL);
    int lnk = 0;
    d = opendir(T);
    struct dirent *e;
    while (d && (e = readdir(d))) {
        if (!strcmp(e->d_name, "fast")) lnk = e->d_type == DT_LNK;
    }
    if (d) closedir(d);
    check("a directory lists a link as a link", lnk);
    char *real = realpath(T "/dirlink/inner", NULL);
    check("realpath resolves through a link", real && !strcmp(real, T "/sub/inner"));
    free(real);
    check("rename moves the link", rename(T "/rel", T "/renamed") == 0 &&
          readlink(T "/renamed", buf, sizeof buf) == 9);
    check("unlink removes the link", unlink(T "/fast") == 0 && stat(T "/file", &st) == 0);
    if (keep) {
        mkdir("/tmp/linktest-kept", 0755);
        unlink("/tmp/linktest-kept/fast");
        unlink("/tmp/linktest-kept/slow");
        symlink("/etc/passwd", "/tmp/linktest-kept/fast");
        symlink("/tmp/linktest-kept/a-name-that-is-well-over-sixty-bytes-long-so-it-needs-a-block", "/tmp/linktest-kept/slow");
    } else {
        unlink("/tmp/linktest-kept/fast");
        unlink("/tmp/linktest-kept/slow");
        rmdir("/tmp/linktest-kept");
    }
    tidy();
    check("tidy up", lstat(T, &st) == -1);
    printf("linktest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`libc.tests` gains `linktest`. `dirtest.c`'s `readlink of a file is EINVAL` stays; nothing in it expected links to be refused.

- [x] **Step 2: Run it.** Host: all ok. Quark: the first check fails (`symlink` is `ENOSYS`).

- [x] **Step 3: Resolution.** `ext2_dir::resolve` walks a working copy of the path in a 4096-byte buffer. At each component it looks the name up; if the entry is a link and it is not the last component (or it is, and `follow_last`), it reads the target (`ext2_ops::read_link`), counts one expansion (the 41st is `ERR_LOOP`), and replaces the consumed part with `target` + `/` + the rest (`ERR_NAME_TOO_LONG` if that exceeds 4095 bytes). An absolute target restarts from the root; a relative one continues from the directory holding the link. `..` is the directory's own `..` entry, as before. `split_path` resolves the parent with `follow_last = true` and never follows the final name.

- [x] **Step 4: Making and reading links.** `ext2_ops::symlink(e2, target, path, uid, gid)` allocates an inode (`S_IFLNK | 0o777`, owner the caller, one link), writes a fast link into `i_block` or a slow one through `ext2::write_file_data` (which gives an ext4 file its extent root), and enters it with `FT_SYMLINK`. `read_link(e2, &inode) -> Result<(usize, [u8; 4096]), u64>` reads `i_block`'s bytes for a fast link (`ext2::is_fast_symlink`: `i_size < 60 && i_blocks == 0`) and the file's data otherwise. ext4's `create` path must not call `init_extent_root` for a fast link. `TAG_SYMLINK` and `TAG_READLINK` go through `handle_namespace` (`TAG_SYMLINK` transacted). `OPEN_NOFOLLOW` resolves with `follow_last = false`; a link found there gets a handle with `FsFileData::Ext2 { inode_num }`, `is_dir = false` and `link = true`, and `READ`, `WRITE`, `TRUNCATE` and `READDIR_BULK` answer it `NOT_SUPPORTED`.

- [x] **Step 5: Clients.** quark-rt's three functions; `quark_vfs_symlink`, `quark_vfs_readlink`; in the layer: `symlink`/`symlinkat`, `readlink`/`readlinkat` (copy at most `bufsiz`, no terminating NUL, `EINVAL` for a non-link), `lstat` and `newfstatat(…, AT_SYMLINK_NOFOLLOW)` open with `OPEN_NOFOLLOW`, stat and close; `open` with `O_NOFOLLOW` whose result is a link closes it and answers `-ELOOP`; `vfs_errno` maps `QUARK_VFS_LOOP` to `-LX_ELOOP` (40). `ls` prints `name -> target` for an entry of type `DT_LNK`.

- [x] **Step 6: Links in the image.** `populate-ext.sh` writes `symlink <path> <target>` for every staged symbolic link (`find usr etc var -type l`, target from `readlink`) after the directories and before the files, and its checking pass looks for them too. The FAT32 rule leaves links out (`find -type f` never matched them).

- [x] **Step 7: Verify.** Build, layer, suites, image; boot `runtests /etc/libc.tests`, `linktest keep`, `ls -l`-style `ls /tmp/linktest-kept`, `dtest files`; `check-rootfs.sh` (the kept links are checked); the same on `make hd-ext4`; then boot once more and run `linktest` (without `keep`) so nothing is left.

- [x] **Step 8: Commit.** quark: "Symbolic links"; explosion: "linktest; links in the image".

**Done**, with these differences from the steps above. `READLINK` is
`[path_len, room]`: a server cannot ask the kernel how much was lent. `LINK`
takes `data[2]` bit 0 to follow a link at its source, which is what `linkat`'s
`AT_SYMLINK_FOLLOW` asks for. A target must fit its block with a NUL (1023
bytes on this image), since e2fsck holds a longer one broken. `resolve`
returns `Found::Inode` or `Found::Device`: the lookup answers for the root's
`dev` directory itself, so a link into `/dev` reaches a device (linktest has a
check for it), and `writable_dir` refuses `/dev`, so nothing is made there
through a link either; the lexical match stays for FAT32 and roots with no
`/dev`. `release_inode` must not free a fast link's `i_block`, which is text.
`ls` finds the VFS with `lookup_retry` instead of spinning on `sys_yield`.
Step 2's failure was not re-run (the layer answered `symlink` with ENOSYS).
Kept links checked with debugfs on both filesystems (ext4: the fast one has no
extents flag, the slow one one extent); libc.tests 8/8, `dtest files` 25/0 and
e2fsck clean on ext2 and ext4.

---

### Task 6: Working directories

**Files:**
- Create: `quark/user/vfs/src/cwd.rs` (per-program current directory); Modify: `quark/user/vfs/src/protocol.rs` (`TAG_CHDIR = 18`, `TAG_FCHDIR = 19`, `TAG_GETCWD = 20`, `TAG_GIVE_CWD = 21`), `quark/user/vfs/src/main.rs` (every path request resolves from a base), `quark/user/vfs/src/ext2_dir.rs` (`path_of(e2, ino) -> Result<(usize, [u8; 4096]), u64>`), `quark/user/vfs/src/handles.rs` (`inode_is_open` counts current directories)
- Modify: `quark/user/quark-rt/src/vfs.rs` (`chdir`, `fchdir`, `getcwd`, `give_cwd`; every path call sends a base word)
- Modify: `quark/user/qsh/src/main.rs` (its own `CWD` and `resolve_path` go; `cd`, the prompt and `pwd` use the VFS; children are given the directory), `quark/user/runtests/src/main.rs`, `quark/user/wm/src/main.rs`, `quark/user/login/src/main.rs` (children are given the directory)
- Modify: `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/quark.c` (`*_at` variants), `quark/user/linux-abi/src/files.c`, `quark/user/linux-abi/src/syscall.c` (`chdir` 80, `fchdir` 81, `getcwd` 79, and `dirfd` in `openat`, `mkdirat`, `unlinkat`, `renameat`, `renameat2`, `linkat`, `symlinkat`, `readlinkat`, `faccessat`, `faccessat2`, `newfstatat`)
- Modify: `rust/library/std/src/sys/pal/quark/os.rs` (`getcwd`, `chdir`)
- Modify: `quark/user/dchild/src/main.rs` (mode `cwd`), `quark/user/dtest/src/main.rs` (section `files`)
- Create: `explosion/toolchain/tests/cwdtest.c`; Modify: `explosion/toolchain/tests/libc.tests`, `explosion/toolchain/tests/dirtest.c` (its `getcwd` check expects the directory it chose)
- Modify: `quark/docs/vfs.md`

**Interfaces:**
- Produces (protocol): every request that names a path takes a *base* in `data[5]`: 0 means the program's current directory, `h + 1` means the open directory handle `h`; an absolute path ignores it. `RENAME` and `LINK` take the second path's base in `data[4]`. `CHDIR` `[len]` + lent path; `FCHDIR` `[handle]`; `GETCWD` with 4096 bytes lent for writing → `[len]` (`NOT_FOUND` if the directory was removed); `GIVE_CWD` `[child_tid]` — allowed when the caller's program is the child's parent's program and the child is another program.
- Produces (quark-rt): `vfs::chdir(vfs: usize, path: &[u8]) -> Result<(), u64>`, `vfs::fchdir(vfs: usize, handle: usize) -> Result<(), u64>`, `vfs::getcwd(vfs: usize, out: &mut [u8]) -> Result<usize, u64>`, `vfs::give_cwd(vfs: usize, child: usize) -> Result<(), u64>`.
- Produces (C): `int quark_vfs_open_at(unsigned long base, const char *path, unsigned long flags, struct quark_vfs_file *out)` and the same `_at` form for `mkdir`, `unlink`, `rmdir`, `rename` (two bases), `link` (two bases), `symlink`, `readlink`; the plain names pass base 0. `int quark_vfs_chdir(const char *)`, `int quark_vfs_fchdir(unsigned long handle)`, `long quark_vfs_getcwd(char *out, unsigned long len)`, `int quark_vfs_give_cwd(unsigned long child)`.

The server holds a program's directory by inode, as Linux does, so renaming a directory above it changes what `getcwd` says and nothing else. A program nobody gave a directory is at `/`. A current directory counts as an open reference: removing it leaves the inode until the program leaves it, and `getcwd` then answers `NOT_FOUND`. On FAT32, which cannot rename anything, the directory is kept as its path.

- [x] **Step 1: The failing tests.** `cwdtest.c` (runs on Linux):

```c
/* A working directory: relative paths, *at calls, and inheritance. */
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define D "/tmp/cwdtest"

int main(void) {
    char buf[PATH_MAX];
    struct stat st;
    printf("working directory:\n");
    unlink(D "/sub/file"); unlink(D "/sub/renamed"); unlink(D "/rel"); rmdir(D "/sub");
    rmdir(D "/gone"); rmdir(D);
    check("chdir to /", chdir("/") == 0);
    check("getcwd says /", getcwd(buf, sizeof buf) && !strcmp(buf, "/"));
    check("mkdir by absolute path", mkdir(D, 0755) == 0);
    check("chdir into it", chdir(D) == 0 && getcwd(buf, sizeof buf) && !strcmp(buf, D));
    check("mkdir by relative path", mkdir("sub", 0755) == 0 && stat(D "/sub", &st) == 0);
    int fd = open("sub/file", O_WRONLY | O_CREAT, 0644);
    check("create by relative path", fd >= 0 && write(fd, "cwd", 3) == 3);
    close(fd);
    check("stat by relative path", stat("sub/file", &st) == 0 && st.st_size == 3);
    int dfd = open("sub", O_RDONLY | O_DIRECTORY);
    check("open a directory", dfd >= 0);
    fd = openat(dfd, "file", O_RDONLY);
    check("openat reads from it", fd >= 0 && read(fd, buf, 3) == 3 && !memcmp(buf, "cwd", 3));
    close(fd);
    check("renameat within it", renameat(dfd, "file", dfd, "renamed") == 0 &&
          fstatat(dfd, "renamed", &st, 0) == 0);
    check("chdir ..", chdir("..") == 0 && getcwd(buf, sizeof buf) && !strcmp(buf, "/tmp"));
    check("fchdir back", fchdir(dfd) == 0 && getcwd(buf, sizeof buf) && !strcmp(buf, D "/sub"));
    check("unlinkat by a relative directory", unlinkat(AT_FDCWD, "renamed", 0) == 0);
    close(dfd);
    check("a directory named by a file is refused", chdir(D "/nothing") == -1 && errno == ENOENT);
    check("getcwd too small is ERANGE", getcwd(buf, 3) == NULL && errno == ERANGE);
    check("into a directory that then goes", mkdir(D "/gone", 0755) == 0 && chdir(D "/gone") == 0 &&
          rmdir(D "/gone") == 0);
    errno = 0;
    check("getcwd says it has gone", getcwd(buf, sizeof buf) == NULL && errno == ENOENT);
    check("and chdir away still works", chdir("/") == 0);
    check("tidy up", rmdir(D "/sub") == 0 && rmdir(D) == 0);
    printf("cwdtest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

`dirtest.c`'s `the working directory is the root` check first calls `chdir("/")`. dtest `files`: `vfs::chdir(vfs, b"/etc")`, then `vfs::open(vfs, b"passwd")` succeeds, `vfs::getcwd` says `/etc`; a `dchild cwd` child (mode `cwd`: exit 0 if `vfs::open(vfs, b"passwd")` succeeds, else 1) given the directory with `vfs::give_cwd` exits 0; one not given it exits 1; `vfs::chdir(vfs, b"/")` afterwards.

- [x] **Step 2: Run them.** Host: all ok. Quark: `chdir` fails (the layer answers `ENOSYS`).

- [x] **Step 3: The server.** `cwd.rs`:

```rust
pub enum Where { Root, Inode(u32), Path([u8; MAX_PATH], usize) }

const MAX_PROGRAMS: usize = 64;
static mut CWD: [(u64, Where); MAX_PROGRAMS] = /* (0, Root) */;

pub fn get(space: u64) -> &'static Where
pub fn set(space: u64, to: Where) -> Result<(), u64>   // watches the space the first time
pub fn forget(space: u64)                               // on TAG_SPACE_DIED
pub fn holds(ino: u32) -> bool                          // for inode_is_open
```

A path request's starting directory is `base_ino(sender, word)`: a non-zero word must be `h + 1` for an open directory handle of the sender's program (else `INVALID_HANDLE`); zero is `cwd::get(space)`. `ext2_dir::resolve` is called with that inode. `CHDIR` resolves (following links), requires a directory the caller may search (`x`), and sets it; the previous inode is `settle`d, as a closing handle is. `GETCWD` builds the path with `ext2_dir::path_of`: from the inode, read `..`, find the entry in the parent whose inode matches, prepend its name, and repeat until the root; an inode with no links (removed) is `NOT_FOUND`. `GIVE_CWD`: `sys_task_info(child)` gives the parent; `space_of(parent) == space_of(sender)` and `space_of(child) != space_of(sender)` or `PERMISSION`.

- [x] **Step 4: quark-rt, the shell and the spawners.** The four functions. `call_with_path` puts 0 in `data[5]` (and `data[4]`). `qsh` drops `CWD`, `HOME`-as-cwd and `resolve_path`: at start it `chdir`s to its `argv[1]` (the home `login` passes, `/home/root` if missing); `cd` with no argument goes home, `cd DIR` calls `vfs::chdir` and reports `cd: DIR: <reason>` on error; `pwd` prints `vfs::getcwd`; the prompt shows `~` for home as now. `qsh`, `runtests`, `wm` and `login` call `vfs::give_cwd(vfs, info.tid)` before `info.start()`.

- [x] **Step 5: C clients.** `vfs_path_call` in `quark.c` takes the base word(s). The layer's `struct openfile` keeps its handle; `base_of(dirfd)` is 0 for `AT_FDCWD`, `handle + 1` for an open directory, and `-EBADF`/`-ENOTDIR` otherwise. `chdir`, `fchdir` (a directory descriptor's handle) and `getcwd` (the length including the NUL is returned, `ERANGE` when it does not fit) call the server; the `*at` calls pass their base.

- [x] **Step 6: std.** `getcwd` asks `quark_rt::vfs::getcwd` (looking the server up with `quark_rt::nameserver::lookup(b"vfs")`); `chdir` calls `quark_rt::vfs::chdir`; errors map to `io::ErrorKind::NotFound`, `NotADirectory`, `PermissionDenied`.

- [x] **Step 7: Verify.** Build, layer, suites, fork, image; boot: `runtests /etc/libc.tests` (cwdtest ok), `dtest files`, `cd /etc`, `ls` (lists `/etc`), `cat passwd`, `pwd`, `cd /nonexistent` (a message, the prompt stays), `cd` (home), `hello`; `check-rootfs.sh`; the same on `make hd-ext4`; on `make hd-fat32`, `cd /etc` and `cat passwd`.

- [x] **Step 8: Commit.** quark: "Working directories"; rust: "std: the working directory"; explosion: "cwdtest".

**Done**, with one addition the steps missed: a spawner gives the child its
directory before starting it, but a task had no address space, and so no
program, until `SYS_TASK_START`. `SYS_TASK_CREATE_IN` (109, ABI 2.4) makes a
task for an address space the caller created; the kernel now keeps a space id
in every task, set there or at start, and counts a created task as a live
member of its program. `spawn::load` uses it. Also: qsh's `resolve_args`,
which rewrote path-like arguments to absolute ones, is gone, and `ls` with no
argument lists `.`; the prompt and `pwd` ask the server; `hello` prints
`current_dir()`; the orphan table is sized for handles and directories
together; `newfstatat(AT_FDCWD, "", AT_EMPTY_PATH)` stats the directory.
FAT32 answers `FCHDIR` and handle bases with `NOT_SUPPORTED`. Step 2's failure
was not re-run (`chdir` was not answered). libc.tests 9/9 on ext2 and ext4,
`dtest files` 33/0, the shell checks as listed, FAT32 `cd /etc`, `cat passwd`,
`cd /dev`, `ls`; e2fsck clean on both ext images.

---

### Task 7: Record locks

**Files:**
- Create: `quark/user/vfs/src/locks.rs`; Modify: `quark/user/vfs/src/protocol.rs` (`TAG_LOCK = 22`, `LOCK_WAIT = 1`, `LOCK_OFD = 2`, `LOCK_QUERY = 4`, `ERR_WOULD_BLOCK = 16`, `ERR_DEADLOCK = 17`), `quark/user/vfs/src/main.rs` (dispatch, deferred replies, release on close and on death)
- Modify: `quark/user/quark-rt/src/vfs.rs` (`lock`), `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/quark.c` (`quark_vfs_lock`), `quark/user/linux-abi/src/files.c` (`__quark_fcntl` lock commands, `__quark_flock`), `quark/user/linux-abi/src/syscall.c` (`flock` 73)
- Modify: `quark/user/dchild/src/main.rs` (mode `lock PATH`), `quark/user/dtest/src/main.rs` (section `locks`)
- Create: `explosion/toolchain/tests/locktest.c`; Modify: `explosion/toolchain/tests/libc.tests`
- Modify: `quark/docs/vfs.md`

**Interfaces:**
- Produces: `TAG_LOCK` — `[handle, kind, start, len, flags]`; `kind` 0 unlock, 1 shared, 2 exclusive; `len` 0 means to the end of the file and beyond; `flags`: `LOCK_WAIT` (answer when granted), `LOCK_OFD` (owned by the open handle rather than the program), `LOCK_QUERY` (grant nothing; reply `[kind, start, len, holder_space]` of the first conflicting lock, `kind` 0 if none). Errors: `WOULD_BLOCK`, `DEADLOCK`, `INVALID_HANDLE`.
- Produces: `quark_rt::vfs::lock(vfs: usize, handle: usize, kind: u64, start: u64, len: u64, flags: u64) -> Result<[u64; 4], u64>`; `int quark_vfs_lock(unsigned long handle, unsigned long kind, unsigned long start, unsigned long len, unsigned long flags, unsigned long out[4])`.

Semantics are Linux's. A program's (POSIX) locks: its own locks never conflict with each other, a new lock replaces and merges its old ones over the same range, and closing *any* of the program's handles on the file releases all of its locks on that file. An open handle's (OFD, `flock`) locks: they conflict with every other owner, including another handle of the same program, and go when that handle closes. A program's death releases everything it owned and drops its waiters. A waiting request is answered when it can be granted, re-checked whenever a lock on that inode is released; before a program waits, the server follows the waits-for chain (program owners only) and answers `DEADLOCK` if it would come back to the program asking.

- [ ] **Step 1: The failing tests.** `locktest.c` (runs on Linux):

```c
#define _GNU_SOURCE /* F_OFD_* */
/* Record locks and flock, within one program. dtest checks them across two. */
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define F "/tmp/locktest"

static int ofd(int fd, int cmd, short type, off_t start, off_t len) {
    struct flock fl = { .l_type = type, .l_whence = SEEK_SET, .l_start = start, .l_len = len };
    return fcntl(fd, cmd, &fl);
}

static int holder;

static void *release_later(void *arg) {
    (void)arg;
    struct timespec ts = { 0, 200 * 1000 * 1000 };
    nanosleep(&ts, NULL);
    ofd(holder, F_OFD_SETLK, F_UNLCK, 0, 0);
    return NULL;
}

static long now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(void) {
    printf("locks:\n");
    int a = open(F, O_RDWR | O_CREAT | O_TRUNC, 0644);
    int b = open(F, O_RDWR);
    write(a, "0123456789abcdefghij", 20);
    struct flock q = { .l_type = F_WRLCK, .l_whence = SEEK_SET };
    check("a program's own lock", ofd(a, F_SETLK, F_WRLCK, 0, 0) == 0);
    check("does not conflict with itself", fcntl(b, F_GETLK, &q) == 0 && q.l_type == F_UNLCK);
    check("and goes when any of its descriptors closes",
          ofd(a, F_SETLK, F_UNLCK, 0, 0) == 0);
    check("an open file's lock", ofd(a, F_OFD_SETLK, F_WRLCK, 0, 10) == 0);
    errno = 0;
    check("conflicts with another open file", ofd(b, F_OFD_SETLK, F_WRLCK, 5, 10) == -1 &&
          (errno == EAGAIN || errno == EACCES));
    check("but not beside it", ofd(b, F_OFD_SETLK, F_WRLCK, 10, 10) == 0);
    struct flock g = { .l_type = F_WRLCK, .l_whence = SEEK_SET, .l_start = 0, .l_len = 5 };
    check("F_OFD_GETLK names the holder's range", fcntl(b, F_OFD_GETLK, &g) == 0 &&
          g.l_type == F_WRLCK && g.l_start == 0 && g.l_len == 10 && g.l_pid == -1);
    ofd(a, F_OFD_SETLK, F_UNLCK, 0, 0);
    ofd(b, F_OFD_SETLK, F_UNLCK, 0, 0);
    check("two shared locks", ofd(a, F_OFD_SETLK, F_RDLCK, 0, 0) == 0 &&
          ofd(b, F_OFD_SETLK, F_RDLCK, 0, 0) == 0);
    check("keep out an exclusive one", ofd(b, F_OFD_SETLK, F_WRLCK, 0, 0) == -1);
    ofd(a, F_OFD_SETLK, F_UNLCK, 0, 0);
    ofd(b, F_OFD_SETLK, F_UNLCK, 0, 0);
    holder = a;
    ofd(a, F_OFD_SETLK, F_WRLCK, 0, 0);
    pthread_t t;
    pthread_create(&t, NULL, release_later, NULL);
    long before = now_ms();
    int waited = ofd(b, F_OFD_SETLKW, F_WRLCK, 0, 0) == 0;
    long took = now_ms() - before;
    pthread_join(t, NULL);
    check("F_OFD_SETLKW waits for the holder", waited && took >= 100);
    ofd(b, F_OFD_SETLK, F_UNLCK, 0, 0);
    check("flock takes the whole file", flock(a, LOCK_EX) == 0);
    errno = 0;
    check("and keeps out another open file", flock(b, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
    check("until it is unlocked", flock(a, LOCK_UN) == 0 && flock(b, LOCK_EX | LOCK_NB) == 0);
    flock(b, LOCK_UN);
    ofd(a, F_OFD_SETLK, F_WRLCK, 0, 0);
    close(a);
    check("closing a descriptor drops its lock", ofd(b, F_OFD_SETLK, F_WRLCK, 0, 0) == 0);
    close(b);
    check("tidy up", unlink(F) == 0);
    printf("locktest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

dtest `locks` (new section): `dchild lock /tmp/dtest-lock` takes an exclusive program lock with `vfs::lock(…, LOCK_WAIT)`, writes one byte to descriptor 3 (a socketpair end the parent gave it, as `spaces` does) and then blocks reading it; dtest waits for the byte, then checks `vfs::lock(…, 2, 0, 0, 0)` is `ERR_WOULD_BLOCK`, `LOCK_QUERY` names the child's space, closes its end (the child's read ends and it exits), and checks the lock is granted once the child has gone. A deadlock check: dtest holds byte 0, the child holds byte 1 and waits for byte 0; dtest's wait for byte 1 answers `ERR_DEADLOCK`.

- [ ] **Step 2: Run them.** Host: all ok. Quark: the first `F_SETLK` fails (`EINVAL`).

- [ ] **Step 3: The server.** `locks.rs`:

```rust
#[derive(Clone, Copy, PartialEq)]
pub enum Owner { Program(u64), Handle(usize) }

#[derive(Clone, Copy)]
pub struct Range { pub inode: u32, pub owner: Owner, pub start: u64, pub end: u64, pub exclusive: bool }

pub struct Waiter { pub sender: usize, pub space: u64, pub want: Range }

const MAX_LOCKS: usize = 256;
const MAX_WAITERS: usize = 64;

pub fn conflict(want: &Range) -> Option<Range>
pub fn apply(want: &Range, unlock: bool)               // merge, split, replace the owner's ranges
pub fn release(owner: Owner, inode: Option<u32>)       // all of an owner's, or on one inode
pub fn wait(w: Waiter) -> Result<(), u64>              // DEADLOCK, or queued
pub fn grantable() -> Option<Waiter>                   // a waiter that can now be granted
pub fn drop_space(space: u64)                          // locks and waiters of a dead program
```

`end` is exclusive and `u64::MAX` for "to the end"; ranges are compared as half-open intervals. `handle_lock` answers at once unless `LOCK_WAIT` finds a conflict, in which case it calls `locks::wait` and does not reply. After every `release` or `apply(unlock)`, the main loop drains `locks::grantable()`, applies each and replies `OK` to its sender. `handle_close` releases `Owner::Handle(h)` and `Owner::Program(space)` on that inode; `client_died` calls `drop_space`.

- [ ] **Step 4: Clients.** The layer's `fcntl`: `F_GETLK` (5), `F_SETLK` (6), `F_SETLKW` (7) with `LOCK_OFD` clear; `F_OFD_GETLK` (36), `F_OFD_SETLK` (37), `F_OFD_SETLKW` (38) with it set. `struct flock` is `short l_type` (`F_RDLCK` 0, `F_WRLCK` 1, `F_UNLCK` 2) at 0, `short l_whence` at 2, `l_start` at 8, `l_len` at 16, `l_pid` at 24; `l_whence` is resolved against the descriptor's offset (`SEEK_CUR`) or size (`SEEK_END`); a negative `l_len` covers the bytes before `l_start`. A query fills `l_type` (`F_UNLCK` if nothing conflicts), `l_start`, `l_len` (0 for "to the end") and `l_pid` (-1 for an OFD lock, else the holder's space id). `WOULD_BLOCK` is `EAGAIN`, `DEADLOCK` is `EDEADLK` (35). `flock` (73): `LOCK_SH` 1, `LOCK_EX` 2, `LOCK_UN` 8, `LOCK_NB` 4 → an OFD lock on the whole file, waiting unless `LOCK_NB`.

- [ ] **Step 5: Verify.** Build, layer, suites, image; boot `runtests /etc/libc.tests` (locktest ok), `dtest locks`, `runtests /etc/fontconfig.tests` (fontconfig's directory lock now works; no `F_SETLKW` failure path); `check-rootfs.sh`.

- [ ] **Step 6: Commit.** quark: "Record locks"; explosion: "locktest".

---

### Task 8: Orphans that survive a crash

**Files:**
- Modify: `quark/user/vfs/src/ext2.rs` (`last_orphan` field, superblock offset 232), `quark/user/vfs/src/ext2_ops.rs` (`orphan_add`, `orphan_remove`, `recover_orphans`), `quark/user/vfs/src/main.rs` (recover at mount, before anything is served)
- Modify: `quark/user/dchild/src/main.rs` (mode `orphan PATH`)
- Create: `explosion/tools/crash-test.sh` (two boots and a check between)
- Modify: `quark/docs/vfs.md` (the known-gap sentence goes)

**Interfaces:**
- Produces: `ext2_ops::orphan_add(e2: &mut Ext2State, ino: u32, inode: &mut Ext2Inode) -> Result<(), u64>`, `orphan_remove(e2: &mut Ext2State, ino: u32) -> Result<(), u64>`, `recover_orphans(e2: &mut Ext2State) -> Result<usize, u64>`; serial `[vfs] freed N orphaned inodes` when N > 0.

ext4 and ext2 keep the list the same way: the superblock's `s_last_orphan` names the first inode, and each listed inode's `i_dtime` names the next (0 ends it). `unlink`, `rmdir` and `rename` that leave an inode with no links and an open handle (or a program's current directory) add it, in the same transaction; the release that frees it removes it first. At mount, before the first request, the server frees every listed inode — truncating it to nothing and clearing its bitmap bit — and clears the list. `release_inode`'s deletion-time guard stays for inodes that are freed outright.

- [ ] **Step 1: The failing test.** `dchild orphan PATH` creates `PATH`, writes 5000 bytes, opens it, unlinks it, prints `holding PATH` and waits forever on `sys_recv`. `tools/crash-test.sh`:

```sh
#!/bin/sh
# Stop a machine while a program holds a removed file open, check the
# filesystem as the crash left it, boot it again, and check it after the
# server has recovered.
#
#     tools/crash-test.sh [hd|hd-ext4]
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
RUN=${RUNDIR:-${TMPDIR:-/tmp}/quark-boot-test}
SCRIPT=$(mktemp)
trap 'rm -f "$SCRIPT"' EXIT
cat > "$SCRIPT" <<'EOF'
sleep 16
type \n
sleep 1
type root\n
sleep 3
type dchild orphan /tmp/crash-orphan\n
sleep 4
quit
EOF
sh "$HERE/boot-test.sh" "$SCRIPT" "$RUN/crash-1.ppm" >/dev/null
echo "== after the crash (the list is expected here):"
sh "$HERE/check-rootfs.sh" || true
cat > "$SCRIPT" <<'EOF'
sleep 20
quit
EOF
sh "$HERE/boot-test.sh" "$SCRIPT" "$RUN/crash-2.ppm" >/dev/null
grep -a "orphaned inode" "$RUN/serial.log"
echo "== after recovery:"
sh "$HERE/check-rootfs.sh"
```

- [ ] **Step 2: Run it.** Expected today: the first check reports the removed inode (it is allocated, has no links and is on no list), the second boot prints no `orphaned inode` line, and the last check still reports it.

- [ ] **Step 3: Implement.** `orphan_add` sets `inode.i_dtime = e2.last_orphan` and `e2.last_orphan = ino`, writing the inode and the superblock (the journal's transaction covers both). `orphan_remove` walks the chain from `last_orphan`, unlinking `ino` (either the superblock's field or the previous inode's `i_dtime`). `recover_orphans` pops the list, frees each inode through `release_inode` (which truncates it and clears its bit), and writes the superblock; each inode is its own transaction on ext4. The call sites are `settle` (the last reference went) and the three namespace operations (the last link went while a reference remains).

- [ ] **Step 4: Verify.** `sh tools/crash-test.sh hd` and `sh tools/crash-test.sh hd-ext4`: the first check shows the list (`-n` counts it as bitmap differences, as scoping found), the second boot prints `freed 1 orphaned inode`, the last check is clean. Then an ordinary boot's `dtest files` and `runtests /etc/libc.tests`.

- [ ] **Step 5: Commit.** quark: "Orphans that survive a crash"; explosion: "crash-test".

---
### Task 9: Memory on demand

**Files:**
- Modify: `quark/src/paging.rs` (markers in non-present entries; `table_is_empty` means all-zero; `reserve`, `marker`, `clear_range`; `free_pt_leaves` skips markers), `quark/src/idt.rs` (a user fault on a marker is served, not fatal), `quark/src/syscall.rs` (`SYS_MAP_ANON` 192, `SYS_MEM_INFO` 193; `validate_user_range` backs pages before checking; `SYS_MUNMAP` clears markers), `quark/src/ipc.rs` (a lent buffer is backed when it is lent), `quark/src/scheduler.rs` (charging at fault time), `quark/src/pmm.rs` (`free_count`)
- Modify: `quark/user/quark-rt/src/syscall.rs`, `quark/user/linux-abi/src/syscall.c` (`mmap` of anonymous memory uses `SYS_MAP_ANON`, no chunking; `MAP_POPULATE` backs it at once)
- Modify: `quark/user/dchild/src/main.rs` (mode `hog`), `quark/user/dtest/src/main.rs` (section `memory`)
- Create: `explosion/toolchain/tests/lazytest.c`; Modify: `explosion/toolchain/tests/libc.tests`
- Modify: `quark/docs/abi.md` (block 0xC0 opens), `quark/CLAUDE.md` (the invariant about markers)

**Interfaces:**
- Produces: `SYS_MAP_ANON = 192` — arg0 = address, arg1 = pages (up to 2^27, 512 GiB), arg2 = flags (bit 0: back every page now) → 0 or `u64::MAX`; the range must be empty, as `SYS_MMAP` requires. `SYS_MEM_INFO = 193` — no arguments → `(free frames << 32) | pages charged to the caller`. `quark_rt::syscall::sys_map_anon(addr: usize, pages: usize, populate: bool) -> Result<(), ()>`, `sys_mem_info() -> (usize, usize)`.
- Produces (paging): `pub const MARKER: u64 = 1 << 10;` `pub const MARKER_OBJECT: u64 = 1 << 11;` `pub const MARKER_SHARED: u64 = 1 << 6;` `pub const OBJECT_SHIFT: u64 = 52;` `pub fn marker_entry(writable: bool, exec: bool) -> u64`, `pub unsafe fn reserve(pml4: usize, virt: usize, entry: u64) -> Result<(), PagingError>`, `pub unsafe fn marker(pml4: usize, virt: usize) -> Option<u64>`, `pub unsafe fn back(pml4: usize, virt: usize, write: bool) -> Result<(), Fault>` (Task 10 extends it), `pub unsafe fn clear_range(pml4: usize, virt: usize, pages: usize) -> usize` (frames freed).

A reserved page is a non-present entry with `MARKER` set; its `WRITABLE` bit and `NO_EXECUTE` bit say what the page will be. The first touch — a user fault, or the kernel backing a buffer before it copies — takes a zeroed frame, charges the task and maps it `PRESENT | USER | OWNED` with those bits. An entry with `MARKER` is not empty, so the page-table walks that reclaim and destroy tables must test for an all-zero entry rather than a clear `PRESENT` bit. When no frame is left, or the task's limit is reached, the task is ended with SIGBUS (`[OOM tid=N]` on serial): the memory was promised and cannot be given, which is Linux's overcommit bargain. `SYS_MMAP` stays as it is, backed at once, for the servers that rely on it.

- [ ] **Step 1: The failing tests.** `lazytest.c` (runs on Linux):

```c
/* Memory is backed when it is touched, not when it is mapped. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define GIB (1024UL * 1024 * 1024)

int main(void) {
    int failed = 0;
    size_t len = 4 * GIB;
    unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    printf("  %s  four gigabytes, mapped\n", p != MAP_FAILED ? "ok  " : "FAIL");
    failed += p == MAP_FAILED;
    if (p != MAP_FAILED) {
        int ok = 1;
        for (size_t at = 0; at < len; at += 64 * 1024 * 1024) {
            ok &= p[at] == 0;
            p[at] = (unsigned char)(at >> 26);
        }
        for (size_t at = 0; at < len; at += 64 * 1024 * 1024) {
            ok &= p[at] == (unsigned char)(at >> 26);
        }
        printf("  %s  and sixty-four pages of it used\n", ok ? "ok  " : "FAIL");
        failed += !ok;
        FILE *f = fopen("/dev/null", "w");
        int wrote = f && fwrite(p + GIB, 1, 8192, f) == 8192;
        if (f) fclose(f);
        printf("  %s  a page never touched can be written from\n", wrote ? "ok  " : "FAIL");
        failed += !wrote;
        munmap(p, len);
    }
    char *big = malloc(GIB);
    printf("  %s  malloc of a gigabyte\n", big ? "ok  " : "FAIL");
    failed += big == NULL;
    free(big);
    printf("lazytest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

dtest `memory` (new section): `sys_mem_info()` before; `sys_map_anon(0xA0_0000_0000, 262144, false)` (a GiB) succeeds and leaves the charge unchanged; writing one byte into each of 16 pages raises the charge by 16 and lowers the free count by at least 16; `sys_munmap` of the GiB in 256-page steps brings the charge back; a lent buffer in an untouched reserved page reaches the VFS (`vfs::write` from it); `dchild hog` (touches reserved pages until it is stopped) ends with exit code -7 while dtest carries on; after it, `sys_mem_info()`'s free count is within 64 pages of where it started.

- [ ] **Step 2: Run them.** Expected: lazytest's first check fails (four GiB is refused today); dtest does not build (`sys_map_anon`).

- [ ] **Step 3: Paging.** Implement the constants and functions above. `reserve` walks and allocates tables as `map_page` does and writes `entry` into a zero leaf (`AlreadyMapped` otherwise). `back(pml4, virt, write)`: read the leaf; `MARKER` without `MARKER_OBJECT` → allocate, zero, charge (`scheduler::charge_or_fail`), map; a write fault on a marker whose `WRITABLE` is clear, or no marker at all, is `Fault::Invalid`. `clear_range` clears markers and present entries alike, freeing `OWNED` frames, and reclaims empty tables. `free_pt_leaves` leaves marker entries alone (they own nothing). `translate` and `walk_flags` still report a marker as not mapped.

- [ ] **Step 4: Faults and system calls.** In `exception_handler`, a user page fault first calls `paging::back(cr3, cr2, write)` with interrupts enabled; `Ok` returns to retry the instruction; `Fault::NoMemory` ends the task with `-SIGBUS` after the `[OOM …]` line; `Fault::Invalid` goes on to today's path (pager or `SIGSEGV`). `validate_user_range` walks the range and calls `back` on every marker before `user_range_accessible`. `call_inner` backs a lent buffer before blocking. `SYS_MAP_ANON` checks `user_range_ok` and emptiness, reserves each page, and backs them all when bit 0 is set (charging, and undoing the reservation if that fails). `SYS_MUNMAP` uses `clear_range`. `SYS_MEM_INFO` reads `pmm::free_count()` and the caller's charge.

- [ ] **Step 5: The Linux layer.** Anonymous `mmap` reserves the whole length with one `SYS_MAP_ANON` (flag set for `MAP_POPULATE`); the chunked `map_pages` path goes. `munmap` still steps in 256-page calls.

- [ ] **Step 6: Verify.** Build, layer, suites (pixman's `stress-test` now gets its 2.7 GiB mask and must still pass), image; boot `runtests /etc/libc.tests` (lazytest ok), `dtest memory`, `dtest`, `runtests /etc/pixman.tests`, `runtests /etc/cairo.tests`, `wm weston-simple-shm wlcairo` (Esc), `hello`. Serial: one `[OOM tid=…]` line, from the hog.

- [ ] **Step 7: Commit.** quark: "Memory on demand"; explosion: "lazytest".

---

### Task 10: Files that can be mapped (private mappings)

**Files:**
- Create: `quark/src/memobj.rs` (memory objects, their page caches, and paging in); Modify: `quark/src/cap.rs` (`CapType::MemObject = 9`, minting and narrowing), `quark/src/lend.rs` (a kernel frame can be lent), `quark/src/ipc.rs` (`pager_call`, the pager bit in `sender`, `sys_reply` accepts it), `quark/src/paging.rs` (`back` handles object markers; `clear_range` and `free_pt_leaves` tell the object), `quark/src/syscall.rs` (`SYS_OBJECT_CREATE` 194, `SYS_OBJECT_MAP` 195, `SYS_OBJECT_CTL` 196)
- Modify: `quark/user/quark-rt/src/syscall.rs`, `quark/user/quark-rt/src/ipc.rs` (`PAGER_BIT`, `TAG_PAGE_IN`, `TAG_OBJECT_IDLE`)
- Create: `quark/user/vfs/src/pager.rs`; Modify: `quark/user/vfs/src/protocol.rs` (`TAG_MAP = 23`), `quark/user/vfs/src/main.rs` (dispatch; reads and writes of a mapped inode go through its object)
- Modify: `quark/user/quark-rt/src/vfs.rs` (`map`), `quark/user/libc/include/quark/vfs.h`, `quark/user/libc/src/quark.c` (`quark_vfs_map`), `quark/user/linux-abi/src/syscall.c` (`mmap` with a descriptor)
- Modify: `explosion/toolchain/build-freetype.sh` (`-Dmmap=enabled`, and its comment)
- Create: `explosion/toolchain/tests/maptest.c`; Modify: `explosion/toolchain/tests/libc.tests`
- Modify: `quark/docs/abi.md`, `quark/docs/vfs.md`

**Interfaces:**
- Produces (kernel): `CapType::MemObject` — `param0` = object id, `param1` = access (1 read, 2 write). `SYS_OBJECT_CREATE = 194` — the pager: arg0 = cookie (its own name for the object), arg1 = size in bytes, arg2 = slot → mints a read-write `MemObject` into that slot, returns the object id. `SYS_OBJECT_MAP = 195` — a holder: arg0 = slot, arg1 = address, arg2 = pages, arg3 = first page in the object, arg4 = flags (1 write, 2 shared, 4 exec) → 0 or `u64::MAX`; writing through a shared mapping needs write access, a private one does not. `SYS_OBJECT_CTL = 196` — the pager: arg0 = object id, arg1 = op, arg2/arg3 per op: `0` resize (arg2 = bytes), `1` read a cached page (arg2 = buffer, arg3 = page) → 1 if cached, 0 if not, `2` write a cached page (arg2 = buffer, arg3 = page) → 1 if cached, `3` take a dirty page (arg2 = buffer) → page index or `u64::MAX` when there is none, `4` release (only when no page is mapped).
- Produces (kernel → pager): `TAG_PAGE_IN = 0xFFFF_0005` — `sender` = faulting TID | `PAGER_BIT` (`1 << 62`), `data` = `[cookie, page, object id]`, one 4096-byte frame lent for writing; the pager fills it with `sys_lent_write` and replies (`OK`, or an error to make the fault fatal). `TAG_OBJECT_IDLE = 0xFFFF_0006` — sender 0, `data` = `[cookie, object id]`, when the last mapped page of an object goes.
- Produces (VFS): `TAG_MAP` — `[handle, flags]` (flags 1 = the caller will write through a shared mapping) → `[slot, size]`, a `MemObject` granted into one of the caller's free slots (read-only unless the handle is writable and flag 1 is set).
- Produces (clients): `quark_rt::vfs::map(vfs: usize, handle: usize, write_shared: bool) -> Result<(usize, u64), u64>`; `int quark_vfs_map(unsigned long handle, int write_shared, unsigned long *slot, unsigned long *size)`.

A page of a mapped file is looked for in the object's cache first; if it is not there, the faulting task — in kernel mode, on its own stack, with interrupts on — calls the pager with a fresh frame lent to it, and the frame goes into the cache when the pager answers. A read-only mapping maps the cached frame itself (not `OWNED`); a private writable one copies it into a frame of the task's own. The cache belongs to the object and lives until the object is released, after the last mapped page is gone and the pager has written back what it must. The object's table slot (1..2047) is kept in bits 52–62 of every entry that refers to it, present or not, so that unmapping and teardown can count its mapped pages without a reverse map. The CPU ignores those bits in a present entry only while protection keys are off (CR4.PKE clear), so the kernel must never turn them on; `cpu.rs` says so beside the other CR4 bits. A page past the end of the file is `SIGBUS`, as on Linux; the last partial page reads as zeroes past the end. Only the pager may call `SYS_OBJECT_CTL`, and a `TAG_PAGE_IN` without `PAGER_BIT` is a forgery and refused.

- [ ] **Step 1: The failing test.** `maptest.c` (runs on Linux), private half:

```c
/* Files mapped into memory: private mappings here; shared ones in Task 11. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

static int failed;

static void check(const char *what, int ok) {
    printf("  %s  %s\n", ok ? "ok  " : "FAIL", what);
    if (!ok) {
        failed++;
    }
}

#define F "/tmp/maptest"
#define PAGES 3
#define SIZE (PAGES * 4096 - 100)

static unsigned char pattern(size_t i) {
    return (unsigned char)(i * 7 + (i >> 12));
}

int main(void) {
    printf("mapped files:\n");
    int fd = open(F, O_RDWR | O_CREAT | O_TRUNC, 0644);
    unsigned char buf[SIZE];
    for (size_t i = 0; i < SIZE; i++) buf[i] = pattern(i);
    check("a file to map", fd >= 0 && write(fd, buf, SIZE) == SIZE);
    unsigned char *r = mmap(NULL, SIZE, PROT_READ, MAP_PRIVATE, fd, 0);
    check("maps for reading", r != MAP_FAILED);
    int same = r != MAP_FAILED;
    for (size_t i = 0; same && i < SIZE; i++) same = r[i] == pattern(i);
    check("and holds what the file holds", same);
    int tail = r != MAP_FAILED;
    for (size_t i = SIZE; tail && i < PAGES * 4096; i++) tail = r[i] == 0;
    check("with zeroes past the end of its last page", tail);
    unsigned char *w = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 4096);
    check("a private writable mapping of page 1", w != MAP_FAILED && w[0] == pattern(4096));
    if (w != MAP_FAILED) w[0] = (unsigned char)~pattern(4096);
    unsigned char one;
    check("a write to it stays private", pread(fd, &one, 1, 4096) == 1 && one == pattern(4096));
    check("and the read-only mapping still shows the file", r != MAP_FAILED && r[4096] == pattern(4096));
    if (w != MAP_FAILED) munmap(w, 4096);
    if (r != MAP_FAILED) munmap(r, SIZE);
    int dir = open("/tmp", O_RDONLY | O_DIRECTORY);
    errno = 0;
    check("a directory cannot be mapped", mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, dir, 0) == MAP_FAILED &&
          errno == ENODEV);
    close(dir);
    int ro = open(F, O_RDONLY);
    errno = 0;
    check("nor written shared through a read-only descriptor",
          mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, ro, 0) == MAP_FAILED && errno == EACCES);
    close(ro);
    close(fd);
    check("tidy up", unlink(F) == 0);
    printf("maptest: %s\n", failed ? "FAILED" : "ok");
    return failed ? 1 : 0;
}
```

- [ ] **Step 2: Run it.** Host: all ok. Quark: `maps for reading` fails (`ENODEV` today).

- [ ] **Step 3: The kernel.** `memobj.rs`:

```rust
pub struct Object {
    pub in_use: bool,
    pub id: u64,                      // never reused
    pub pager: usize,                 // TID of the pager
    pub pager_number: u64,            // its endpoint number, so a new task in that slot is not it
    pub cookie: u64,
    pub bytes: u64,
    pub mapped: u64,                  // entries (present or not) that name this object
    pub cache: alloc::collections::BTreeMap<u64, usize>,   // page -> frame
    pub dirty: alloc::collections::BTreeSet<u64>,
}

pub const MAX_OBJECTS: usize = 2048;   // slot 0 is "none"

pub fn create(pager: usize, cookie: u64, bytes: u64) -> Option<(usize, u64)>   // (slot, id)
pub fn slot_of(id: u64) -> Option<usize>
pub fn page_in(slot: usize, page: u64) -> Result<usize, Fault>                // frame; may call the pager
pub fn map_ref(slot: usize, n: u64)
pub fn unmap_ref(slot: usize, n: u64)                                         // TAG_OBJECT_IDLE at zero
pub fn ctl(caller: usize, id: u64, op: u64, a: u64, b: u64) -> u64
```

`page_in` returns a cached frame, or takes a frame, lends it to the pager with `ipc::pager_call(pager, msg, Lent::Frame { phys, access: LEND_WRITE })`, and caches it on `OK`. Two tasks faulting on one uncached page may both call the pager; the second result is freed and the first kept. `paging::back` on an object marker: slot from bits 52–62, page from bits 12–51, `page_in`, then map read-only frames directly (flags without `OWNED`, slot bits kept) or copy into a new `OWNED` frame for a private writable page (charged). `clear_range` and `free_pt_leaves` call `unmap_ref` for every entry carrying slot bits. `lend.rs` gains `Lent::Frame`, copied through the identity map without a page walk. `pager_call` is `call_inner` with the lend set by the kernel and `sender` marked; `sys_reply` masks `PAGER_BIT` off `dest`. The capability rules: `SYS_OBJECT_CREATE` mints for the caller; `sys_cap_mint` may derive a `MemObject` with the same id and fewer access bits from one the minter holds; `SYS_OBJECT_MAP` looks the slot up in the caller's CSpace. `SYS_OBJECT_CTL` requires the caller to be the object's pager (TID and endpoint number).

- [ ] **Step 4: The VFS as a pager.** `pager.rs`:

```rust
pub struct Mapped { pub inode: u32, pub id: u64, pub slot: usize }
const MAX_MAPPED: usize = 64;

pub fn object_for(inode: u32, bytes: u64) -> Result<&'static Mapped, u64>   // creates on first use
pub fn page_in(sender: usize, msg: &Message)        // reads the page, lends it back, replies
pub fn idle(id: u64)                                 // takes dirty pages, writes them, releases
pub fn read_through(inode: u32, page: u64, out: &mut [u8; 4096]) -> bool   // cached copy, if any
pub fn wrote(inode: u32, offset: u64, data: &[u8])  // keeps cached pages equal to the file
pub fn resized(inode: u32, bytes: u64)
```

`TAG_MAP` checks the handle (a regular file, `NOT_SUPPORTED` on FAT32 and for devices), gets the object, mints a derived capability with the caller's access into a scratch slot, grants it with `sys_cap_grant_any(sender, scratch)` (the caller is in a call, which is consent), deletes the scratch slot and replies `[slot, size]`. The dispatch arm for `TAG_PAGE_IN` accepts only `sender & PAGER_BIT != 0`; `TAG_OBJECT_IDLE` only sender 0. `handle_read_ext2` reads a mapped inode's pages through `read_through` first (so a shared mapping's writes are seen), `handle_write_ext2` calls `wrote` after writing, and `truncate` calls `resized`.

- [ ] **Step 5: Clients.** `mmap` with a descriptor: `PROT_WRITE` with `MAP_SHARED` asks `TAG_MAP` for write access; the address comes from the same arena as anonymous memory; `SYS_OBJECT_MAP` with the page offset (`offset` must be page-aligned, else `EINVAL`); the slot is deleted afterwards (the mapping keeps the object). `ENODEV` for a directory or a device, `EACCES` for write-shared through a read-only descriptor, `EBADF` for a bad descriptor.

- [ ] **Step 6: FreeType maps its fonts.** `build-freetype.sh` switches to `-Dmmap=enabled`, and its comment says why the decision changed: a mapping is backed page by page now, so the Unix stream costs what it touches. Rebuild FreeType, fontconfig, cairo, the tests and the clients.

- [ ] **Step 7: Verify.** Build, layer, all suites, image; boot `runtests /etc/libc.tests` (maptest ok), `runtests /etc/fonts.tests` (the same checksum, now through a mapping), `runtests /etc/cairo.tests`, `runtests /etc/fontconfig.tests` (its caches over a kilobyte are mapped), `wm wlcairo` (the text as before); `check-rootfs.sh`; the same on `make hd-ext4`.

- [ ] **Step 8: Commit.** quark: "Files can be mapped"; explosion: "maptest; FreeType maps its fonts".

---

### Task 11: Shared file mappings

**Files:**
- Modify: `quark/src/memobj.rs` (shared frames, dirty pages, `SYS_OBJECT_SYNC`), `quark/src/paging.rs` (shared writable entries), `quark/src/syscall.rs` (`SYS_OBJECT_SYNC` 197)
- Modify: `quark/user/vfs/src/pager.rs` (`sync`), `quark/user/vfs/src/main.rs`
- Modify: `quark/user/linux-abi/src/syscall.c` (`msync` 26; `munmap` of a shared mapping), `quark/user/quark-rt/src/syscall.rs`
- Modify: `quark/user/dchild/src/main.rs` (mode `mapwrite PATH`), `quark/user/dtest/src/main.rs` (section `memory`)
- Modify: `explosion/toolchain/tests/maptest.c`
- Modify: `quark/docs/abi.md`, `quark/docs/vfs.md`

**Interfaces:**
- Produces: `SYS_OBJECT_SYNC = 197` — a holder: arg0 = start address, arg1 = pages → the kernel finds the objects mapped shared in that range and asks each pager to write back (`TAG_OBJECT_SYNC = 0xFFFF_0007`, `sender` = caller | `PAGER_BIT`, `data` = `[cookie, object id]`), returning when they have answered. `quark_rt::syscall::sys_object_sync(addr: usize, pages: usize) -> Result<(), ()>`.

A shared page is the cached frame itself, mapped into every program that maps it, so all of them see one another's writes at once, and `read()` sees them because a mapped inode is read through its cache. A frame mapped writable is marked dirty for as long as it is mapped that way; the pager writes back every dirty page when the object goes idle, when `msync` asks, and before it releases the object. `write()` updates the cached copy in place, so mappings see it too. Shortening the file leaves pages already mapped as they are; a new fault past the new end is `SIGBUS`.

- [ ] **Step 1: The failing test.** `maptest.c` gains, before `tidy up`:

```c
    fd = open(F, O_RDWR | O_CREAT | O_TRUNC, 0644);
    ftruncate(fd, 8192);
    unsigned char *s1 = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    unsigned char *s2 = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    check("two shared mappings", s1 != MAP_FAILED && s2 != MAP_FAILED);
    if (s1 != MAP_FAILED && s2 != MAP_FAILED) {
        memcpy(s1 + 10, "shared", 6);
        check("see each other's writes", !memcmp(s2 + 10, "shared", 6));
        char got[6];
        check("and read() sees them", pread(fd, got, 6, 10) == 6 && !memcmp(got, "shared", 6));
        check("msync", msync(s1, 8192, MS_SYNC) == 0);
        pwrite(fd, "PWRITE", 6, 4100);
        check("a write() shows in the mapping", !memcmp(s1 + 4100, "PWRITE", 6));
        munmap(s1, 8192);
        munmap(s2, 8192);
    }
    close(fd);
    fd = open(F, O_RDONLY);
    char back[6];
    check("and what was written through the mapping is in the file",
          pread(fd, back, 6, 10) == 6 && !memcmp(back, "shared", 6));
    close(fd);
```

dtest `memory`: `dchild mapwrite /tmp/dtest-map` maps the (dtest-created, 4096-byte) file shared, writes `from the child` at offset 0 and exits without `msync`; afterwards dtest's `vfs::read` finds it (the object went idle when the child's address space was destroyed and the VFS wrote it back).

- [ ] **Step 2: Run it.** Expected: `two shared mappings` passes (Task 10 maps them) but `see each other's writes` fails — each mapping has its own copy.

- [ ] **Step 3: Implement.** In `back`, an object marker with `MARKER_SHARED` maps the cached frame itself, writable if the marker is, and a writable one adds the page to `dirty`. `SYS_OBJECT_SYNC` walks the caller's range for entries with slot bits and `MARKER_SHARED`, and makes one `pager_call` per object with `TAG_OBJECT_SYNC`. The pager's `sync` and `idle` loop `SYS_OBJECT_CTL` op 3, writing each dirty page with the ordinary write path (clipped to the file's size), and op 4 releases after `idle`. `msync` in the layer calls `SYS_OBJECT_SYNC` for `MS_SYNC` and `MS_ASYNC` alike (asynchronous writing is not offered); `MS_INVALIDATE` is accepted and does nothing more.

- [ ] **Step 4: Verify.** Build, layer, suites, image; boot `runtests /etc/libc.tests`, `dtest memory`, `dtest`; `check-rootfs.sh`; the same on `make hd-ext4`.

- [ ] **Step 5: Commit.** quark: "Shared file mappings"; explosion: "maptest: shared mappings".

---
### Task 12: Font caches built with the image

**Files:**
- Modify: `explosion/toolchain/build-fontconfig.sh` (also builds fontconfig for the host and installs its `fc-cache` as `$QUARK_HOSTDEPS/bin/quark-fc-cache`)
- Create: `explosion/tools/stage-font-caches.sh`; Modify: `explosion/Makefile` (`stage` runs it after the overlays)
- Modify: `explosion/tools/populate-ext.sh` (every staged directory and file keeps its modification time, in whole seconds)
- Modify: `explosion/toolchain/tests/fctest.c`, `explosion/toolchain/tests/fontconfig.tests` (`fctest` runs before `fc-cache`)
- Modify: `explosion/toolchain/README.md`

**Interfaces:**
- Produces: `tools/stage-font-caches.sh <stage>` — does nothing unless `<stage>/etc/fonts/fonts.conf` exists; warns and does nothing if `quark-fc-cache` is missing; otherwise sets every directory under `usr/share/fonts` to the current second, clears `var/cache/fontconfig`, runs `FONTCONFIG_FILE=<stage>/etc/fonts/fonts.conf quark-fc-cache -s -f -y <stage>`, and appends what it wrote to `<stage>/.overlays` so that a stage without fonts takes it back out.

The host fontconfig is the same source built natively with the same configuration (`--sysconfdir=/etc --localstatedir=/var -Dadditional-fonts-dirs=no`, FreeType from `build-freetype.sh`'s `build-host`, expat from `$QUARK_HOSTDEPS`). Its caches are Quark's, byte for byte, except for the directory times they record, which is why the image must keep those times.

- [ ] **Step 1: The failing test.** `fctest.c` gains, on Quark only:

```c
#ifdef __quark__
    /* The cache came with the image: it is older than this boot. */
    struct timespec real, mono;
    clock_gettime(CLOCK_REALTIME, &real);
    clock_gettime(CLOCK_MONOTONIC, &mono);
    time_t booted = real.tv_sec - mono.tv_sec;
    struct stat cst;
    check("the cache was built with the image",
          cache_file && stat((const char *)cache_file, &cst) == 0 && cst.st_mtime < booted);
#endif
```

(`#include <sys/stat.h>` and `<time.h>`; `cache_file` is the path `FcDirCacheLoad` returned, read before it is freed.) `fontconfig.tests` becomes `fctest`, `fc-cache -v`, `fc-list`, `fc-match monospace`.

- [ ] **Step 2: Run it.** On today's image: `the cache was built with the image` fails — the image has no cache, so `FcInit` writes one during the test.

- [ ] **Step 3: Implement.** The host build in `build-fontconfig.sh` (`build-host` under the source, `ninja fc-cache/fc-cache`, copied to `$QUARK_HOSTDEPS/bin/quark-fc-cache`); the stage script; `populate-ext.sh` ends with a third debugfs run of `set_inode_field <path> mtime <seconds>` (and `ctime`, `atime`) for every staged file and directory, directories last, with the seconds from `stat -c %Y`. The Makefile's `stage` calls the script after `stage-overlays.sh`.

- [ ] **Step 4: Verify.** Rebuild fontconfig (tools and host), suites, image; `debugfs -R 'ls -l /var/cache/fontconfig'` shows the caches and their links; boot: `runtests /etc/fontconfig.tests` (fctest ok before `fc-cache` has run; `fc-cache -v` says both directories were skipped as valid), `wm wlcairo` draws text on its first frame; `check-rootfs.sh`; the same on `make hd-ext4`; `make hd` without `ROOT_OVERLAYS` leaves no cache in the stage.

- [ ] **Step 5: Commit.** explosion: "Font caches come with the image".

---

## Part B — programs that fall over

### Task 13: `wm` takes what it is given, and the shell takes quotes

**Files:**
- Modify: `quark/user/wm/src/main.rs` (arguments are command lines; checks before the display is claimed; manifest grants; `WM_SESSION`; the empty-backdrop hint), `quark/user/wmdemo/src/main.rs`, `quark/user/wmtype/src/main.rs` (read `WM_SESSION`)
- Modify: `quark/user/qsh/src/main.rs` (quoting; too many arguments is an error)
- Modify: `quark/user/runtests/src/main.rs` (list lines: `?` accepts any exit status but not a signal; `@N` sets a deadline of N seconds, default 300, after which the program is killed and reported)

**Interfaces:**
- Produces: `wm "<program> [args…]" …` — each argument is a program and its arguments, split on spaces. `wm` with no argument prints `usage: wm "<program> [args]" …` and exits 2 without touching the display; more than four programs is `wm: at most 4 programs`, exit 2; a program that cannot be loaded is named (`wm: cannot run 'foo'`), exit 1, and the display is never claimed. A session program's environment has `WM_SESSION=<n>` (1-based) beside `WAYLAND_SOCKET=3`; its `argv[0]` is its name and the rest are its own arguments. `wm`'s manifest asks for what the shell's does (`task_mgmt(0)`, `phys_alloc(64)`) so that it can grant what its programs ask for.
- Produces: `qsh` splits words on spaces except inside `"…"` (where `\"` and `\\` are escapes) and `'…'` (literal); an unterminated quote prints `qsh: unterminated quote` and runs nothing; a seventeenth word prints `qsh: too many arguments` and runs nothing.

- [ ] **Step 1: The failing checks.** A key script (`$SP/p17t13.keys`) with, one screenshot each: `wm`, `wm ctortest ctortest ctortest ctortest ctortest`, `wm nonexistent`, `wm "ls /etc"`, `wm hello`, `wm "dtest wire"`, `wm wmdemo`, `wm weston-simple-shm weston-simple-shm weston-simple-shm weston-simple-shm`, `wm qsh`, `echo "a  b" c`, `echo 'x y'`, `echo "unterminated`. Expected today: `wm` claims the display before complaining; five programs start four; `wm "ls /etc"` is one program named `"ls`; `hello` aborts; `wmdemo`'s title is right only because of `argv[1]`; the quotes are passed through.

- [ ] **Step 2: The shell.** A `split_args(line: &[u8], out: &mut [&[u8]; 16], store: &mut [u8; 256]) -> Result<usize, &'static str>` copies each word, unquoted, into `store` and records slices of it; both the single-command path and each pipeline stage use it.

- [ ] **Step 3: wm.** Before `claim`: check `argv(1)`, the count, and load every program (`spawn::load_path`), keeping the `Spawned`s; then claim, then grant (`sys_cap_grant` of the nameserver endpoint and `quark_rt::manifest::grant_image(tid, image, 12)` — the image is still in `FILE_BUF` when `load_path`'s `grant` closure runs, which is where the grant goes), connect, set `argv` and `env`, give the directory (Task 6) and start. `wmdemo` and `wmtype` read `quark_rt::args::getenv(b"WM_SESSION")` for their title number. After `EMPTY_HINT_TICKS` (300) with no window, the backdrop shows `No window yet. Esc ends the session.` in the middle of the screen until a window appears.

- [ ] **Step 4: runtests.** Lines: an optional `?`, then an optional `@N`, then the program and its arguments. The wait becomes: `sys_task_watch(child)`, `sys_recv_timeout(TID_ANY, …)` until `TAG_TASK_DIED` for the child (sender 0) or the deadline, then `sys_wait`; at the deadline, `sys_task_kill` and `  FAIL  name (timed out)`. With `?`, an exit status is a pass and only a signal (a negative status) or a timeout is a failure.

- [ ] **Step 5: Verify.** Build; image; boot the Step 1 script: usage without a display change; `at most 4`; `cannot run 'nonexistent'` without a display change; `ls /etc`'s listing appears once the session ends; `hello` runs its threads to the end; `dtest wire` passes; `wmdemo #1`; four windows; `qsh` exits (no input); `a  b c`; `x y`; `qsh: unterminated quote`. `runtests /etc/libc.tests` still passes.

- [ ] **Step 6: Commit.** quark: "wm takes what it is given; the shell takes quotes".

---

### Task 14: Displays and keyboards nest; notices come only from the kernel

**Files:**
- Modify: `quark/user/fb/src/main.rs` (a stack of up to 8 claimants), `quark/user/input/src/main.rs` (the same for raw input)
- Modify: `quark/user/quark-rt/src/ipc.rs` (`death_notice`, `space_death_notice`)
- Modify: `quark/user/nameserver/src/main.rs`, `quark/user/vfs/src/main.rs`, `quark/user/wm/src/main.rs`, `quark/user/fb/src/main.rs`, `quark/user/input/src/main.rs` (use them)
- Modify: `quark/user/dtest/src/main.rs` (section `service`)
- Modify: `quark/CLAUDE.md` ("The screen": the display is a stack)

**Interfaces:**
- Produces: `quark_rt::ipc::death_notice(msg: &Message) -> Option<usize>` — `Some(tid)` only for `sender == 0 && tag == TAG_TASK_DIED`; `space_death_notice(msg) -> Option<u64>` likewise for `TAG_SPACE_DIED`. A forged notice is an unknown request and is answered with an error.
- Produces (fb and input): a claim pushes the claimant (the one below is told it lost the display, as today); a release by the top pops it and hands the display (keyboard) to the new top, or to nobody when the stack is empty; a release or death of a claimant below the top removes it without disturbing the top; a ninth claim is refused.

- [ ] **Step 1: The failing checks.** dtest `service`: a call to the nameserver with `TAG_TASK_DIED` and `data[0]` = the VFS's TID is answered with an error (today it is never answered: the call times out after 50 ticks), and `nameserver::lookup(b"vfs")` still succeeds afterwards; the same forged notice to `fb` is answered with an error. A key script: `wm "wm weston-simple-shm"`, shot, Esc, shot, Esc, shot, `echo back`, shot.

- [ ] **Step 2: Run them.** Expected: the forged calls time out and `lookup(b"vfs")` fails from then on (reboot before continuing); the nested session ends on a blank screen that never returns to the console.

- [ ] **Step 3: Implement.** The helpers; every server's `TAG_TASK_DIED`/`TAG_SPACE_DIED` arm becomes a guard on the helper, so a forged one falls through to the error arm. `fb`: `static mut STACK: [(usize, usize); 8]` of `(tid, slot)` and a length replacing `OWNER`/`PREVIOUS`; `hand_back` gives the display to the new top; `TAG_TASK_DIED` removes the dead TID wherever it is. `input`: `raw_stack: [usize; 8]`; the top is the owner that `TAG_INPUT_POLL` and the key pump serve; releasing or dying pops or removes. `wm` repaints everything when it is handed the display back (`TAG_FB_GAINED`), including the hint.

- [ ] **Step 4: Verify.** Build; image; boot: dtest `service` passes; the nested script shows the inner window, then the outer backdrop with its hint (the outer compositor has the display and the keyboard back), then the console; `wm weston-simple-shm wlcairo` and Esc still behave.

- [ ] **Step 5: Commit.** quark: "Displays and keyboards nest; only the kernel reports deaths".

---

### Task 15: Services survive a fuzzer

**Files:**
- Create: `quark/user/qfuzz/` (a `no_std` program; add it to the Makefile's program lists)
- Modify: `quark/user/disk/src/main.rs` (answers only its claimant: `TAG_DISK_CLAIM = 5` with an endpoint offered, watched), `quark/user/vfs/src/main.rs` (claims the disk at start)
- Modify: `quark/user/keyboard/src/main.rs` (answers only its claimant: `TAG_KBD_CLAIM`), `quark/user/input/src/main.rs` (claims it; serves line readers without blocking)
- Modify: every server the fuzzer breaks
- Create: `explosion/toolchain/tests/fuzz.tests`

**Interfaces:**
- Produces: `qfuzz <rounds> [seed]` — for each of `nameserver`, `vfs`, `fb`, `console`, `input`, `net`, `disk`, `keyboard`: `rounds` requests built from the seed (tags drawn from the service's own range, the kernel's `0xFFFF_00xx` range and anything at all; data words drawn from small numbers, handles, huge numbers and random bits; with and without a lent buffer of random length and access; with and without an offered capability), each made with a 50-tick deadline. Path requests to the VFS use garbage names under `/tmp/qfuzz` (the fuzzer changes into it first, and never generates a leading `/` or a `..` component). After each service it checks the service is alive (`sys_task_info`), answers `TAG_PING` within 100 ticks, and still does its job: the nameserver finds `vfs`, the VFS reads `/etc/passwd`, `fb` answers `TAG_FB_INFO` with the mode, the console prints a line, `input` answers a claim and a release, `net` answers a ping, `disk` and `keyboard` refuse the fuzzer. It releases any display or keyboard claim it was granted. It prints the seed, one line per service, and exits non-zero if any check failed.
- Produces: `fuzz.tests`: `@600 qfuzz 2000 1`, `@600 qfuzz 2000 2`, `@600 qfuzz 2000 3`.

A line reader in `input` is deferred like any other waiter: the server keeps serving requests, takes keys with `get_key_nb` whenever its receive times out (every tick), and answers the reader when its line is finished. A program asking for a line used to stop the whole server — the display's compositor included — until somebody pressed Enter.

- [ ] **Step 1: The fuzzer and a first run.** Write `qfuzz` (a `Rng` — xorshift64* — seeded from the argument or `sys_getrandom`), build, image with `fuzz.tests` in a suite, boot and run `runtests /etc/fuzz.tests`. Expected on today's services: `disk` and `keyboard` answer the fuzzer; `input` stops answering after the first `TAG_READ`; whatever else the fuzzer finds.

- [ ] **Step 2: Drivers answer only their server.** The disk driver's first request must be `TAG_DISK_CLAIM` with an endpoint on offer (`sys_call_offer_self`); it takes it, watches the claimant, and from then on answers that TID alone (`TAG_ERROR` code 5 for anyone else) until the claimant dies. The VFS claims before its first read. The keyboard driver and the input server the same way.

- [ ] **Step 3: Input without blocking.** Replace `serve_read`'s key loop with a pending reader (`reader: Option<(usize, usize)>`) completed from the tick-driven key pump; `TAG_READ` while a reader is pending is deferred behind it, as a read during a claim is.

- [ ] **Step 4: Fix what the fuzzer finds.** Every crash, hang or wrong answer is fixed in the server that has it, the seed that found it goes in the commit message, and `fuzz.tests` gains that seed if it is not one of the three. The fuzzer runs until `runtests /etc/fuzz.tests` passes three times in a row with three fresh seeds (from `sys_getrandom`, printed) as well.

- [ ] **Step 5: Verify.** Boot: `runtests /etc/fuzz.tests` passes; afterwards `dtest`, `runtests /etc/libc.tests`, `ls /`, typing a line at the prompt, `wm weston-simple-shm` all behave; serial has no `PANIC`, `UFAULT` or `KFAULT`; `check-rootfs.sh` is clean (the fuzzer's `/tmp/qfuzz` garbage is in a consistent filesystem); the same on `make hd-ext4`.

- [ ] **Step 6: Commit.** quark: "Services survive a fuzzer" (plus one commit per fix that deserves its own); explosion: "fuzz.tests".

---

### Task 16: `wm` survives a bad client

**Files:**
- Create: `explosion/toolchain/wlfuzz.c`; Modify: `explosion/toolchain/build-weston-client.sh` (builds it; no libwayland — it writes the wire format itself)
- Modify: `quark/user/wm/src/client.rs`, `quark/user/wm/src/protocol.rs` and whatever else it breaks

**Interfaces:**
- Produces: `wlfuzz <seed> <messages>` — on descriptor 3 (`WAYLAND_SOCKET`), a `get_registry` and a `sync`, then `<messages>` messages built from the seed: object ids 0, 1, 2, recently created, and random; opcodes 0–20 and random; sizes below the header, odd, past the end of what was sent, and right; strings with a wrong length, no NUL, or a huge length; arrays with bad lengths; `SCM_RIGHTS` descriptors where none are expected, and missing where they are (`wl_shm.create_pool` with a pool size larger than the memfd, or with `/dev/null`); `wl_surface.attach` of buffers that were destroyed. It stops when the compositor closes the connection, prints how many messages were sent and whether a `wl_display.error` arrived, and exits 0 either way: the test is whether the compositor lives.

A client that breaks the protocol is sent `wl_display.error` with the object and a code, and disconnected, as the Wayland specification says. Nothing a client sends may take down `wm` or reach another client.

- [ ] **Step 1: The fuzzer and a first run.** Build `wlfuzz`; image; a key script that runs, for seeds 1 to 20, `wm "wlfuzz <seed> 500" wlcairo` for eight seconds each (two screenshots a second apart, to see `wlcairo`'s square turn), then Esc. Expected on today's compositor: whatever it finds.

- [ ] **Step 2: Fix what it finds.** Every compositor fault or hang, and every case where a bad message is accepted without an error, is fixed; the seeds that found them are recorded in the commit message and added to the script's list.

- [ ] **Step 3: Verify.** The script, twenty seeds and the recorded ones: `wlcairo` keeps turning in every session, serial has no `UFAULT` for the compositor, each session ends on Esc and the console comes back. `wm weston-simple-shm wlcairo wlclip wlprobe` behaves as before.

- [ ] **Step 4: Commit.** quark: "wm survives a bad client"; explosion: "wlfuzz".

---

### Task 17: Every program takes bad arguments

**Files:**
- Create: `explosion/tools/gen-hostile-tests.sh`; Modify: `explosion/Makefile` (writes `stage/etc/hostile.tests` after staging, listing what is in `stage/usr/bin`)
- Modify: every program the sweep breaks (`quark/user/*`, `explosion/toolchain/*.c`)

**Interfaces:**
- Produces: `hostile.tests` — for every program in `/usr/bin` except `shutdown`, `login`, `wm`, `runtests` and `qfuzz` (which the sweep must not start with arbitrary arguments), one `?@30` line with each of: no arguments; `-`; `--`; `-x`; `--help`; a 300-byte word; `/nonexistent`; `/etc`; `/dev/null`; `/usr/bin/ls`; `99999999999999999999`; `-1`; `0`; UTF-8 (`héllo`); a control byte (`\x01`); and fifteen arguments. A program may complain and exit with any status; it may not fault or run past its deadline. Programs that read standard input get none (runtests gives them none) and must say so rather than wait.

- [ ] **Step 1: The first sweep.** Generate, image, boot, `runtests /etc/hostile.tests`. Expected: a list of faults and timeouts.

- [ ] **Step 2: Fix every one.** In Rust programs: no `unwrap`, `expect` or indexing on anything that came from outside; parse errors are messages. In C programs: check `argc` and every call that can fail. Clients started without a compositor say so and exit 1. Each fix is checked by rerunning the sweep.

- [ ] **Step 3: Verify.** `runtests /etc/hostile.tests` passes; serial has no `UFAULT` or `PANIC`; `dtest` and every suite still pass; `check-rootfs.sh` is clean.

- [ ] **Step 4: Commit.** quark: "Every program takes bad arguments"; explosion: "hostile.tests".

---

### Task 18: Write it down

- [ ] `quark/CLAUDE.md`: new invariants — a program is its address space (handles, working directories and locks belong to it); a reserved page is a marker, not an empty entry, and page-table code tests for an all-zero entry; a mapped file's pages are the object's, counted by the slot bits in every entry that names it; a page-in call carries `PAGER_BIT` and nothing else may claim to be one; the display and the keyboard are stacks; death notices come only from sender 0; drivers answer only their server; a server never blocks on one client. Known gaps: remove the ones fixed here (links, working directory, record locks, `getrandom`, file mapping, the orphan list, demand paging) and add what remains (for example: no `mprotect`; truncating a file leaves pages already mapped past the new end; `std::fs` is unsupported).
- [ ] `quark/docs/abi.md` and `quark/docs/vfs.md`: every new call, tag, flag and error, with ABI 2.2's row.
- [ ] `explosion/toolchain/README.md`: the font caches, FreeType mapping its fonts, the new tests and fuzzers, `runtests`' `?` and `@N`.
- [ ] `~/src/osdev/ROADMAP.md`: a Phase 17 section (what it fixed, what the fuzzers found, what is left), the running order (17 ran between 13 and 14), and Phase 14 next.
- [ ] Tick this plan; commit and push quark, explosion and the fork.

---

## Acceptance

`linktest`, `cwdtest`, `locktest`, `randtest`, `lazytest`, `maptest`, `threadfile`, `filetest` and `dirtest` pass on ext2 and ext4, and `e2fsck` is clean after them; `crash-test.sh` recovers an orphan on both. `fttest` and `cairotext` keep their checksums with FreeType mapping its fonts, and the image's font caches are used as they come. `qfuzz`, `wlfuzz` and the hostile-argument sweep leave every service, the compositor and every program standing. `wm wm`, `wm nonexistent`, `wm "ls /etc"` and a line of eighty characters all do what they say. `dtest`, every earlier suite, the compositor and `hello` still pass.
