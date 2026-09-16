# Close the Standing Gaps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This project runs plans inline (superpowers:executing-plans), on `main`, one commit per task.

**Goal:** No task holds a `PhysRange` broader than the device it drives, and `sys_send`/`sys_call` are gated by a capability that names an endpoint rather than a bit in a TID mask.

**Architecture:** Two halves. *Memory:* a call may **lend** a buffer to the task it calls; that task copies into or out of it through the kernel (`SYS_LENT_READ`/`SYS_LENT_WRITE`) while the call lasts, so the disk, VFS and NET servers stop mapping client frames named by physical address — and stop needing `PhysRange` at all. init is started with exactly the framebuffer and its boot modules instead of all of memory. *Endpoints:* every task has an endpoint with a number that is never reused; an `Endpoint` capability (new type 8) records that number. The nameserver holds one for each service — offered with the registration call (`SYS_CALL_OFFER`, taken with `SYS_CAP_TAKE`) — and grants a copy to whoever looks the service up. The TID-bitmask type 7, the self-mint special case and the reap-time sweep are withdrawn at ABI 2.0.

**Tech Stack:** Rust (`no_std` kernel and user space, nightly-2026-03-01), C (Quark libc, linux-abi under musl), QEMU/OVMF via `explosion/tools/boot-test.sh`.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 12 — Close the standing gaps", and `quark/CLAUDE.md` "Known gaps".

## Global Constraints

- "Do the disk driver first — it has one request shape and one client that matters — then VFS, then NET. Each is a protocol change on a service and its callers, so each is its own commit and its own boot test."
- "SMP and a stable driver ABI stay off the list."
- **Done when** "no task in the system holds a `PhysRange` broader than the device it drives, and `sys_send` is gated by a capability that names an endpoint rather than a bit in a mask."
- `docs/abi.md` is the contract; `tools/check-abi.sh` must pass; additions bump the minor version, an incompatible change the major, and syscall numbers and capability type numbers are never reused.
- Every task ends with a boot test: `explosion/tools/boot-test.sh <keys> <ppm>` after `make hd WAYLAND_CLIENTS=$PWD/clients TEST_SUITES="<suite dirs>"` in `explosion`, with `PATH="$HOME/.local/bin:$HOME/opt/cross/bin:$PATH"`.
- Commit messages end with the session's `Co-Authored-By`/`Claude-Session` lines.

## Design decisions (and why)

**Lending, not sharing.** The servers speak synchronous IPC, not streams, so a
memory *descriptor* has no channel to travel on, and a shared-memory *handle*
is a global number — a server that maps whatever handle a request names can be
pointed at another client's buffer. A buffer lent with the call is named by
nothing but the call: the server can reach it only while that client is blocked
calling it, only inside the range and with the access the client declared, and
never learns where it lives. This is QNX's `MsgRead`/`MsgWrite`. The kernel does
the copy page by page through the identity map, re-walking the client's page
tables each time because a thread sharing the client's address space may run in
between.

**One endpoint per task, addressed by TID.** IPC keeps naming destinations by
TID, so no call site changes shape; what changes is the check. The number a
capability records is the endpoint's, not the TID, and it is never reused, so a
capability cannot pass to a slot's next occupant and the reap-time sweep has
nothing left to do.

**Capabilities travel with calls, in both directions.** Replies already could:
`sys_cap_grant` accepts a destination blocked calling the granter, which is how
`fb` lends the display. The other direction is new: a caller *offers* one slot
for the length of one call and the callee *takes* it or not, so nothing can fill
a server's CSpace uninvited. The nameserver takes an offered capability from
each service that registers and grants a copy to each client that looks it up;
`fb` takes one from each claimant so it can call it back; the keyboard takes one
from the input server so it can deliver Ctrl-C.

**Who may mint an `Endpoint`.** The endpoint's owner, the task that created the
owner, or a task already holding one for it. That is ownership, not a special
case — the old rule existed because a TID set could only be narrowed.

**CSpace grows to 64 slots.** A task now holds one `Endpoint` per service it
talks to. "Any slot" grants and takes land in slots 16–63, clear of the fixed
slots manifests and spawners use (0–15), and granting an `Endpoint` the
destination already holds returns the slot it is in rather than using another,
so repeated lookups cost nothing.

**ABI 2.0 at the end, with a recorded exception.** Type 7 is deprecated when
type 8 arrives (1.13) and withdrawn in 2.0 without the usual full major version
of overlap: keeping it keeps TID-named authority, its sweep and its special case
in the kernel, which is what this phase removes, and nothing outside this tree
is built against 1.x. `docs/abi.md` says so.

---

## File map

| File | Change |
|---|---|
| `src/lend.rs` (new) | Constants and the page-by-page copy for lent buffers |
| `src/ipc.rs` | Per-call lent buffer and offered slot; `lent_to`, `offer_to`, `take_offer` |
| `src/syscall.rs` | `SYS_CALL_LEND` 23, `SYS_CALL_OFFER` 24, `SYS_LENT_READ` 25, `SYS_LENT_WRITE` 26, `SYS_CAP_TAKE` 91, `SYS_CAP_READ` 92; `ANY_SLOT` grants; type-8 minting; ABI 1.12 → 1.13 → 2.0 |
| `src/cap.rs` | Endpoint numbers; type 8; `MAX_CAPS` 64; received-slot search; later, type 7 and the sweep removed |
| `src/scheduler.rs` | Assign/clear endpoint numbers; `task_cr3`, `parent_of` accessors |
| `src/userspace.rs` | init's `PhysRange` = framebuffer + modules |
| `src/main.rs` | `mod lend;` |
| `docs/abi.md`, `CLAUDE.md` | Contract and invariants |
| `user/quark-rt/src/syscall.rs` | Wrappers and constants for all of the above |
| `user/quark-rt/src/{vfs,net,spawn,nameserver}.rs` | Lending clients; offered registration |
| `user/libc/{include/quark/syscall.h,include/quark/vfs.h,src/quark.c,src/io.c}`, `user/linux-abi/src/{files.c,manifest.c}` | Lending C clients; no transfer page |
| `user/disk`, `user/vfs`, `user/net` | Servers copy through lent buffers; manifests drop `PhysRange` |
| `user/{init,disktest,cat,fstest,login,runtests,socktest,qsh}` | Clients lend; manifests drop what they no longer use |
| `user/{nameserver,fb,keyboard,input,qtty,wm,dchild}` | Endpoint objects, offers and takes |
| `user/dtest`, `user/dchild`, `user/nettest` (new) | Tests |
| `explosion/tools/boot-test.sh`, `explosion/tools/echo-server.py` (new) | A NIC and a host echo server for NET's test |

---

### Task 1: Read any task's capabilities, and a test that fails until the phase is done

**Files:**
- Modify: `src/syscall.rs` (constant, dispatcher arm, ABI minor → 12)
- Modify: `user/quark-rt/src/syscall.rs` (constant, `CapInfo`, `sys_cap_read`)
- Modify: `user/dtest/src/main.rs` (`test_physical_authority`)
- Modify: `docs/abi.md` (row, 1.12 entry, version)

**Interfaces:**
- Produces: `syscall::sys_cap_read(tid, slot) -> Result<CapInfo, ()>` with `CapInfo { cap_type: u64, param0: u64, param1: u64, valid: bool }`; fails past the last slot, for a missing task, or without `TaskMgmt` over the target (a task may always read its own).

- [x] **Step 1: The kernel call** — `pub const SYS_CAP_READ: u64 = 92;` and, beside `SYS_CAP_INSPECT`:

```rust
SYS_CAP_READ => {
    // arg0 = tid, arg1 = slot, arg2 = out: type, param0, param1, valid
    let caller = scheduler::current_tid();
    let tid = arg0 as usize;
    let slot = arg1 as usize;
    if slot >= crate::cap::MAX_CAPS || !validate_user_ptr_mut(arg2, 32) {
        return u64::MAX;
    }
    if tid != caller && !crate::cap::task_has_task_mgmt(caller, tid) {
        return u64::MAX;
    }
    let cap = match unsafe { scheduler::get_task_mut(tid) } {
        Some(t) => t.cspace[slot],
        None => return u64::MAX,
    };
    let out = [
        cap.cap_type as u64,
        cap.param0,
        cap.param1,
        crate::cap::slot_is_valid(&cap) as u64,
    ];
    let _ua = crate::cpu::UserAccess::begin();
    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), arg2 as *mut u64, 4) };
    0
}
```

`SYS_CAP_INSPECT` truncates both parameters to sixteen bits, which cannot show a
physical range, and only reads the caller's own CSpace.

- [x] **Step 2: The wrapper** in quark-rt, with `SYS_CAP_READ: u64 = 92`, a
`CapInfo` struct as above, and `sys_cap_read` calling `syscall3` with a
four-word out array. `sh tools/check-abi.sh` must agree.

- [x] **Step 3: The failing test** — in dtest, run first:

```rust
/// A capability over more physical memory than this is not one device's: a
/// framebuffer is a few megabytes, a boot module one. The grants this phase
/// removes were four gigabytes.
const DEVICE_SPAN: u64 = 64 << 20;
/// Where the kernel is loaded, which no task may map.
const KERNEL_IMAGE: u64 = 0x10_0000;

fn test_physical_authority() {
    println!("physical memory authority:");
    let mut seen = 0;
    let mut broad = 0;
    let mut kernel = 0;
    for tid in 1..64 {
        if syscall::sys_task_info(tid).is_err() {
            continue;
        }
        for slot in 0.. {
            let Ok(cap) = syscall::sys_cap_read(tid, slot) else { break };
            if cap.cap_type != syscall::CAP_TYPE_PHYS_RANGE || !cap.valid {
                continue;
            }
            seen += 1;
            if cap.param1.saturating_sub(cap.param0) > DEVICE_SPAN {
                println!("    tid {} may map {:#x}..{:#x}", tid, cap.param0, cap.param1);
                broad += 1;
            }
            if cap.param0 <= KERNEL_IMAGE && KERNEL_IMAGE < cap.param1 {
                kernel += 1;
            }
        }
    }
    check("another task's capabilities can be read", seen > 0);
    check("no task may map more than one device's memory", broad == 0);
    check("no task may map the kernel", kernel == 0);
}
```

- [x] **Step 4: Run it** — boot, `dtest`. Expected: the first check passes
(`fb` holds the framebuffer); the other two fail, naming init, disk, vfs and
net with `0x0..0x100000000`.

- [x] **Step 5: Document** — `docs/abi.md`: the row for 92 in the capability
table, a paragraph on why it exists beside 83, `**Version 1.12.**`, and a 1.12
line in "What each minor added". `ABI_VERSION_MINOR = 12`.

- [x] **Step 6: Commit** — "Read another task's capabilities whole", noting
that dtest's new checks fail on purpose until the phase is done.

---

### Task 2: A call can lend a buffer

**Files:**
- Create: `src/lend.rs`
- Modify: `src/main.rs` (`mod lend;`), `src/ipc.rs`, `src/scheduler.rs` (`pub fn task_cr3(tid) -> usize`), `src/syscall.rs`
- Modify: `user/quark-rt/src/syscall.rs`, `user/libc/include/quark/syscall.h`
- Modify: `user/dtest/src/main.rs` (`test_lent_buffers`)
- Modify: `docs/abi.md` (IPC rows, a "Lending memory with a call" section, 1.12 entry)

**Interfaces:**
- Produces (kernel): `SYS_CALL_LEND` 23 `(dest, msg, reply out, buf, len | LEND_READ | LEND_WRITE)`; `SYS_LENT_READ` 25 and `SYS_LENT_WRITE` 26 `(client, offset, local buf, len)` → bytes copied or `u64::MAX`.
- Produces (quark-rt): `LEND_READ = 1 << 62`, `LEND_WRITE = 1 << 63`; `sys_call_lend(dest, &Message, &mut Message, &[u8]) -> Result<(), ()>` (lends for reading); `sys_call_lend_mut(dest, &Message, &mut Message, &mut [u8]) -> Result<(), ()>` (lends for writing); `sys_call_lend_rw(dest, &Message, &mut Message, &mut [u8]) -> Result<(), ()>` (both); `sys_lent_read(client, offset, &mut [u8]) -> Result<usize, ()>`; `sys_lent_write(client, offset, &[u8]) -> Result<usize, ()>`.
- Produces (C): `SYS_CALL_LEND`, `SYS_LENT_READ`, `SYS_LENT_WRITE`, `QUARK_LEND_READ`, `QUARK_LEND_WRITE` in `quark/syscall.h`.

- [x] **Step 1: The failing test** — a thread lends dtest's main task a buffer
three ways, and main plays server:

```rust
static mut LEND_BUF: [u8; 64] = [0; 64];
static LEND_SERVER: AtomicUsize = AtomicUsize::new(0);
static LEND_GO: sync::Semaphore = sync::Semaphore::new(0);
/// Bit 0: first call replied. 1: second replied. 2: an unwritable buffer could
/// not be lent for writing. 3: nothing was lent to a task nobody was calling.
static LEND_RESULTS: AtomicU32 = AtomicU32::new(0);

extern "C" fn lender() -> ! {
    LEND_GO.acquire(); // until main has granted the right to call it
    let server = LEND_SERVER.load(Ordering::SeqCst);
    let mut results = 0;
    let mut reply = Message::empty();
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(LEND_BUF) };
    buf[..8].copy_from_slice(b"lent-buf");
    let ask = |tag| Message { sender: 0, tag, data: [0; 6] };
    if syscall::sys_call_lend_rw(server, &ask(1), &mut reply, buf).is_ok() { results |= 1; }
    if syscall::sys_call_lend(server, &ask(2), &mut reply, &buf[..]).is_ok() { results |= 2; }
    let args = unsafe {
        core::slice::from_raw_parts_mut(quark_rt::args::ARGS_PAGE_ADDR as *mut u8, 16)
    };
    if syscall::sys_call_lend_mut(server, &ask(3), &mut reply, args).is_err() { results |= 4; }
    let mut probe = [0u8; 1];
    if syscall::sys_lent_read(server, 0, &mut probe).is_err() { results |= 8; }
    LEND_RESULTS.store(results, Ordering::SeqCst);
    syscall::sys_exit_code(0);
}
```

(`sys_call_lend_rw` lends for both — the first call needs both.) Main mints an
`Endpoint` naming itself into `SLOT_SCRATCH`, grants it to the thread's
`SLOT_ENDPOINT`, deletes the scratch copy, releases `LEND_GO`, then:

```rust
let t = t.tid();
let mut msg = Message::empty();
let mut got = [0u8; 8];
check("the lending call arrives", syscall::sys_recv(t, &mut msg).is_ok() && msg.tag == 1);
check("read what was lent", syscall::sys_lent_read(t, 0, &mut got) == Ok(8) && &got == b"lent-buf");
check("write into what was lent", syscall::sys_lent_write(t, 4, b"XY") == Ok(2));
check("not past its end", syscall::sys_lent_read(t, 60, &mut got).is_err());
check("not at an offset that wraps", syscall::sys_lent_read(t, usize::MAX, &mut got[..1]).is_err());
let _ = syscall::sys_reply(t, &Message::empty());
check("the write landed where it was aimed", unsafe { &LEND_BUF[..8] } == b"lentXYuf");
check("nothing is lent once the call is answered", syscall::sys_lent_read(t, 0, &mut got).is_err());
check("the read-only lend arrives", syscall::sys_recv(t, &mut msg).is_ok() && msg.tag == 2);
check("it can be read", syscall::sys_lent_read(t, 0, &mut got) == Ok(8));
check("but not written", syscall::sys_lent_write(t, 0, b"Z").is_err());
let _ = syscall::sys_reply(t, &Message::empty());
let _ = wait_for(t);
let results = LEND_RESULTS.load(Ordering::SeqCst);
check("both lending calls were answered", results & 3 == 3);
check("an unwritable buffer cannot be lent for writing", results & 4 != 0);
check("nothing is lent to a task nobody is calling", results & 8 != 0);
```

The "nothing is lent once answered" check holds whatever the thread does next:
its second call cannot be further than `CallSendBlocked` until main receives it.

- [x] **Step 2: Run it** — it does not build: none of the calls exist.

- [x] **Step 3: `src/lend.rs`**

```rust
//! Memory lent with a call.
//!
//! A server reads and writes what a client lent it through the kernel, which
//! copies page by page through the identity map. The server never learns a
//! physical address, cannot reach the buffer once it has replied, and needs no
//! capability over physical memory to serve anybody — which is what let the
//! disk, VFS and NET servers give up theirs.

use crate::paging;

/// The task called may read what is lent.
pub const LEND_READ: u64 = 1 << 62;
/// The task called may write into what is lent.
pub const LEND_WRITE: u64 = 1 << 63;
/// The length, below the two access bits.
pub const LEND_LEN_MASK: u64 = LEND_READ - 1;
/// The most one call may lend.
pub const LEND_MAX: usize = 16 << 20;
/// The most one read or write copies, so interrupts are never off for long.
pub const COPY_MAX: usize = 1 << 20;
/// Every frame the allocator hands out is below this, inside the identity map.
const IDENTITY_END: usize = 0x1_0000_0000;

/// Copy `len` bytes between `local` in the current task and `at` in the
/// address space rooted at `cr3`; `into_lent` says which way.
///
/// Each page is looked up again as it is reached. The lender is blocked, but a
/// thread sharing its address space is not, and it may have unmapped the
/// buffer since the call was made.
///
/// # Safety
/// `cr3` is a live user address space; `local..local + len` has been validated
/// for the current task, writable unless `into_lent`; interrupts are off and
/// stay off, so nothing runs between checking a page and copying it.
pub unsafe fn copy(cr3: usize, at: usize, local: usize, len: usize, into_lent: bool) -> bool {
    unsafe {
        let mut done = 0;
        while done < len {
            let va = at + done;
            let n = (4096 - (va & 0xFFF)).min(len - done);
            let Some(flags) = paging::walk_flags(cr3, va) else { return false };
            if flags & paging::USER == 0 || (into_lent && flags & paging::WRITABLE == 0) {
                return false;
            }
            let Some(phys) = paging::translate(cr3, va) else { return false };
            if phys + n > IDENTITY_END {
                return false;
            }
            let _ua = crate::cpu::UserAccess::begin();
            if into_lent {
                core::ptr::copy_nonoverlapping((local + done) as *const u8, phys as *mut u8, n);
            } else {
                core::ptr::copy_nonoverlapping(phys as *const u8, (local + done) as *mut u8, n);
            }
            done += n;
        }
        true
    }
}
```

- [x] **Step 4: The call's state** — in `ipc.rs`:

```rust
/// A buffer lent with a call, for the task called to use until it replies.
#[derive(Clone, Copy)]
pub struct Lent {
    pub addr: usize,
    pub len: usize,
    pub access: u64,
}
```

`TaskIpc` gains `lent: Option<Lent>`. `call_inner` takes `lent: Option<Lent>`
and stores it in `TASK_IPC[caller]` as soon as interrupts are off, before either
delivery path; the post-wake block sets it back to `None` on every exit.
`sys_call`/`sys_call_timeout` pass `None`; new `sys_call_lend(dest, msg, lent)`
passes `Some`. `cleanup_task_ipc` clears it with the rest. And:

```rust
/// What `client` lent with the call it is blocked in to `server`, and the
/// address space it lives in. Nothing unless `server` has received that call
/// and not yet answered it.
pub fn lent_to(client: usize, server: usize) -> Option<(Lent, usize)> {
    if client >= MAX_TASKS {
        return None;
    }
    let flags = irq_save();
    let out = unsafe {
        match (TASK_IPC[client].state, TASK_IPC[client].lent) {
            (IpcState::CallBlocked(s), Some(lent)) if s == server => {
                let cr3 = scheduler::task_cr3(client);
                if cr3 != 0 { Some((lent, cr3)) } else { None }
            }
            _ => None,
        }
    };
    irq_restore(flags);
    out
}
```

- [x] **Step 5: The calls** — in `syscall.rs`, `SYS_CALL_LEND` is `SYS_CALL`
plus: split `arg4` into `access = arg4 & (LEND_READ | LEND_WRITE)` and
`len = arg4 & LEND_LEN_MASK`; refuse `access == 0`, `len == 0` or
`len > LEND_MAX`; refuse unless `validate_user_range(arg3, len, access has
LEND_WRITE)` — so a bad buffer is the caller's error, found before anything is
sent. Then `ipc::sys_call_lend(dest, &msg, Lent { addr, len, access })`.

`SYS_LENT_READ | SYS_LENT_WRITE` share an arm: `into_lent = nr ==
SYS_LENT_WRITE`; zero length copies nothing and returns 0; refuse `len >
COPY_MAX`; validate the local buffer (read-only when writing into the lent one,
writable when reading from it); `ipc::lent_to(client, current_tid())` or fail;
require `LEND_WRITE` or `LEND_READ` accordingly; require
`offset.checked_add(len) <= lent.len`; then `lend::copy(cr3, lent.addr +
offset, local, len, into_lent)` → `len` or `u64::MAX`.

- [x] **Step 6: Wrappers** — quark-rt constants and the five functions above
(`sys_call_lend_rw` included); `syscall5` carries `len | access`. C header: the
three numbers and two bits. `tools/check-abi.sh` agrees.

- [x] **Step 7: Run it** — boot, `dtest`. Expected: every lending check passes;
Task 1's two checks still fail.

- [x] **Step 8: Document and commit** — `docs/abi.md`: rows 23, 25, 26 and a
section:

> **Lending memory with a call.** `SYS_CALL_LEND` is `SYS_CALL` with a buffer
> the task called may use until it replies: read it with `SYS_LENT_READ` and
> write it with `SYS_LENT_WRITE`, at offsets inside the length lent, with only
> the access lent (`arg4` bit 62 read, bit 63 write, the rest the length, at
> most 16 MiB), and only while it has received the call and not yet answered.
> Each read or write copies at most 1 MiB. The kernel does the copying, so the
> server never learns where the memory is; the buffer is checked when the call
> is made and again, page by page, as it is copied. This is how a driver serves
> a client without any authority over physical memory.

Commit: "A call can lend a buffer".

---

### Task 3: The disk driver copies instead of mapping

**Files:**
- Modify: `user/disk/src/main.rs`
- Modify: `user/vfs/src/{main.rs,ext2.rs,ext2_alloc.rs,ext2_dir.rs,csum.rs,journal.rs}` (every disk request; `buf_phys` gone)
- Modify: `user/disktest/src/main.rs` (its sector read)

**Interfaces:**
- Consumes: Task 2's lending calls.
- Produces: the disk protocol — `TAG_READ_SECTOR (lba)` and `TAG_READ_SECTORS (lba, count ≤ 8)` lend a buffer of `512 × count` to be written; `TAG_WRITE_SECTOR (lba)` lends 512 bytes to be read. `data[1]` is no longer read.

- [x] **Step 1: The driver** — replace `TEMP_MAP_ADDR` with a page of its own,
`sys_mmap(DRIVE_BUF, 1)` at start. Reads: `ata_read_sector(lba, DRIVE_BUF)`
(or `ata_read_sectors`), then `sys_lent_write(msg.sender, 0, &buf[..512 *
count])`, error `1` if that fails. Write: `sys_lent_read(msg.sender, 0, &mut
buf[..512])` first, error `1` if it fails, then `ata_write_sector`. Manifest:
drop `phys_range`, and replace the comment with why the driver needs none.

- [x] **Step 2: The VFS as a disk client** — `DISK_IO_BUF`, the cache pages and
the journal buffers come from `sys_mmap`; `raw_read_sector(disk_tid, lba)`,
`raw_read_sectors(disk_tid, lba, count)`, `read_sector_bypass(disk_tid, lba)`
and both write paths lend `DISK_IO_BUF` (`sys_call_lend_mut` for reads, sized
`512 × count`; `sys_call_lend` for writes). `buf_phys` disappears from
`DiskState`, `Ext2State`, `init_ext2` and every call site —
`grep -rn buf_phys user/vfs` must come back empty.

- [x] **Step 3: disktest** — nothing to do: it only ever talks to the VFS, so
it moves in Task 4.

- [x] **Step 4: Verify** — boot to the shell (the VFS mounts through the new
path, so a working login is the first check), `dtest`, `disktest`,
`runtests /etc/pixman.tests` (thirty programs read off the disk). Expected:
dtest's broad-range check now names init, vfs and net but not disk.

- [x] **Step 5: Commit** — "The disk driver copies what it was lent".

---

### Task 4: The VFS copies instead of mapping, and its clients lend

**Files:**
- Modify: `user/vfs/src/{main.rs,ext2.rs}` (client reads, writes, bulk readdir; manifest)
- Modify: `user/quark-rt/src/{vfs.rs,spawn.rs}`
- Modify: `user/{init,cat,disktest,fstest,login,runtests,ls}/src/main.rs`
- Modify: `user/libc/include/quark/vfs.h`, `user/libc/src/{quark.c,io.c}`, `user/linux-abi/src/{files.c,manifest.c}`
- Modify: manifests of programs that no longer allocate frames

**Interfaces:**
- Produces (quark-rt): `vfs::read(vfs_tid, handle, buf: &mut [u8], offset: u32) -> Result<u32, u64>` (reads at most `min(buf.len(), 4096)`); `vfs::write(vfs_tid, handle, buf: &[u8], offset: u32) -> Result<u32, u64>` (at most 4096); `readdir_bulk` unchanged in signature, now lending a page.
- Produces (C): `int quark_vfs_read(unsigned long handle, void *buf, unsigned long offset, unsigned long len, unsigned long *got)` and `int quark_vfs_write(unsigned long handle, const void *buf, unsigned long offset, unsigned long len, unsigned long *put)`.

- [x] **Step 1: The server** — `CLIENT_BUF` becomes a page of the VFS's own.
`TAG_READ` fills it exactly as today, then `sys_lent_write(sender, 0,
&page[..n])`; `TAG_WRITE` first `sys_lent_read(sender, 0, &mut page[..len])`,
then writes from it as today. `read_file_data`/`write_file_data` (ext2 and
FAT32) lose their physical-address parameter. `TAG_READDIR_BULK` writes its
entries into a lent 4096-byte buffer rather than a shared-memory handle the
request names — a handle is a global number, so any client could have named
another's. Manifest: no `phys_range`, no `phys_alloc`.

- [x] **Step 2: quark-rt** — `vfs::read`/`write` as above; `readdir_bulk` lends
a stack page. `spawn::load_path` stages the image in `sys_mmap` memory and
reads each page straight into it; the frame bookkeeping goes.

- [x] **Step 3: Rust callers** — init (`FILE_BUF_BASE` pages from `sys_mmap`),
cat, disktest, fstest, login (both reads), runtests (the list) lend ordinary
buffers.

- [x] **Step 4: C callers** — `quark.c` lends through a new
`quark_call_lend(dest, msg, reply, buf, len, access)`; `io.c` and `files.c`
read and write the caller's buffer a page at a time with no transfer page,
`xfer_phys`/`xfer_page` and `XFER_VADDR` removed. `manifest.c`'s
`phys_alloc(8)` existed only for that page: check `clone.c` allocates nothing,
then leave the manifest empty with a comment saying why the object stays.

- [x] **Step 5: Manifests** — `grep sys_phys_alloc` each program and the quark-rt
modules it uses; drop `phys_alloc` from those with no caller left (cat, fstest,
disktest, login, runtests, qsh are the candidates; dtest and threadtest keep it
for thread stacks; dchild keeps it for its orphan mode's thread).

- [x] **Step 6: Verify** — rebuild the C tests and pixman
(`build-tests.sh`, `build-pixman.sh`, `build-cairo.sh`'s objects link the new
`liblinux-abi.a`), boot, and run: `dtest`, `cat` on a file, `ls /usr/bin`,
`fstest` (it writes), `runtests /etc/libc.tests`, `/etc/cairo.tests`,
`/etc/pixman.tests`, and `wm weston-simple-shm wlcairo`. Expected: all pass;
dtest's broad-range check names init and net only.

- [x] **Step 7: Commit** — "The VFS copies what it was lent".

---

### Task 5: NET copies instead of mapping

**Files:**
- Modify: `user/net/src/main.rs` (UDP and TCP send/receive; manifest)
- Modify: `user/quark-rt/src/net.rs`, `user/socktest/src/main.rs` (manifest)
- Create: `user/nettest/{Cargo.toml,.cargo/config.toml,linker.ld,src/main.rs}`; Modify: `Makefile`
- Create: `explosion/tools/echo-server.py`; Modify: `explosion/tools/boot-test.sh`

**Interfaces:**
- Produces (quark-rt): `udp_send(net_tid, ip, port, src_port, data: &[u8])`, `udp_recv(net_tid, port, buf: &mut [u8]) -> Result<(usize, [u8; 4], u16), u64>`, `tcp_send(net_tid, handle, data: &[u8]) -> Result<usize, u64>`, `tcp_recv(net_tid, handle, buf: &mut [u8]) -> Result<usize, u64>` — each lending instead of passing a frame.

- [x] **Step 1: The test harness** — `echo-server.py` echoes on UDP and TCP port
7007 of 127.0.0.1, which QEMU's user network presents to the guest as
10.0.2.2. `boot-test.sh` adds `-device rtl8139,netdev=n -netdev user,id=n` and
starts the echo server for the run, killing it by pid on exit.

- [x] **Step 2: The failing test** — `nettest`: UDP-send `"quark-udp"` to
10.0.2.2:7007 and `udp_recv` the echo; `tcp_connect` there, `tcp_send`
`"quark-tcp"`, `tcp_recv` the echo, close. One `ok`/`FAIL` line each, exit 0
only if all pass, manifest empty. Run it with today's net: it passes, because
the old page protocol works — so first change `net.rs` to lend (Step 4) and
watch it fail against the old server.

- [x] **Step 3: The server** — `CLIENT_BUF` becomes a page of NET's own.
`TAG_UDP_SEND` and `TAG_TCP_SEND` `sys_lent_read` what they send; immediate
`TAG_TCP_RECV` replies `sys_lent_write` what they deliver; the deferred ones
keep only `pending_tid` and the length, and copy with `sys_lent_write` when the
data arrives — the client is still blocked in the call, so its buffer is still
lent. `pending_phys` and `UdpReader::phys_addr` go. Manifest: drop
`phys_range`; `phys_alloc` stays for the card's own DMA buffers.

- [x] **Step 4: quark-rt `net.rs`** — the four calls lend.

- [x] **Step 5: Verify** — boot with the NIC, `nettest`, `socktest 10.0.2.2
7007` (the fd path; the echo server echoes its request), `dtest`. Expected:
both network tests pass; dtest's broad-range check names only init.
`socktest` drops `phys_alloc`.

- [x] **Step 6: Commit** — quark "NET copies what it was lent"; explosion
"Boot tests have a network card and something to talk to".

---

### Task 6: init starts with the memory it maps, and nothing more

**Files:**
- Modify: `src/userspace.rs` (`spawn_init`), `src/cap.rs` (`populate_from_bitmask`, `task_has_phys_range`)
- Modify: `CLAUDE.md` (the `CAP_MAP_PHYS` warning)

- [x] **Step 1: Kernel** — `spawn_init` gives init `CAP_ALL & !CAP_MAP_PHYS` in
both `task.caps` and its CSpace, then one kernel-rooted `PhysRange` for the
framebuffer (page-aligned `fb.addr .. fb.addr + pitch × height`) and one per
boot module (page-aligned). `populate_from_bitmask` no longer turns
`CAP_MAP_PHYS` into anything, and `task_has_phys_range` stops consulting the
per-UID bit: a legacy bit that means "all of memory" is how `SYS_CAP_TRANSFER`
and `SYS_SET_USER_CAPS` could still have handed it out.

- [x] **Step 2: Verify** — boot (fb still gets the screen, init still reads
`boot.img`), `dtest`. Expected: `no task may map more than one device's memory`
and `no task may map the kernel` pass, and the whole dtest run is clean.

- [x] **Step 3: Commit** — "init holds the framebuffer and its modules, not all
of memory"; CLAUDE.md's `CAP_MAP_PHYS` bullet says the bit now confers nothing,
and the `PhysRange` known gap is gone.

---

### Task 7: Endpoints with numbers, alongside the sets (ABI 1.13)

**Files:**
- Modify: `src/cap.rs`, `src/scheduler.rs`, `src/ipc.rs`, `src/syscall.rs`
- Modify: `user/quark-rt/src/syscall.rs` (constants, wrappers; `CAP_TYPE_ENDPOINT_SET = 7` for today's users, `CAP_TYPE_ENDPOINT = 8`)
- Modify: `user/{init,wm,dchild}/src/main.rs` (rename to `CAP_TYPE_ENDPOINT_SET`; no behaviour change)
- Modify: `user/dchild/src/main.rs` (`serve` mode), `user/dtest/src/main.rs` (`test_endpoint_objects`)
- Modify: `docs/abi.md`

**Interfaces:**
- Produces (kernel): `cap::endpoint_of(tid) -> u64` (0 = none); `CapType::Endpoint = 8` with `param0` the endpoint's number; `sys_cap_mint(slot, 8, tid, 0)` mints for `tid`'s endpoint if the caller is `tid`, created `tid`, or already holds one for it; `MAX_CAPS = 64`; `ANY_SLOT = u64::MAX - 1` for `SYS_CAP_GRANT`'s destination slot (returns the slot used; an `Endpoint` already held is not copied again); `SYS_CALL_OFFER` 24 `(dest, msg, reply out, slot)`; `SYS_CAP_TAKE` 91 `(caller, slot or ANY_SLOT)` → slot.
- Produces (quark-rt): `ANY_SLOT`, `sys_cap_grant_any(dest, src_slot) -> Result<usize, ()>`, `sys_call_offer(dest, &Message, &mut Message, slot) -> Result<(), ()>`, `sys_cap_take(caller, slot) -> Result<usize, ()>`, `sys_cap_take_any(caller) -> Result<usize, ()>`.

- [x] **Step 1: The failing tests** — dchild gains `serve`: receive one call,
reply tag 42, exit 0. dtest:

```rust
fn test_endpoint_objects() {
    println!("endpoints:");
    let me = syscall::sys_getpid() as usize;
    // Minting for yourself is ownership; minting for a stranger is not.
    check("a task may mint a capability to itself", mint_endpoint(SELF_SLOT, me));
    check(
        "but not to a task it did not make",
        !mint_endpoint(STRANGER_SLOT, quark_rt::nameserver::NAMESERVER_TID),
    );
    // A capability names a task, not the slot it ran in.
    let Some(a) = load_child(&[b"dchild", b"serve"]) else {
        check("start a child to call", false);
        return;
    };
    let _ = a.start();
    check("a creator may mint a capability to its child", mint_endpoint(CHILD_SLOT, a.tid));
    check("which reaches it", call_tag(a.tid) == Some(42));
    let _ = wait_for(a.tid);
    let Some(b) = load_child(&[b"dchild", b"serve"]) else {
        check("start a second child", false);
        return;
    };
    let _ = b.start();
    check("the next child takes the same slot", b.tid == a.tid);
    check("and the old capability does not reach it", call_tag(b.tid).is_none());
    let _ = mint_endpoint(CHILD_SLOT + 1, b.tid);
    let _ = call_tag(b.tid); // lets it finish
    let _ = wait_for(b.tid);
    // The offer and any-slot checks follow, and every slot used is deleted.
}
```

`SELF_SLOT`, `STRANGER_SLOT` and `CHILD_SLOT` are 40, 41 and 42 — in the
received range, clear of dtest's fixed slots.

`mint_endpoint(slot, tid)` is `sys_cap_mint(slot, CAP_TYPE_ENDPOINT, tid, 0)`;
`call_tag` is a `sys_call_timeout` of 50 ticks returning the reply tag. The
offer check: a thread (granted a capability to call main, as in Task 2) makes a
`sys_call_offer` of a capability naming itself; main's `sys_cap_take_any` returns
a slot whose `sys_cap_read` says type 8, a second take fails, and a take from a
task not calling main fails. The any-slot check: two `sys_cap_grant_any` of the
same child capability into the child return the same slot, and that slot is 16
or above.

- [x] **Step 2: Run it** — it does not build (type 8, the new calls).

- [x] **Step 3: Numbers** — in `cap.rs`:

```rust
/// Each task's endpoint, as a number no endpoint has had before or will again.
///
/// An `Endpoint` capability records this, not the TID, so it names the task it
/// was minted for and nothing else. When that task is reaped its number is
/// gone, and a capability holding it names nothing; whatever takes the slot
/// next has a number of its own. The TID sets needed a sweep of every CSpace at
/// reap time to approximate that.
static mut ENDPOINTS: [u64; MAX_TASKS] = [0; MAX_TASKS];
static NEXT_ENDPOINT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Give `tid` a fresh endpoint. Called when a task slot is filled.
pub fn open_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        let id = NEXT_ENDPOINT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe { ENDPOINTS[tid] = id };
    }
}

/// `tid`'s endpoint is gone, and its number with it. Called when it is reaped.
pub fn close_endpoint(tid: usize) {
    if tid < MAX_TASKS {
        unsafe { ENDPOINTS[tid] = 0 };
    }
}

/// The number of `tid`'s endpoint, or 0 if there is no such task.
pub fn endpoint_of(tid: usize) -> u64 {
    if tid < MAX_TASKS { unsafe { ENDPOINTS[tid] } } else { 0 }
}
```

`scheduler::spawn` and `create_empty_task` call `open_endpoint` when they fill a
slot; `reap_one` calls `close_endpoint` before `TASKS[i] = None`.
`scheduler::parent_of(tid) -> Option<usize>`.

- [x] **Step 4: Type 8** — `CapType::EndpointSet = 7` (the old variant,
renamed, doc says deprecated) and `CapType::Endpoint = 8`.
`task_has_endpoint(tid, dest)`: any valid slot that is a set with `dest`'s bit,
or an `Endpoint` whose `param0` equals a non-zero `endpoint_of(dest)`.
`validate_attenuation` for `Endpoint`: equal `param0`. `SYS_CAP_MINT` for type
8 resolves `param0` as a TID: `endpoint_of(tid)` must be non-zero and the
caller must be `tid`, `parent_of(tid)`, or hold a valid `Endpoint` with that
number; the slot stores the number and `param1 = 0`.

- [x] **Step 5: Room** — `MAX_CAPS = 64`; `pub const RECEIVED: Range<usize> =
16..MAX_CAPS`; `pub fn receive_slot(cs: &CSpace, cap: &CapSlot) -> Option<usize>`
returns the slot of a valid `Endpoint` with the same number if there is one,
else the first empty slot in `RECEIVED`. `SYS_CAP_GRANT` with `arg2 ==
ANY_SLOT` uses it and returns the slot; an explicit slot still returns 0.

- [x] **Step 6: Offers** — `TaskIpc.offer: Option<usize>`, set by
`sys_call_offer` like `lent`, cleared the same way;
`ipc::take_offer(caller, taker) -> Option<usize>` returns and clears it when
`caller` is `CallBlocked(taker)`. `SYS_CALL_OFFER` validates the slot holds a
valid capability before calling. `SYS_CAP_TAKE` copies that slot into the
taker exactly as `SYS_CAP_GRANT` derives (root provenance, current
generation), into `receive_slot` for `ANY_SLOT` or the explicit empty slot, and
returns the slot.

- [x] **Step 7: Wrappers and renames**; `check-abi.sh`; build.

- [x] **Step 8: Verify** — boot, `dtest`: every check passes (Tasks 3–6 made
Task 1's pass). The `runtests` suites and `wm` behave as before.

- [x] **Step 9: Document and commit** — `docs/abi.md`: rows 24, 91; type 8 and
`ANY_SLOT` under capabilities; type 7 deprecated; `**Version 1.13.**`. Commit
"Endpoints with numbers that are never reused".

---

### Task 8: Services hand out endpoints

**Files:**
- Modify: `user/nameserver/src/main.rs`, `user/quark-rt/src/nameserver.rs`
- Modify: `user/init/src/main.rs` (service masks removed)
- Modify: `user/fb/src/main.rs`, `user/qtty/src/main.rs`, `user/wm/src/main.rs` (claims offer; fb takes)
- Modify: `user/keyboard/src/main.rs`, `user/input/src/main.rs` (Ctrl-C registration offers; keyboard takes)
- Modify: `user/dchild/src/main.rs` (`register`, `lookup` modes; its CSpace test uses type 8), `user/dtest/src/main.rs` (`test_runtime_service`; Task 2's grant uses type 8)
- Modify: `CLAUDE.md` ("Events are pulled" reason)

**Interfaces:**
- Consumes: Task 7.
- Produces: the nameserver protocol — `TAG_REGISTER` must offer a capability naming the registrant (else refused; a name held by a live task is refused too); `TAG_LOOKUP` grants the caller a copy into any slot before replying with the TID (and `TAG_NOT_FOUND` if the grant fails); registrants are watched and dropped when they die.

- [x] **Step 1: The failing test** — dchild `register NAME` registers and
serves one call; `lookup NAME` looks up and exits with the reply tag. dtest
starts `register dchild-svc`, waits until its own lookup succeeds (up to a
second), starts `lookup dchild-svc`, and checks the exit status is 42 — a
program reaching a service it was never introduced to. Against Task 7's tree it
fails: nothing grants the capability.

- [x] **Step 2: The nameserver** — as in the interfaces; `sys_cap_take_any`,
`sys_cap_grant_any`, `sys_task_watch`, and `TAG_TASK_DIED` removes the entry and
`sys_cap_delete`s its slot. The nameserver's own entry has no slot.

- [x] **Step 3: Registering** — `nameserver::register` mints a capability naming
the caller into `SLOT_SCRATCH`, offers it with the call, and deletes it.

- [x] **Step 4: init** — `SERVICE_MASK`, `add_service`, `service_mask`, the
final top-up loop and `SLOT_ENDPOINT_EXTRA` use go. `grant_endpoints(tid,
slot)` mints a capability to the nameserver (init created it) and grants it;
every program gets that one in `SLOT_ENDPOINT`. init mints its own to the
nameserver and to `fb` before calling either.

- [x] **Step 5: Calling back** — qtty and wm claim the display with
`sys_call_offer` of a capability naming themselves; fb takes it into any slot,
keeps the slot for `OWNER` and `PREVIOUS`, and deletes a slot when its task is
neither any more. input registers for Ctrl-C with an offer; the keyboard takes
it and keeps the slot. wm mints its own `Endpoint` (type 8) for its clients.

- [x] **Step 6: dchild's CSpace test and dtest's lending thread** use type 8.

- [x] **Step 7: Verify** — boot, `dtest` (all pass, including the runtime
service), the three suites, `wm weston-simple-shm wlcairo` (then close it so
`fb` hands the display back to qtty — the call-back path), Ctrl-C at a running
`cat` of a large file, `ipcping`, `ps`. Serial shows no `[cap] ... denied`.

- [x] **Step 8: Commit** — "Services hand out the right to call them".

---

### Task 9: Withdraw the TID sets (ABI 2.0)

**Files:**
- Modify: `src/cap.rs`, `src/scheduler.rs`, `src/syscall.rs`, `docs/abi.md`, `CLAUDE.md`
- Modify: `user/quark-rt/src/syscall.rs`, `user/dtest/src/main.rs`

- [x] **Step 1: The failing test** — dtest: minting type 7 fails.

- [x] **Step 2: Remove** — type 7 from minting, attenuation, `task_has_endpoint`
and `populate_from_bitmask` (the `CAP_ENDPOINT` bit confers nothing); the
self-bit rule in `can_mint`; `revoke_endpoints_to` and its call in `reap_one`;
the per-UID `CAP_ENDPOINT` path. `CAP_TYPE_ENDPOINT_SET` leaves quark-rt.
`ABI_VERSION_MAJOR = 2`, minor 0.

- [x] **Step 3: Document** — `docs/abi.md`: version 2.0; a "2.0" section saying
what changed and why the usual deprecation period was not kept; type 7 listed
as withdrawn. CLAUDE.md: the IPC invariant describes endpoint numbers, the
endpoint known gap goes, and a new gap records that servers still key clients by
TID (`Message.sender`).

- [x] **Step 4: Verify** — as Task 8, plus `hello` (the hosted build relinks
against quark-rt).

- [x] **Step 5: Commit** — "Withdraw TID sets: ABI 2.0".

---

### Task 10: Write it down

- [ ] `ROADMAP.md`: Phase 12 done, with what it found; the running order.
- [ ] Tick this plan; commit it; push quark and explosion.

---

## Acceptance

dtest reports no task able to map more than a device's memory or the kernel;
the disk, VFS and NET manifests ask for no `PhysRange`; a program reaches a
service it was never introduced to; a capability does not reach a slot's next
occupant; minting a TID set fails; every suite, the compositor and the network
tests pass on the final tree.
