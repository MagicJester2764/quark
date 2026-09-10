# Quark syscall ABI

**Version 1.8.** Query the running kernel with `SYS_ABI_VERSION` (240), which
returns `(major << 16) | minor`.

This document is the contract between the Quark kernel and everything above it.
A service, a language runtime, or a C library should be buildable against this
document alone, without reading kernel source.

## Calling convention

```
RAX = syscall number
RDI = arg0   RSI = arg1   RDX = arg2   R10 = arg3   R8 = arg4   R9 = arg5
```

Entry is `syscall`/`sysret` via the STAR/LSTAR/SFMASK MSRs. The return value is
in RAX. R10 is used rather than RCX because `syscall` clobbers RCX with the
return address.

The kernel clobbers RDI, RSI, RDX, R8, R9 and R10; callers must treat them as
volatile. Caller-saved scratch registers are scrubbed before `sysret` so kernel
values do not leak back to user space. RFLAGS.AC is cleared on entry by SFMASK,
so a task cannot pre-open the SMAP window before trapping in.

## Return encoding

There is no `errno`. Each call returns a single `u64`:

- **`u64::MAX` (`0xFFFF_FFFF_FFFF_FFFF`) means failure.** No call returns it as
  a success value.
- Everything else is call-specific: a handle, a count, a packed struct, or `0`
  for "succeeded, nothing to report".

Two calls deviate deliberately and say so below: `SYS_CALL_TIMEOUT` returns `1`
for a timeout, because timing out is an answer rather than a failure to ask; and
`SYS_FD_READ_NB` returns `0xFFFF_FFFE` for "would block", distinct from `0`,
which is end of file.

This is deliberately coarse. It is enough to build a libc's `errno` on top of
only where a call is extended to report a reason; do not assume a failing call
tells you *why* it failed.

## Numbering

Numbers are assigned in 16-slot blocks, one subsystem per block, so a new call
lands beside its relatives rather than in whatever gap happens to be free. The
unused slots in a block are reserved for that subsystem.

| Block | Range     | Subsystem                     |
|-------|-----------|-------------------------------|
| 0x00  | 0–15      | Process lifecycle             |
| 0x10  | 16–31     | IPC                           |
| 0x20  | 32–47     | Memory                        |
| 0x30  | 48–63     | Shared memory                 |
| 0x40  | 64–79     | File descriptors and pipes    |
| 0x50  | 80–95     | Capabilities                  |
| 0x60  | 96–111    | Task lifecycle and identity   |
| 0x70  | 112–127   | Hardware and drivers          |
| 0x80  | 128–143   | Synchronisation               |
| 0x90  | 144–159   | Time                          |
| 0xA0  | 160–175   | Kernel debug console          |
| 0xB0  | 176–191   | Sockets                       |
| 0xF0  | 240–255   | ABI introspection             |

Blocks 0xC0–0xE0 are unassigned and available for new subsystems. Threads
never needed one: a thread is a task started with its creator's address space,
so it is built from calls that already existed.

## Stability and deprecation

- **Numbers are never reused.** A withdrawn call leaves its slot empty. A
  program built against an older minor version that calls a withdrawn number
  gets a clean failure rather than a different call's behaviour.
- **Minor version** increases when calls are added. Additions never move or
  change existing calls, so an older program keeps working.
- **Major version** increases when a call's meaning, arguments, or return
  encoding changes incompatibly. A program should read `SYS_ABI_VERSION` and
  refuse to run against a major it was not built for.
- **Deprecation is a three-step process**: mark the call deprecated in this
  document with its replacement; keep it working for at least one major
  version; then withdraw it and leave the slot empty.

### What each minor added

| Version | Added |
|---|---|
| 1.0 | The ABI as first frozen. |
| 1.1 | `SYS_ADDRSPACE_SELF` (41), `SYS_SET_FS_BASE` (102), `SYS_TASK_START_ARG` (103) — what a thread needs: the caller's own address space to share, a per-task FS base for thread-local storage, and an argument to hand the new thread. |
| 1.2 | `SYS_FUTEX_WAIT_TIMEOUT` (130). |
| 1.3 | `SYS_SOCK_FD` (176), `SYS_SOCK_INFO` (177) — a network connection as a file descriptor. |
| 1.4 | `SYS_TASK_WATCH` (104) — be told when a task dies, so what it was lent can be taken back. |
| 1.5 | `SYS_TASK_PRIORITY` (105) — which scheduling band a task runs in. |
| 1.6 | `SYS_MMAP_FD` (42), `SYS_MEMFD_CREATE` (53), `SYS_FD_CLOSE` (71), `SYS_SOCKETPAIR` (72), `SYS_FD_SEND` (73), `SYS_FD_RECV` (74), `SYS_POLLSET_CREATE` (75), `SYS_POLLSET_CTL` (76), `SYS_POLLSET_WAIT` (77), `SYS_POLL` (78) — a bidirectional stream, descriptor passing, memory named by a descriptor, and waiting on more than one thing at once. The descriptor table also goes from 8 entries to 32. |
| 1.7 | `SYS_SET_CLEAR_TID` (106) — a word to clear and wake when a task exits. Also: making a task in your own address space no longer needs `TaskMgmt`, because a thread is not a new principal. |
| 1.8 | `SYS_FD_RECV` (74) learns to choose: `at = u64::MAX - 1` asks for any free descriptor and the call returns which one it took. A caller translating `recvmsg` cannot name a slot — Linux picks the number — and probing for a free one would mean reading, which is the thing a receive must do exactly once. Both it and `SYS_FD_SEND` (73) also take flags in arg4, where bit 0 is `MSG_DONTWAIT`: return `0xFFFF_FFFE` rather than park. A reader that loops until a read finds nothing — libwayland does — hangs for ever without it, because the last turn of every such loop is the empty one. |

A capability may only be minted from one the caller already holds, and only
narrowed — with one exception. **An `Endpoint` naming only the caller may
always be minted**, whoever they are. Admitting others to call you confers no
authority over anybody else, and without it an `Endpoint` can only ever shrink:
a server started at run time could never admit a client it spawned, because its
own task ID is in nobody's destination set. It did not exist when those sets
were made.

Five calls are **deprecated as of 1.0** — the `CAP_*` object-capability calls
(80–85) replace them:

| Call | Number | Why | Replacement |
|---|---|---|---|
| `SYS_GRANT_CAP` | 86 | Expands a bitmask bit into a *wildcard* capability, e.g. `CAP_MAP_PHYS` becomes `PhysRange(0, 4 GiB)`, silently widening any narrow grant made alongside it | `SYS_CAP_MINT` + `SYS_CAP_GRANT` |
| `SYS_GRANT_IOPORT` | 87 | Same, for the full port range | `SYS_CAP_MINT` with `IoPort(start, end)` |
| `SYS_GRANT_IRQ` | 88 | Same shape | `SYS_CAP_MINT` with `Irq(n)` |
| `SYS_SET_USER_CAPS` | 89 | Per-UID authority predates capabilities and is not consulted by anything that grants correctly | per-task CSpace |
| `SYS_GET_USER_CAPS` | 90 | as above | per-task CSpace |

New code must not call these. See `CLAUDE.md` for why granting `CAP_MAP_PHYS`
in particular undoes an otherwise careful capability grant.

## Capabilities

Authority comes from capabilities, not from a privilege level — there is no
UID 0 bypass in the kernel. Each task has a CSpace of 16 slots holding
`CapSlot { cap_type, generation, root_slot, root_tid, param0, param1 }`.

| Type | param0 | param1 |
|---|---|---|
| `IoPort` | first port | last port |
| `PhysRange` | first address | last address (page aligned) |
| `Irq` | IRQ number (`0xFF` = any) | — |
| `TaskMgmt` | target TID (`0` = any) | — |
| `PhysAlloc` | max pages (`0` = unlimited) | — |
| `SetUid` | — | — |
| `Endpoint` | bitmask of destination TIDs | — |

Delegation may narrow a capability but never widen it; delegating at equal
breadth is allowed, since a set is a subset of itself.

## The calls

`Cap` names the capability required. `—` means none. Blocking calls are marked;
everything else returns promptly.

### Process lifecycle (0x00)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 0 | `SYS_EXIT` | — | does not return | — |
| 1 | `SYS_EXIT_CODE` | arg0 = status | does not return | — |
| 2 | `SYS_YIELD` | — | 0 | — |
| 3 | `SYS_GETPID` | — | current TID | — |
| 4 | `SYS_WAIT` | — | `tid \| (exit_code << 32)`, or `u64::MAX` if no children. **Blocks.** | — |
| 5 | `SYS_TASK_KILL` | arg0 = tid | 0 / `u64::MAX` | `TaskMgmt` for target, or same UID |
| 6 | `SYS_SIGNAL` | arg0 = tid, arg1 = signal bits | 0 / `u64::MAX` | as above |
| 7 | `SYS_TASK_INFO` | arg0 = tid | packed info, or `u64::MAX` | — |

`SYS_EXIT` is equivalent to `SYS_EXIT_CODE(0)`; it exists separately because
`syscall0` leaves RDI undefined, so the original call could not grow an
argument.

### IPC (0x10)

Messages are fixed size: sender TID, a `u64` tag, and six `u64` payload words.

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 16 | `SYS_SEND` | arg0 = dest, arg1 = msg | 0 / `u64::MAX`. **Blocks** until received. | `Endpoint` for dest |
| 17 | `SYS_RECV` | arg0 = from (`TID_ANY` for any), arg1 = msg out | 0 / `u64::MAX`. **Blocks.** | — |
| 18 | `SYS_CALL` | arg0 = dest, arg1 = msg, arg2 = reply out | 0 / `u64::MAX`. **Blocks** until replied. | `Endpoint` for dest |
| 19 | `SYS_REPLY` | arg0 = dest, arg1 = msg | 0 / `u64::MAX` | — |
| 20 | `SYS_CALL_TIMEOUT` | arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = ticks | **0 = replied, 1 = timed out**, `u64::MAX` = failed. **Blocks** up to the deadline. | `Endpoint` for dest |
| 21 | `SYS_RECV_TIMEOUT` | arg0 = from, arg1 = msg out, arg2 = ticks | 0 / `u64::MAX`. **Blocks** up to the deadline. | — |
| 22 | `SYS_NOTIFY` | arg0 = dest, arg1 = badge | 0 / `u64::MAX` | `Endpoint` for dest |

The `Endpoint` check applies to calls where the sender names its own
destination. IPC the kernel performs on a task's behalf through an installed
file descriptor bypasses it deliberately: the fd is itself the authorisation,
and only a `TaskMgmt` holder can install one.

`SYS_NOTIFY` rejects the reserved signal bits; those may only be raised through
`SYS_SIGNAL`, which checks the caller's authority over the target.

Prefer `SYS_CALL_TIMEOUT` over `SYS_CALL` for any destination not known to be a
running server. A TID is not a promise that anything is listening, and a plain
call to a task that never reaches `SYS_RECV` blocks forever.

### Memory (0x20)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 32 | `SYS_MMAP` | arg0 = vaddr, arg1 = pages | 0 / `u64::MAX` | — (quota enforced) |
| 42 | `SYS_MMAP_FD` | arg0 = fd naming memory, arg1 = vaddr | 0 / `u64::MAX` | — |
| 33 | `SYS_MUNMAP` | arg0 = vaddr, arg1 = pages | 0 / `u64::MAX` | — |
| 34 | `SYS_PHYS_ALLOC` | arg0 = pages | physical address / `u64::MAX` | `PhysAlloc` |
| 35 | `SYS_PHYS_FREE` | arg0 = phys, arg1 = count | 0 / `u64::MAX` | `PhysAlloc` + frame ownership |
| 36 | `SYS_ADDRSPACE_CREATE` | — | CR3 / `u64::MAX` | `TaskMgmt` |
| 37 | `SYS_ADDRSPACE_MAP` | arg0 = cr3, arg1 = virt, arg2 = phys, arg3 = pages, arg4 = flags | 0 / `u64::MAX` | `TaskMgmt` + frame ownership or `PhysRange` |
| 38 | `SYS_MAP_PHYS` | arg0 = phys, arg1 = virt, arg2 = pages | 0 / `u64::MAX` | frame ownership or `PhysRange` |
| 39 | `SYS_SET_MEM_LIMIT` | arg0 = tid, arg1 = pages (0 = unlimited) | 0 / `u64::MAX` | `TaskMgmt` |
| 40 | `SYS_SET_PAGER` | arg0 = tid, arg1 = pager tid | 0 / `u64::MAX` | `TaskMgmt` |

**Mapping authority is ownership first, `PhysRange` second.** A task may map
frames it owns — `SYS_PHYS_ALLOC` records the caller as owner — with no
capability at all, since handing back memory the allocator just gave you conveys
no new authority. `PhysRange` is required only for frames the allocator never
owned (device MMIO, the framebuffer) and for pages another task allocated.

**All user mappings must be at or above `USER_MIN_ADDR` (0x80_0000_0000).**
Lower addresses are rejected: address spaces share the page directories beneath
PML4[0], so a low mapping would write into tables every address space shares.

Physical addresses are page aligned; a request that is not is rejected.

### Shared memory (0x30)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 48 | `SYS_SHMEM_CREATE` | arg0 = pages (1–4096) | handle / `u64::MAX` | — (charged to creator's quota) |
| 49 | `SYS_SHMEM_MAP` | arg0 = handle, arg1 = vaddr | 0 / `u64::MAX` | must have been granted |
| 50 | `SYS_SHMEM_UNMAP` | arg0 = handle, arg1 = vaddr | 0 / `u64::MAX` | — |
| 51 | `SYS_SHMEM_GRANT` | arg0 = handle, arg1 = target tid | 0 / `u64::MAX` | must be the creator |
| 52 | `SYS_SHMEM_DESTROY` | arg0 = handle | 0 / `u64::MAX` | must be the creator |
| 53 | `SYS_MEMFD_CREATE` | arg0 = pages | fd naming the region / `u64::MAX` | — (charged to creator's quota) |

A region is assembled from up to sixteen contiguous runs of physical frames, so
a large one does not need a large unfragmented span: 4096 pages is sixteen
megabytes, which no allocator on a small machine will give in one piece. It was
sixteen pages until the display server needed to share a screenful with a
client, then one run of 1024 — which was exactly one 1280x800 buffer, and
therefore not two. There are 256 regions in the system.

`SYS_MEMFD_CREATE` makes the same region and names it with a descriptor, which
is what lets it be passed over a stream, inherited across a spawn, or closed
like anything else a program holds.

Destruction is deferred while any mapping remains. Futexes are keyed on physical
address, so a futex word inside a shared region is one object to every task that
maps it.

### File descriptors and pipes (0x40)

Thirty-two descriptors per task. 0, 1 and 2 are stdin, stdout and stderr by
convention. A descriptor is one of: unset, an IPC endpoint (a service TID plus a
tag), a pipe read end, a pipe write end, one end of a stream, shared memory, or
a set of descriptors to wait on.

A descriptor names an object and holds a reference to it. Closing the last one
frees the object, and is what makes a pipe's reader see end-of-file.

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 64 | `SYS_FD_READ` | arg0 = fd, arg1 = buf, arg2 = max len | bytes read, `0` = EOF, `u64::MAX` = error. **Blocks.** | — |
| 65 | `SYS_FD_WRITE` | arg0 = fd, arg1 = buf, arg2 = len | bytes written / `u64::MAX` | — |
| 66 | `SYS_FD_READ_NB` | arg0 = fd, arg1 = buf, arg2 = max len | bytes, `0` = EOF, **`0xFFFF_FFFE` = would block**, `u64::MAX` = error | — |
| 67 | `SYS_FD_SET` | arg0 = target tid, arg1 = fd, arg2 = service tid, arg3 = tag | 0 / `u64::MAX` | `TaskMgmt` |
| 68 | `SYS_FD_DUP` | arg0 = target tid, arg1 = target fd, arg2 = source fd | 0 / `u64::MAX` | `TaskMgmt` |
| 69 | `SYS_PIPE_CREATE` | — | handle / `u64::MAX` | — (bounded per task) |
| 70 | `SYS_PIPE_FD_SET` | arg0 = target tid, arg1 = fd, arg2 = pipe handle, arg3 = 1 for write end | 0 / `u64::MAX` | `TaskMgmt` |
| 71 | `SYS_FD_CLOSE` | arg0 = fd | 0 / `u64::MAX` | — |
| 72 | `SYS_SOCKETPAIR` | — | `(fd0 << 32) \| fd1`, both in the caller's table / `u64::MAX` | — |
| 73 | `SYS_FD_SEND` | arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to pass or `u64::MAX`, arg4 = flags (1 = do not wait) | bytes written, `0xFFFF_FFFE` if it would have blocked / `u64::MAX` | — |
| 74 | `SYS_FD_RECV` | arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to install a passed descriptor at, `u64::MAX - 1` for any free one, or `u64::MAX` to leave it queued, arg4 = flags (1 = do not wait) | `((fd + 1) << 32) \| bytes`, high half 0 if none arrived, `0xFFFF_FFFE` if it would have blocked / `u64::MAX` | — |
| 75 | `SYS_POLLSET_CREATE` | — | fd naming the set / `u64::MAX` | — |
| 76 | `SYS_POLLSET_CTL` | arg0 = set fd, arg1 = op (0 add, 1 modify, 2 remove), arg2 = fd, arg3 = events, arg4 = token | 0 / `u64::MAX` | — |
| 77 | `SYS_POLLSET_WAIT` | arg0 = set fd, arg1 = array of `(u64 token, u32 events, u32 pad)`, arg2 = capacity, arg3 = timeout in ticks | entries filled, 0 = timed out / `u64::MAX` | — |
| 78 | `SYS_POLL` | arg0 = array of `(u32 fd, u32 events, u32 revents, u32 pad)`, arg1 = count, arg2 = timeout in ticks | entries with non-zero `revents` / `u64::MAX` | — |

**Streams.** `SYS_SOCKETPAIR` makes two connected ends and puts both in the
caller's table; moving one into another task is `SYS_FD_DUP` followed by closing
the caller's copy. An end is reference counted, so that last step does not tell
the peer the connection has gone.

**Passing a descriptor needs no authority over the peer.** `SYS_FD_DUP` puts one
into a task that never asked, so it requires `TaskMgmt` over that task.
`SYS_FD_SEND` hands one to a task that called `SYS_FD_RECV`: the sender chose to
send and the receiver asked to take, and consent on both sides is the whole
authorisation. Memory arriving this way admits the receiver to the region.

**Waiting.** Events are `1` readable, `2` writable, `4` hangup, `8` invalid.
Hangup is reported whether or not it was asked for, because waiting for readable
on a stream whose peer has gone is waiting for something that cannot arrive.
`SYS_POLLSET_CTL` refuses a descriptor that can never become ready — an IPC
endpoint has no buffer — while `SYS_POLL` reports `8` in that entry's `revents`
instead, because one bad entry should not deny the caller the answer about the
others.

`SYS_PIPE_CREATE` needs no capability because a shell needs it for pipelines.
Pipes are reference counted through the descriptors that hold them; a read
returns EOF once the last writer closes, and a pipe is freed when both counts
reach zero.

Note that `SYS_PIPE_FD_SET` **overwrites** the target slot without releasing
what was there. Install pipe ends onto a descriptor a task has not already
inherited, or the previous endpoint's reference is stranded.

### Capabilities (0x50)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 80 | `SYS_CAP_MINT` | arg0 = slot, arg1 = type, arg2 = param0, arg3 = param1 | 0 / `u64::MAX` | must already hold one covering it |
| 81 | `SYS_CAP_GRANT` | arg0 = dest tid, arg1 = src slot, arg2 = dest slot | 0 / `u64::MAX` | — |
| 82 | `SYS_CAP_REVOKE` | arg0 = slot | 0 / `u64::MAX` | must be the minter |
| 83 | `SYS_CAP_INSPECT` | arg0 = slot | packed descriptor | — |
| 84 | `SYS_CAP_DELETE` | arg0 = slot | 0 / `u64::MAX` | — |
| 85 | `SYS_CAP_TRANSFER` | arg0 = dest tid, arg1 = bits | 0 / `u64::MAX` | — |
| 86–90 | *deprecated* | see the deprecation table above | | |

`SYS_CAP_INSPECT` truncates parameters and cannot report a 64-bit destination
set, so an `Endpoint` capability must be **delegated** with `SYS_CAP_GRANT`
rather than read back and re-minted.

Revocation is generation-based; a capability minted into a slot carries that
slot's generation, and revoking bumps it. The counter is 32 bits, so wraparound
cannot resurrect a revoked capability in practice.

### Task lifecycle and identity (0x60)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 96 | `SYS_TASK_CREATE` | — | TID / `u64::MAX` | `TaskMgmt` |
| 97 | `SYS_TASK_START` | arg0 = tid, arg1 = rip, arg2 = rsp, arg3 = cr3 | 0 / `u64::MAX` | `TaskMgmt` |
| 98 | `SYS_GET_UID` | — | current UID | — |
| 99 | `SYS_SET_UID` | arg0 = tid, arg1 = uid | 0 / `u64::MAX` | `SetUid` |
| 100 | `SYS_SET_GID` | arg0 = tid, arg1 = gid | 0 / `u64::MAX` | `SetUid` |
| 101 | `SYS_GET_TUID` | arg0 = tid | that task's UID | — |
| 104 | `SYS_TASK_WATCH` | arg0 = tid | 0, or `u64::MAX` if that task is already gone | — |
| 106 | `SYS_SET_CLEAR_TID` | arg0 = address of a `u32`, or 0 | this task's id / `u64::MAX` | — |
| 105 | `SYS_TASK_PRIORITY` | arg0 = tid, arg1 = band | 0 / `u64::MAX` | `TaskMgmt` for target, and the caller's own band or worse |

There is no fork or exec. A parent creates a task, builds its address space,
loads its image, sets its arguments and capabilities, then starts it. TIDs are
reused once a task is reaped, so a TID identifies a task only for as long as
that task lives — see the note on `Endpoint` revocation below.

`SYS_GET_TUID` exists for servers doing permission checks on behalf of a
caller: the VFS uses it to evaluate file modes against the requester.

`SYS_TASK_WATCH` asks to be told when a task dies. The notification arrives at
the watcher's next `SYS_RECV` as a message from the kernel — sender 0, tag
`0xFFFF_0003`, `data[0]` the TID that died — and is delivered when the task is
marked dead rather than when it is reaped, since reaping waits on a parent that
may never call `SYS_WAIT`.

It takes no capability. `SYS_TASK_INFO` already tells anybody whether a given
task is alive, so a watch discloses nothing new; what it removes is the polling,
and the window between polls in which a server still believes a dead task holds
what it lent out. Failure means the task was already gone, which is an answer:
the caller may reclaim immediately.

Registrations are dropped when either task dies, so a watcher is never told
about the next occupant of a recycled TID. A watcher that lets eight
notifications go uncollected loses the ninth; a server whose whole job is to
reclaim on death should not be one of them.

`SYS_TASK_PRIORITY` puts a task in a scheduling band: 0 drivers, 1 servers,
2 ordinary programs, 3 the idle task. A task runs only when nothing in a better
band is waiting, and takes turns within its own; a task woken into a better band
than the running one preempts it at the next tick rather than waiting out its
slice.

A task also runs in the better of its own band and the band of anything blocked
waiting on it, for as long as that is true. Bands otherwise introduce the
problem they are famous for: a server in an ordinary band, called by something
in a better one, is preempted by any middling task that comes along, and the
caller — which outranks that task — waits behind it. The work is being done on
the caller's behalf, so it is done at the caller's urgency, and the loan is
returned when the caller stops waiting, whether that is a reply, a timeout, or
the caller dying.

It follows the same narrowing rule as capabilities — a caller cannot grant a
better band than it is in itself — so a shell running as an ordinary program
cannot promote what it starts. Programs ask for a band in their manifest, and
only a spawner already in that band can satisfy the request. `init` starts in
the driver band for exactly that reason and steps down to an ordinary one once
it has finished starting things.

**A driver that spins is a driver that starves the system.** Bands make a
`sys_yield` loop fatal rather than merely wasteful: a task in a better band that
yields is immediately runnable again, so nothing below it ever runs. Wait by
blocking — `sys_recv_timeout`, or `sleep_ticks`, which is that call with nobody
to hear from.

### Hardware and drivers (0x70)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 112 | `SYS_IRQ_REGISTER` | arg0 = irq | 0 / `u64::MAX` | `Irq` for that line |
| 113 | `SYS_IRQ_ACK` | arg0 = irq | 0 / `u64::MAX` | `Irq` for that line |
| 114 | `SYS_IOPORT` | arg0 = port, arg1 = op, arg2 = value | read value, or 0 / `u64::MAX` | `IoPort` covering the port |
| 115 | `SYS_IOPORT_REP` | arg0 = port, arg1 = buf, arg2 = words, arg3 = op | 0 / `u64::MAX` | `IoPort` covering the port |

`SYS_IOPORT` ops: 0 = read8, 1 = write8, 2 = read16, 3 = write16, 4 = read32,
5 = write32. `SYS_IOPORT_REP` ops: 0 = `rep insw`, 1 = `rep outsw`.

Interrupts are delivered to the registered task as notifications through a
per-IRQ ring buffer, polled in `SYS_RECV`.

### Synchronisation (0x80)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 128 | `SYS_FUTEX_WAIT` | arg0 = addr (4-byte aligned), arg1 = expected | 0 = woken, 1 = value already differed, `u64::MAX` = bad address or no wait slot. **Blocks.** | — |
| 129 | `SYS_FUTEX_WAKE` | arg0 = addr, arg1 = max to wake | number woken | — |
| 130 | `SYS_FUTEX_WAIT_TIMEOUT` | arg0 = addr, arg1 = expected, arg2 = ticks | 0 = woken, 1 = value already differed, 2 = timed out, `u64::MAX` = bad address or no wait slot. **Blocks.** | — |

Futexes are keyed on **physical** address, so a word in shared memory is one
futex to every task that maps it, whatever virtual address each uses.

A timeout of 0 ticks makes `SYS_FUTEX_WAIT_TIMEOUT` a check rather than a wait:
it returns 1 if the value already differs and 2 if it does not, without
blocking. There is no way to ask for an unbounded wait through this call; that
is what `SYS_FUTEX_WAIT` is.

A timed wait that expires leaves its wait slot claimed until the woken task
returns through the kernel and reads why it woke, so a task blocked on a futex
holds its slot from the moment it waits to the moment it runs again. With
`MAX_FUTEX_WAITERS` slots in total, a caller that gets `u64::MAX` should treat
it as a resource limit rather than as a bad argument.

### Time (0x90)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 144 | `SYS_TICKS` | — | ticks since boot | — |

The PIT runs at 100 Hz, so one tick is 10 ms. Every timeout argument in this
ABI is in ticks.

### Sockets (0xB0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 176 | `SYS_SOCK_FD` | arg0 = net server tid, arg1 = connection handle | the new fd / `u64::MAX` | Endpoint to the net server |
| 177 | `SYS_SOCK_INFO` | arg0 = fd | `(net_tid << 32) \| handle` / `u64::MAX` | — |

A socket is a connection the net server already holds, bound to a descriptor in
the calling task's fd table. `SYS_FD_READ` and `SYS_FD_WRITE` on that descriptor
carry data to and from the server, in the same chunked-IPC form they use for a
service — so a socket is read and written by code that has no idea it is one.

The handle is **not** checked here. Connections belong to the task that opened
them, and the net server refuses one named by anybody else; the kernel stamps
the sender on every message, so it cannot be forged. What is checked is the
right to talk to that server at all, once, at bind time, rather than on every
read and write.

The lowest free descriptor from 3 upwards is used. 0, 1 and 2 are stdio by
convention even when unset, and handing one out would silently redirect a
program's output into a socket.

Closing is not a kernel operation: the connection is the net server's, and
`quark_rt::socket` closes it through `TAG_TCP_CLOSE` when the stream is
dropped. `SYS_SOCK_INFO` exists so a program holding only a descriptor can
recover what to close.

**Throughput.** The fd path carries 40 bytes per message, so a socket does a
round trip per 40 bytes rather than per page. That is the cost of going through
the same path as every other descriptor. It is a data-path change to fix and
not an interface one; the page-based calls the net server has always had remain
for a caller that needs the bandwidth.

### Kernel debug console (0xA0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 160 | `SYS_WRITE` | arg0 = buf, arg1 = len | bytes written / `u64::MAX` | — |
| 161 | `SYS_CONSOLE_POS` | — | packed `(row, col)` | — |

These write to the kernel's own console, bypassing the user-space console
server. They exist for bring-up and for output before a console exists.
**Expect them to be withdrawn** once early output is handled another way;
ordinary programs should use file descriptor 1.

### ABI introspection (0xF0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 240 | `SYS_ABI_VERSION` | — | `(major << 16) \| minor` | — |

## User pointers

Every pointer a caller passes is validated as **mapped**, not merely in range:
the kernel runs on the caller's CR3, so an in-range but unmapped address would
fault inside the kernel, possibly with a lock held and interrupts off. Reads and
writes are checked separately, and a null pointer is rejected.

SMEP and SMAP are enabled when the CPU reports them. The kernel touches user
memory only inside a narrow guard that sets RFLAGS.AC, and never holds that
guard across a blocking operation, so a syscall that blocks mid-copy cannot
leave the window open for whatever runs next.

## Notes for implementers

**TID reuse.** Reaping returns a task slot to the pool, so TIDs are recycled.
Anything that names a task by number must cope with the name changing meaning:
the kernel clears a dead TID's bit from every `Endpoint` capability for exactly
this reason. Do not cache a TID across the lifetime of the task it named.

**No threads yet.** `x86_64-unknown-quark` sets `singlethread: true`. A libc or
runtime port should expect a single thread of execution per address space until
the ABI gains a thread block.

**Services are found by name.** The nameserver is at a well-known TID and maps
names to TIDs (`TAG_LOOKUP`), and back (`TAG_LOOKUP_TID`). Every IPC server also
answers `TAG_PING` with an empty reply, which is the portable way to ask whether
one is alive. Note that not every service is an IPC server — the console is
driven by a pipe and does not answer.
