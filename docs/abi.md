# Quark syscall ABI

**Version 2.8.** Query the running kernel with `SYS_ABI_VERSION` (240), which
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
| 0xC0  | 192–207   | Memory, continued             |
| 0xF0  | 240–255   | ABI introspection             |

Blocks 0xD0–0xE0 are unassigned and available for new subsystems. Threads
never needed one: a thread is a task started with its creator's address space,
so it is built from calls that already existed.

## Stability and deprecation

- **Numbers are never reused.** A withdrawn call leaves its slot empty. A
  program built against an older minor version that calls a withdrawn number
  gets a clean failure rather than a different call's behaviour. Capability
  type numbers are kept the same way.
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
| 1.9 | `SYS_MEMFD_TRUNCATE` (54) — `ftruncate` on memory named by a descriptor, which is how every Wayland client makes its buffer pool: `memfd_create`, `ftruncate`, `mmap`. Only while nobody has the region mapped and no descriptor for it is travelling, because growing it otherwise changes what is behind somebody else's live mapping. `SYS_MMAP_FD` (42) also returns the size it mapped instead of 0: a task that received a descriptor has no other way to learn how big the memory is, and the sender's word for it is the one thing it must not take. |
| 1.10 | `SYS_FD_DUP` (68) and `SYS_PIPE_FD_SET` (70) no longer need `TaskMgmt` when the target is the caller, and both accept `u64::MAX - 1` for "any free descriptor" and return the number they took. Putting a descriptor into somebody else's table hands them something they never asked for; putting one into your own is `dup` and `pipe`, which every C library calls and which confer nothing. Also: `sys_cap_grant` (81) requires `TaskMgmt` over the destination, or the destination's consent — either a `sys_call` to the granter in progress, or a standing `Endpoint` naming it. Unrestricted, any task could fill any other's sixteen CSpace slots and leave it unable to be handed a display again. |
| 1.11 | `SYS_ADDRSPACE_GIVE` (43) — move pages of the caller's own memory into an address space it made, which owns them from then on and frees them with itself. `SYS_ADDRSPACE_MAP` (37) is deprecated in its favour: every program a spawner loaded with it stayed allocated until the spawner exited. Also: `SYS_WAIT` (4) reaps the child it returns, so its memory is free, and its TID may be reused, by the time the call returns. A child used to be reaped only when the machine next went idle, which a parent running programs back to back never let it do. |
| 1.12 | `SYS_CALL_LEND` (23), `SYS_LENT_READ` (25), `SYS_LENT_WRITE` (26) — a call can lend the task it calls a buffer, which that task copies into and out of through the kernel until it replies. `SYS_CAP_READ` (92) — read one slot of a task's CSpace whole, for any task the caller manages. Also: the `CAP_MAP_PHYS` bit, per task or per user, confers nothing, and init starts with `PhysRange` capabilities over the framebuffer and its boot modules instead of all of memory. |
| 1.13 | Capability type 8, `Endpoint`: permission to call one task, recorded as the number of that task's endpoint, which no other task will ever have. `SYS_CALL_OFFER` (24) — a call can offer the task it calls a copy of one capability — and `SYS_CAP_TAKE` (91), which takes it. `SYS_CAP_GRANT` (81) accepts `u64::MAX - 1` for "any slot" and returns the slot it used. A CSpace has 64 slots instead of 16. Type 7, the TID set, is renamed `EndpointSet` and deprecated. |

A capability may only be minted from one the caller already holds, and only
narrowed — except an `Endpoint`, which is minted on ownership: **by the task it
names, the task that created that one, or a task already holding one for it.**
Admitting others to call you confers no authority over anybody else, and
neither does a parent admitting others to call its child.

### What 2.0 changed

Capability type 7, a set of destination task IDs, is **withdrawn**:
`SYS_CAP_MINT` refuses it and nothing grants one. The `CAP_ENDPOINT` bit, per
task or per user, confers nothing; it expanded into a set naming every task.
Two things went with the sets: the rule that any task could mint one naming
only itself, and the kernel clearing a dead task's bit from every CSpace
before its ID could be used again. An `Endpoint` (type 8) is now the only
permission to originate IPC. Nothing else changed from 1.13, so a program that
never used type 7 runs as before.

**An exception to the deprecation rule.** Type 7 was deprecated at 1.13 and
withdrawn at 2.0, without the full major version in between. Keeping it would
have kept authority named by task ID in the kernel — the sweep, the special
case, and a capability that outlived the task it named — and removing those is
the reason for the change. Nothing outside this tree was built against 1.x. The
rule still holds for everything else.

### What each minor of 2 added

| Version | Added |
|---|---|
| 2.1 | `SYS_BOOT_TIME` (145) — the date, read from the machine's clock at boot. Before it nothing here knew what day it was, and files were dated from 1970. |
| 2.2 | `SYS_TASK_SPACE` (107) and `SYS_SPACE_WATCH` (108) — a program's identity is its address space, so a server can keep what a program holds for the program rather than for the one thread that asked. Also: a thread starts holding a copy of its creator's capabilities and descriptors, and in its band. |
| 2.3 | `SYS_GETRANDOM` (116) — random bytes from a ChaCha20 generator seeded from RDSEED or RDRAND and the machine's timing. Before it a program had the clock, and expat salted its hash tables with it. |
| 2.4 | `SYS_TASK_CREATE_IN` (109) — a task made for an address space the caller created belongs to that program before it runs, so a spawner can hand it things servers keep per program, such as a working directory. `SYS_TASK_SPACE` answers for it at once. |
| 2.5 | Block 0xC0 opens. `SYS_MAP_ANON` (192) — memory reserved and given its frames when first touched, up to 512 GiB at once; `SYS_MEM_INFO` (193) — free frames, and what the caller is charged. Also: a user page fault on a reserved page is served rather than fatal, and one that cannot be served ends the task with SIGBUS. |
| 2.6 | `SYS_OBJECT_CREATE` (194), `SYS_OBJECT_MAP` (195), `SYS_OBJECT_CTL` (196) and capability type 9, `MemObject` — memory objects whose pages a user-space pager provides as they are touched, which is how a file is mapped. Also: a task's page fault can call a pager (`TAG_PAGE_IN`, sender marked with bit 62), a pager hears when nothing maps an object (`TAG_OBJECT_IDLE`), and notices from the kernel are received before calls waiting behind them. |
| 2.7 | `SYS_OBJECT_SYNC` (197) — what was written through shared mappings reaches the files (`TAG_OBJECT_SYNC` to each pager). Also: `SYS_OBJECT_CTL` op 3 takes a starting page, and leaves a page dirty while it is mapped writable. |
| 2.8 | `SYS_CALL_WITH` (27) — a call with any of a buffer lent, a capability offered and a deadline. |
| 2.9 | What a process is, and what it runs in. Block 0xD0 opens: `SYS_PTY_CREATE` (208) and `SYS_PTY_CTL` (209) — a pseudo-terminal pair as two ordinary descriptors, with a line discipline (echo, canonical input, newline translation), a `termios` and a window size. `SYS_FORK` (110) — a copy of the caller in a copy of its address space, which returns 0 there. `SYS_EXEC_SPACE` (111) — the caller becomes the program in an address space it built, keeping its id, its descriptors and its capabilities. `SYS_ADDRSPACE_DESTROY` (38) — throw away an address space nothing is running in, which a spawn or an exec that failed part-way had no way to do. Also: `SYS_ADDRSPACE_CREATE` (36) and `SYS_ADDRSPACE_GIVE` (43) no longer ask for `TaskMgmt` — an address space the caller made, filled with pages it already owned, confers authority over nothing, and *starting a task* in one still does ask. |

### Deprecated

Five calls are **deprecated as of 1.0** — the `CAP_*` object-capability calls
(80–85) replace them:

| Call | Number | Why | Replacement |
|---|---|---|---|
| `SYS_GRANT_CAP` | 86 | Expands a bitmask bit into a *wildcard* capability, e.g. `CAP_IOPORT` becomes every port, silently widening any narrow grant made alongside it. (`CAP_MAP_PHYS` expands to nothing as of 1.12, and `CAP_ENDPOINT` as of 2.0.) | `SYS_CAP_MINT` + `SYS_CAP_GRANT` |
| `SYS_GRANT_IOPORT` | 87 | Same, for the full port range | `SYS_CAP_MINT` with `IoPort(start, end)` |
| `SYS_GRANT_IRQ` | 88 | Same shape | `SYS_CAP_MINT` with `Irq(n)` |
| `SYS_SET_USER_CAPS` | 89 | Per-UID authority predates capabilities and is not consulted by anything that grants correctly | per-task CSpace |
| `SYS_GET_USER_CAPS` | 90 | as above | per-task CSpace |

New code must not call these. Each turns a bit into the widest capability of
its kind, which undoes any narrow grant made alongside it; `CAP_MAP_PHYS`, the
worst of them, has conferred nothing since 1.12.

`SYS_ADDRSPACE_MAP` (37) is **deprecated as of 1.11**, replaced by
`SYS_ADDRSPACE_GIVE` (43). The frames it maps into a child stay the spawner's,
which is wrong both ways: they outlive the child, and die with the spawner.

Capability type 7, deprecated at 1.13, was withdrawn at 2.0; see *What 2.0
changed*.

## Capabilities

Authority comes from capabilities, not from a privilege level — there is no
UID 0 bypass in the kernel. Each task has a CSpace of 64 slots holding
`CapSlot { cap_type, generation, root_slot, root_tid, param0, param1 }`.

| # | Type | param0 | param1 |
|---|---|---|---|
| 1 | `IoPort` | first port | last port |
| 2 | `PhysRange` | first address | last address (page aligned) |
| 3 | `Irq` | IRQ number (`0xFF` = any) | — |
| 4 | `TaskMgmt` | target TID (`0` = any) | — |
| 5 | `PhysAlloc` | max pages (`0` = unlimited) | — |
| 6 | `SetUid` | — | — |
| 7 | *withdrawn at 2.0* | — | — |
| 8 | `Endpoint` | the destination's endpoint number | — |

Delegation may narrow a capability but never widen it; delegating at equal
breadth is allowed, since a set is a subset of itself.

**Endpoints.** Every task has an endpoint, with a number the kernel never gives
to anything else, even once the task is gone. An `Endpoint` capability records
that number, so it permits calling the task it was minted for and nothing that
later has the same TID. `SYS_CAP_MINT` takes the destination's *TID* as param0
and stores its number; `SYS_CAP_READ` shows the number. Numbers start at 64, so
one cannot be mistaken for a TID. IPC still names its destination by TID: the
number is what the check compares.

**Slots.** Slots 0–15 are for what spawners and manifests place deliberately.
A capability given without naming a slot — `SYS_CAP_GRANT` or `SYS_CAP_TAKE`
with `u64::MAX - 1` — lands in the first empty slot from 16 up, and the call
returns which. An `Endpoint` the receiver already holds, in any slot, is not
copied again: the call returns the slot it is in, so asking for the same
service twice costs nothing.

## The calls

`Cap` names the capability required. `—` means none. Blocking calls are marked;
everything else returns promptly.

### Process lifecycle (0x00)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 0 | `SYS_EXIT` | — | does not return | — |
| 1 | `SYS_EXIT_CODE` | arg0 = status, of which the low eight bits are kept | does not return | — |
| 2 | `SYS_YIELD` | — | 0 | — |
| 3 | `SYS_GETPID` | — | current TID | — |
| 4 | `SYS_WAIT` | — | `tid \| (exit_code << 32)`, or `u64::MAX` if no children. **Blocks.** | — |
| 5 | `SYS_TASK_KILL` | arg0 = tid | 0 / `u64::MAX` | `TaskMgmt` for target, or same UID |
| 6 | `SYS_SIGNAL` | arg0 = tid, arg1 = signal bits | 0 / `u64::MAX` | as above |
| 7 | `SYS_TASK_INFO` | arg0 = tid | packed info, or `u64::MAX` | — |

A status is kept as its low eight bits, as Linux's wait status keeps it. A
negative status is how the kernel says a task was killed — `SYS_TASK_KILL`
reports -9, a fault the negated signal — so a task cannot set one and claim it
was.

`SYS_EXIT` is equivalent to `SYS_EXIT_CODE(0)`; it exists separately because
`syscall0` leaves RDI undefined, so the original call could not grow an
argument.

**A negative exit code means the kernel killed the task.** Its magnitude is the
signal Linux sends for the exception that did it: 4 for an invalid opcode, 5
for a debug trap or breakpoint, 7 for an alignment check, 8 for a divide error
or floating-point exception, and 11 for everything else — a page fault with no
pager, a general protection fault, a stack fault. `SYS_WAIT` reports it as it
reports any other status. A killed task never halts the machine: only a fault
taken in ring 0 does that. `SYS_TASK_KILL` reports -9, as SIGKILL would.

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
| 23 | `SYS_CALL_LEND` | arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = buffer, arg4 = length \| access bits | as `SYS_CALL` | `Endpoint` for dest |
| 24 | `SYS_CALL_OFFER` | arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = slot | as `SYS_CALL`; `u64::MAX` without calling if the slot holds no valid capability | `Endpoint` for dest |
| 25 | `SYS_LENT_READ` | arg0 = caller, arg1 = offset, arg2 = buffer out, arg3 = length | bytes copied / `u64::MAX` | the caller's call is being served |
| 26 | `SYS_LENT_WRITE` | arg0 = caller, arg1 = offset, arg2 = buffer, arg3 = length | bytes copied / `u64::MAX` | as above |
| 27 | `SYS_CALL_WITH` | arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = `CallWith` | as `SYS_CALL_TIMEOUT`; `u64::MAX` without calling if a part is refused | `Endpoint` for dest |

The `Endpoint` check applies to calls where the sender names its own
destination. IPC the kernel performs on a task's behalf through an installed
file descriptor bypasses it deliberately: the fd is itself the authorisation,
and only a `TaskMgmt` holder can install one.

`SYS_NOTIFY` rejects the reserved signal bits; those may only be raised through
`SYS_SIGNAL`, which checks the caller's authority over the target.

**Lending memory with a call.** `SYS_CALL_LEND` is `SYS_CALL` with a buffer the
task called may use until it replies: read it with `SYS_LENT_READ`, write it
with `SYS_LENT_WRITE`. Bit 62 of arg4 lends it for reading, bit 63 for writing,
and the bits below them are the length, at most 16 MiB. A read or write names
the caller whose call it is serving and an offset inside what was lent, copies
at most 1 MiB, and works only between receiving that call and answering it —
before, the call has not been accepted; after, it is over. The kernel does the
copying, so the server never learns where the memory is: the buffer is checked
when the call is made and again, page by page, as it is copied, since a thread
sharing the caller's address space may have changed it in between.

This is how a server fills or reads a client's buffer without mapping it, and
so without any authority over physical memory. It replaces requests that named
a physical page for the server to map.

**Offering a capability with a call.** `SYS_CALL_OFFER` is `SYS_CALL` with one
slot of the caller's CSpace on offer. The task called may copy it with
`SYS_CAP_TAKE` between receiving the call and answering it, once; if it does
not, nothing happens. This is how a client hands a server the right to call it
back — a registration, a claim on the display — without the server's CSpace
being open to anybody who wants to fill it. The copy is derived as a grant
would derive it, so revoking the original revokes it too.

**All of them at once.** `SYS_CALL_WITH` takes a pointer to four words:
the buffer's address, its length with the lending bits as `SYS_CALL_LEND`'s
arg4 has them (0 lends nothing), the slot to offer (`u64::MAX` for none), and
the ticks to wait for the reply (0 for ever). Each part is checked as the call
with only that part checks it, and the result is `SYS_CALL_TIMEOUT`'s. It is
what a caller that cannot trust the task it calls uses to lend or offer with a
deadline.

Prefer `SYS_CALL_TIMEOUT` over `SYS_CALL` for any destination not known to be a
running server. A TID is not a promise that anything is listening, and a plain
call to a task that never reaches `SYS_RECV` blocks forever.

### Memory (0x20)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 32 | `SYS_MMAP` | arg0 = vaddr, arg1 = pages | 0 / `u64::MAX` | — (quota enforced) |
| 42 | `SYS_MMAP_FD` | arg0 = fd naming memory, arg1 = vaddr | bytes mapped / `u64::MAX` | — |
| 33 | `SYS_MUNMAP` | arg0 = vaddr, arg1 = pages | 0 / `u64::MAX` | — |
| 34 | `SYS_PHYS_ALLOC` | arg0 = pages | physical address / `u64::MAX` | `PhysAlloc` |
| 35 | `SYS_PHYS_FREE` | arg0 = phys, arg1 = count | 0 / `u64::MAX` | `PhysAlloc` + frame ownership |
| 36 | `SYS_ADDRSPACE_CREATE` | — | CR3 / `u64::MAX` | — |
| 38 | `SYS_ADDRSPACE_DESTROY` | arg0 = cr3 the caller made, with no task in it | 0 / `u64::MAX` | — |
| 37 | `SYS_ADDRSPACE_MAP` | arg0 = cr3, arg1 = virt, arg2 = phys, arg3 = pages, arg4 = flags | 0 / `u64::MAX` | `TaskMgmt` + frame ownership or `PhysRange` — **deprecated** |
| 43 | `SYS_ADDRSPACE_GIVE` | arg0 = cr3, arg1 = virt there, arg2 = virt here, arg3 = pages (at most 256), arg4 = flags (bit 0: writable) | 0 / `u64::MAX` | an address space the caller made, or runs in, and the pages are its own |
| 38 | `SYS_MAP_PHYS` | arg0 = phys, arg1 = virt, arg2 = pages | 0 / `u64::MAX` | frame ownership or `PhysRange` |
| 39 | `SYS_SET_MEM_LIMIT` | arg0 = tid, arg1 = pages (0 = unlimited) | 0 / `u64::MAX` | `TaskMgmt` |
| 40 | `SYS_SET_PAGER` | arg0 = tid, arg1 = pager tid | 0 / `u64::MAX` | `TaskMgmt` |

**Mapping authority is ownership first, `PhysRange` second.** A task may map
frames it owns — `SYS_PHYS_ALLOC` records the caller as owner — with no
capability at all, since handing back memory the allocator just gave you conveys
no new authority. `PhysRange` is required only for frames the allocator never
owned (device MMIO, the framebuffer, a boot module). Data a server reads or
writes for a client is lent with the call, not mapped (see `SYS_CALL_LEND`), so
no server needs a range for that — and none is given one.

**All user mappings must be at or above `USER_MIN_ADDR` (0x80_0000_0000).**
Lower addresses are rejected: address spaces share the page directories beneath
PML4[0], so a low mapping would write into tables every address space shares.

**Giving memory moves it.** `SYS_ADDRSPACE_GIVE` takes pages the caller got
from `SYS_MMAP` and moves them into an address space the caller created. They
leave the caller and belong to the target, which frees them when it is
destroyed; the caller is no longer charged for them. Anything else is refused —
device memory, shared memory, a frame from `SYS_PHYS_ALLOC` — and so is a page
already mapped at the far end, or the caller's own address space as the target.
Everything is checked before anything moves, but a failure part way (for want
of a page table) leaves the pages before it moved. This is how a spawner loads
a program.

`SYS_ADDRSPACE_MAP` lends frames instead. They stay the caller's, so they are
freed when the *caller* exits, even under a child still running on them, and
never when the child does.

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
| 54 | `SYS_MEMFD_TRUNCATE` | arg0 = fd naming memory, arg1 = size in bytes | bytes the region became / `u64::MAX` | — (charged to creator's quota) |

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
| 68 | `SYS_FD_DUP` | arg0 = target tid, arg1 = target fd or `u64::MAX - 1` for any free one, arg2 = source fd, arg3 = lowest acceptable fd when arg1 asks for any | the fd it took / `u64::MAX` | `TaskMgmt` over the target, unless the target is the caller |
| 69 | `SYS_PIPE_CREATE` | — | handle / `u64::MAX` | — (bounded per task) |
| 70 | `SYS_PIPE_FD_SET` | arg0 = target tid, arg1 = fd or `u64::MAX - 1` for any free one, arg2 = pipe handle, arg3 = 1 for write end | the fd it took / `u64::MAX` | `TaskMgmt` over the target, unless the target is the caller |
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

**Copying onto a descriptor closes it first**, as `dup2` does: `SYS_FD_DUP`,
`SYS_FD_SET` and `SYS_PIPE_FD_SET` release whatever the target slot named, and
copying a descriptor onto itself changes nothing. A poll set cannot be copied or
sent at all: it counts no holders, so it has exactly one.

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
| 80 | `SYS_CAP_MINT` | arg0 = slot, arg1 = type, arg2 = param0, arg3 = param1 | 0 / `u64::MAX` | must already hold one covering it; for an `Endpoint`, param0 is the destination's TID and the rule is ownership (above) |
| 81 | `SYS_CAP_GRANT` | arg0 = dest tid, arg1 = src slot, arg2 = dest slot or `u64::MAX - 1` for any | 0, or the slot used when any; `u64::MAX` on failure | `TaskMgmt` over dest, or its consent |
| 82 | `SYS_CAP_REVOKE` | arg0 = slot | 0 / `u64::MAX` | must be the minter |
| 83 | `SYS_CAP_INSPECT` | arg0 = slot | packed descriptor | — |
| 84 | `SYS_CAP_DELETE` | arg0 = slot | 0 / `u64::MAX` | — |
| 85 | `SYS_CAP_TRANSFER` | arg0 = dest tid, arg1 = bits | 0 / `u64::MAX` | — |
| 91 | `SYS_CAP_TAKE` | arg0 = caller, arg1 = slot or `u64::MAX - 1` for any | the slot used / `u64::MAX` | caller's `SYS_CALL_OFFER` to this task is being served |
| 92 | `SYS_CAP_READ` | arg0 = tid, arg1 = slot, arg2 = out: four `u64`s — type, param0, param1, valid (1/0) | 0 / `u64::MAX` past the last slot | `TaskMgmt` over tid, unless tid is the caller |
| 86–90 | *deprecated* | see the deprecation table above | | |

`SYS_CAP_INSPECT` truncates parameters to sixteen bits, so it cannot report an
`Endpoint`'s number. Delegate one with `SYS_CAP_GRANT`, or read it with
`SYS_CAP_READ`.

`SYS_CAP_READ` reports a slot whole, and for any task the caller manages. It
exists so that "no task may map more than its device" is something a test can
check rather than something to believe: `SYS_CAP_INSPECT` can show neither a
physical range nor anybody else's CSpace. An empty slot reads as type 0.

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
| 107 | `SYS_TASK_SPACE` | arg0 = tid | that task's space id / `u64::MAX` | — |
| 108 | `SYS_SPACE_WATCH` | arg0 = space id | 0, or `u64::MAX` if no task of it is alive | — |
| 109 | `SYS_TASK_CREATE_IN` | arg0 = cr3 of an address space the caller created | TID / `u64::MAX` | `TaskMgmt` |
| 110 | `SYS_FORK` | — | the child's TID, `0` in the child / `u64::MAX` | — |
| 111 | `SYS_EXEC_SPACE` | arg0 = cr3 the caller made, arg1 = entry, arg2 = rsp | does not return / `u64::MAX` | — |
| 105 | `SYS_TASK_PRIORITY` | arg0 = tid, arg1 = band | 0 / `u64::MAX` | `TaskMgmt` for target, and the caller's own band or worse |

There is no fork or exec. A parent creates a task, builds its address space,
loads its image, sets its arguments and capabilities, then starts it. TIDs are
reused once a task is reaped, so a TID identifies a task only for as long as
that task lives — see the note on TID reuse below.

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

A program is its address space. `SYS_TASK_SPACE` names it: every address
space gets an id when it is made, counting up from 1 for as long as the machine
runs and never given out again, and every thread of a program answers with the
same one. A server that keeps something for a client — an open file, a lock, a
working directory — keeps it for the space, so any thread of the program may
use it. `SYS_SPACE_WATCH` is `SYS_TASK_WATCH` for a program: the notice is sender
0, tag `0xFFFF_0004`, `data[0]` the space id, and it comes once, when the last
live task of that space dies. Failure means none is alive.

A task made with `SYS_TASK_CREATE_IN` belongs to that address space's program
from the moment it exists: `SYS_TASK_SPACE` names it, a watch on the program
counts it, and `SYS_TASK_START` refuses to start it anywhere else.

A task started with `SYS_TASK_START` in its creator's own address space is a
thread of it, and starts with a copy of what its creator holds at that moment:
each capability in a slot the creator did not already fill for it, each
descriptor likewise (as `SYS_FD_DUP` would copy it; poll sets and sockets stay
behind, since neither counts its holders), and the creator's band. A copy, not
a share: what either is given or gives up afterwards, the other does not see.

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
| 116 | `SYS_GETRANDOM` | arg0 = buf, arg1 = len, arg2 = flags (none yet) | bytes written, at most 1 MiB / `u64::MAX` | — |

`SYS_IOPORT` ops: 0 = read8, 1 = write8, 2 = read16, 3 = write16, 4 = read32,
5 = write32. `SYS_IOPORT_REP` ops: 0 = `rep insw`, 1 = `rep outsw`.

Interrupts are delivered to the registered task as notifications through a
per-IRQ ring buffer, polled in `SYS_RECV`.

`SYS_GETRANDOM` never blocks and needs nothing. The generator is ChaCha20
(RFC 8439) with fast key erasure: each call's first block replaces the key
before anything is handed out, so a key read out of memory later recomputes
nothing given out before it. It is seeded at boot from RDSEED, else RDRAND,
with the TSC, the timer and the clock, and every timer tick folds the TSC into
what the next call mixes in. A CPU with neither instruction is seeded from
timing alone, which is guessable, and the kernel says so on the serial line
(`[random] no RDRAND or RDSEED; seeded from timing`). The kernel checks the
block function against the RFC's test vector at boot and will not start if it
is wrong.

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
| 145 | `SYS_BOOT_TIME` | — | seconds since 1970 when tick 0 was counted; 0 if the machine has no clock | — |

The PIT runs at 100 Hz, so one tick is 10 ms. Every timeout argument in this
ABI is in ticks. The time of day is `SYS_BOOT_TIME + SYS_TICKS / 100`: the
kernel reads the PC's battery-backed clock once, at boot, and never again.

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

### Memory, continued (0xC0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 192 | `SYS_MAP_ANON` | arg0 = address, arg1 = pages (at most 2^27), arg2 = flags (1 = back every page now, 2 = no more pages than the machine has) | 0 / `u64::MAX` | — |
| 193 | `SYS_MEM_INFO` | — | `(free frames << 32) \| pages charged to the caller` | — |
| 194 | `SYS_OBJECT_CREATE` | arg0 = cookie, arg1 = bytes, arg2 = slot | object id / `u64::MAX` | — |
| 195 | `SYS_OBJECT_MAP` | arg0 = slot, arg1 = address, arg2 = pages, arg3 = first page, arg4 = flags (1 write, 2 shared, 4 exec) | 0 / `u64::MAX` | `MemObject`: read; write too for a shared writable mapping |
| 196 | `SYS_OBJECT_CTL` | arg0 = object id, arg1 = op, arg2, arg3 | per op / `u64::MAX` | the object's pager |
| 197 | `SYS_OBJECT_SYNC` | arg0 = address, arg1 = pages | 0 / `u64::MAX` if a pager failed | — |

`SYS_MAP_ANON` reserves memory without giving it any: each page gets a zeroed
frame, charged to the task that touches it, the first time it is read or
written — by the task, or by the kernel copying to or from it for a call. The
range must hold nothing, as for `SYS_MMAP`, and a reserved page counts as
something to `SYS_MMAP` and to another `SYS_MAP_ANON`. A reservation of 2 MiB
or more costs a page-directory entry, not a page table, until it is touched.
`SYS_MUNMAP` removes reservations and mappings alike. `SYS_MMAP` is unchanged:
it gives its frames at once, for the servers that count on it.

A page promised and not there to give — no free frame, or the task's limit
(`SYS_SET_MEM_LIMIT`) reached — ends the task that touched it with SIGBUS
(`-7`), and says `[OOM tid=N]` on the serial line: Linux's overcommit bargain,
and its answer. With arg2 bit 0 the whole range is backed before the call
returns, and the call fails instead. With bit 1 a reservation of more pages
than the machine has frames is refused outright — Linux's overcommit
heuristic, which the C library applies to every mapping without
`MAP_NORESERVE`, so that a `calloc` nothing could hold returns NULL rather
than a region that ends the program when it is read.

**Memory objects.** An object is a run of pages a user-space pager provides.
`SYS_OBJECT_CREATE` makes one of `bytes` bytes, with the caller as its pager
and `cookie` as the pager's own name for it, and puts a read-write `MemObject`
capability (type 9: `param0` the object id, never reused; `param1` the access,
1 read and 2 write) in `slot`. The pager mints narrower copies with
`SYS_CAP_MINT` — the same id, no access its own lacks — and grants them.

`SYS_OBJECT_MAP` reserves `pages` pages at `address` for pages `first..` of
the object; the range must be free. Each page is fetched when first touched:
from the object's cache if it is there, and otherwise from the pager, which
the touching task calls itself. The pager receives `TAG_PAGE_IN`
(`0xFFFF_0005`) with `sender` the task's TID with bit 62 (`PAGER_BIT`) set —
nobody else can set it — and `data` = `[cookie, page, object id]`, and a
fresh 4096-byte frame lent for writing. It fills the frame with
`SYS_LENT_WRITE` and replies to `sender` as given (0 to deliver the page, an
error to make the fault SIGBUS). The page joins the object's cache. A shared
mapping, and a read-only one, maps the cached frame itself; a private writable
one gets a copy of its own, charged to it. A page past the object's end is
SIGBUS, as on Linux. When the last page of an object is unmapped, its pager
hears `TAG_OBJECT_IDLE` (`0xFFFF_0006`, sender 0, `data` = `[cookie, id]`).

`SYS_OBJECT_CTL` is the pager's, on its own object:

| Op | Name | arg2 | arg3 | Returns |
|---|---|---|---|---|
| 0 | resize | new size in bytes | — | 0 |
| 1 | read a cached page | a page to fill | page | 1 if cached, 0 if not |
| 2 | write a cached page | a page to copy | page | 1 if cached, 0 if not |
| 3 | take a dirty page | a page to fill | the lowest page to consider | the page's index, `u64::MAX` if none |
| 4 | release | — | — | 0, or `u64::MAX` while anything maps it |

A page mapped shared and writable is the cached frame itself, so every
mapping sees every other's writes at once; the page is dirty from its first
mapping, and op 3 leaves it dirty while anything maps it writable — it can
change again without a fault — so a pager walks on from each page it takes.
`SYS_OBJECT_SYNC` finds the objects mapped shared in the caller's range and
calls each one's pager with `TAG_OBJECT_SYNC` (`0xFFFF_0007`, `sender` marked
as for a page-in, `data` = `[cookie, object id]`), returning once all have
answered. A pager writes back what is dirty then, and again when the object
goes idle, before it releases it.

The cache belongs to the object and lasts until it is released; nothing
records where a cached frame is mapped, so a shrinking object keeps the
frames past its new end. An object's slot is kept in bits 52–62 of every
page-table entry that refers to it, which is what counts its mapped pages;
the CPU ignores those bits only while protection keys are off, and the
kernel keeps CR4.PKE clear. A pager's objects stop paging when it dies, and
go when nothing maps them.

### ABI introspection (0xF0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 208 | `SYS_PTY_CREATE` | — | a descriptor for a new pty's master / `u64::MAX` | — |
| 210 | `SYS_PTY_OPEN` | arg0 = a pty's number | a descriptor for its slave / `u64::MAX` | — |
| 209 | `SYS_PTY_CTL` | arg0 = a descriptor naming either end, arg1 = op (0 get termios, 1 set termios, 2 get window size, 3 set window size, 4 the pty's number), arg2 = the structure | per op / `u64::MAX` | — |
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

## How a program starts

A spawner creates a task and an address space, maps the program's segments and
a stack, and starts it at its entry point with nothing in its registers. What a
program is told about itself is on one read-only page at `0x80_8000_0000`,
which the spawner fills in three parts:

| Offset | Contents |
|---|---|
| 0 | argument count `n`; then `n` entries, each a `u64` length and that many bytes |
| after the arguments | environment count `m`; then `m` entries in the same form |
| 3184 | program header entry size (56); entry count `k`; then `k` ELF64 program headers |

The arguments and the environment stop before offset 3184 whatever their
length, so the header table cannot be crowded out. It is a verbatim copy of the
program's own table, at most sixteen entries, with any `PT_PHDR` entry's
address rewritten to the copy's so that a loader computing a base from the two
gets zero.

It exists because a program's headers are not in any segment it loads, and a C
library needs them: musl finds a static program's thread-local template through
`AT_PHDR`, and without it every thread-local landed outside its block. A C
runtime passes `AT_PHDR` = `0x80_8000_0000 + 3184 + 16`, `AT_PHENT` and
`AT_PHNUM` from this table. A spawner older than the table leaves the count
zero; a program older than it never reads that far.

## Notes for implementers

**TID reuse.** Reaping returns a task slot to the pool, so TIDs are recycled.
Anything that names a task by number must cope with the name changing meaning:
an `Endpoint` records the number of a task's endpoint rather than its TID for
exactly this reason. A server that keeps a client's TID beyond one call should
watch it (`SYS_TASK_WATCH`) and forget it when it dies. Do not cache a TID
across the lifetime of the task it named.

**Threads are tasks.** A thread is a task created in its creator's own address
space (`SYS_TASK_CREATE` then `SYS_TASK_START_ARG`), with its own FS base
(`SYS_SET_FS_BASE`) for thread-locals and a word the kernel clears when it
exits (`SYS_SET_CLEAR_TID`). Each task has its own floating-point and SSE state,
saved on every switch.

**Services are found by name.** The nameserver is at a well-known TID and maps
names to TIDs (`TAG_LOOKUP`), and back (`TAG_LOOKUP_TID`). Every IPC server also
answers `TAG_PING` with an empty reply, which is the portable way to ask whether
one is alive. Note that not every service is an IPC server — the text console,
`qtty`, is driven by a pipe and does not answer.
