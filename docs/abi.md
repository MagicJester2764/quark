# Quark syscall ABI

**Version 3.2.** Query the running kernel with `SYS_ABI_VERSION` (240), which
returns `(major << 16) | minor`.

This document is the contract between the Quark kernel and everything above it.
A service, a language runtime, or a C library should be buildable against this
document alone, without reading kernel source.

The kernel installs it. `make install` in the kernel's tree puts a copy at
`usr/share/doc/quark/abi.md` and, beside it, `usr/include/quark/abi.h`:
`QUARK_ABI_VERSION_MAJOR`, `QUARK_ABI_VERSION_MINOR` and one `#define` per
call, generated from the constants the dispatch is compiled from. A consumer
that keeps its own copy of the numbers — as a runtime and a C library must —
checks it against that header rather than against anybody's source.

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

A few calls deviate deliberately and say so below: `SYS_CALL_TIMEOUT` returns
`1` for a timeout, because timing out is an answer rather than a failure to
ask; the futex waits return `1` when the word had already changed and `2` when
the time ran out; and the calls that answer instead of waiting —
`SYS_FD_READ_NB`, `SYS_FD_WRITE_NB`, and `SYS_FD_SEND` and `SYS_FD_RECV` when
asked not to wait — return `0xFFFF_FFFE` for "would block", distinct from `0`,
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
| 0xD0  | 208–223   | Terminals                     |
| 0xE0  | 224–239   | Descriptors, continued        |
| 0xF0  | 240–255   | ABI introspection             |

Every block is assigned. Threads never needed one: a thread is a task started
with its creator's address space, so it is built from calls that already
existed.

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
| 2.9 | What a process is, and what it runs in. `SYS_TIMER_CREATE` (146), `SYS_TIMER_SET` (147) and `SYS_TIMER_GET` (148) — a deadline as a descriptor, so that a program's event loop waits for a blink with everything else it waits for. Block 0xD0 opens: `SYS_PTY_CREATE` (208) and `SYS_PTY_CTL` (209) — a pseudo-terminal pair as two ordinary descriptors, with a line discipline (echo, canonical input, newline translation), a `termios` and a window size. `SYS_FORK` (110) — a copy of the caller in a copy of its address space, which returns 0 there. `SYS_EXEC_SPACE` (111) — the caller becomes the program in an address space it built, keeping its id, its descriptors and its capabilities. `SYS_ADDRSPACE_DESTROY` (38, and see 3.0 — the number collided and the call never ran) — throw away an address space nothing is running in, which a spawn or an exec that failed part-way had no way to do. Also: `SYS_ADDRSPACE_CREATE` (36) and `SYS_ADDRSPACE_GIVE` (43) no longer ask for `TaskMgmt` — an address space the caller made, filled with pages it already owned, confers authority over nothing — and a task the caller created and has not started is its own to fill and to start, so `SYS_TASK_CREATE_IN` (109), `SYS_TASK_START` (97), `SYS_FD_DUP` (68), `SYS_PIPE_FD_SET` (70) and `SYS_CAP_GRANT` (81) accept one without it. |

### What 3.0 changed

**`SYS_ADDRSPACE_DESTROY` moves from 38 to 44**, because 38 was already
`SYS_MAP_PHYS`. The dispatch is a `match` and takes the first arm that matches,
so the call added at 2.9 never ran: every attempt to throw away an address
space mapped physical memory instead, and the leak it was written to close was
never closed. Nothing could have depended on the old number — it did not do
what its name said — so this is a correction rather than a change of
interface, but it is a number that moved and the major version says so.
`tools/check-abi.sh` fails on two names sharing a number now, and on this
document lacking a row for a call or describing a version the kernel does not
report.

Added with it:

| Call | What |
|---|---|
| `SYS_FD_WRITE_NB` (79) | The mirror of `SYS_FD_READ_NB`: a write that answers "would block" rather than parking. A descriptor a program marked non-blocking must not park it in either direction. |
| `SYS_EVENT_CREATE` (131) | A counter with a descriptor — `eventfd`. One task adds to it, another waits until it is not zero and takes what is there. Semaphore mode takes one instead of all. It is in the synchronisation block because that is what it is for, and because the descriptor block is full. |

Also: **`SYS_POLL` with no descriptors is a sleep** rather than an immediate
return. A main loop whose sources are all timeouts polls nothing at all, and
returning at once turned that loop into a spin.

### What each minor of 3 added

| Version | Added |
|---|---|
| 3.1 | **A descriptor belongs to a program, and may name an object in a server.** Every task of a program uses one table, so what one thread opens its siblings see and what one closes is closed; a thread used to start on a copy of its creator's table and share nothing afterwards. Sixty-four descriptors rather than thirty-two. `SYS_EXIT_PROGRAM` (8) ends every task of the caller's program with one status, which is what `exit` means and what `SYS_EXIT_CODE` — one task — never did. Block 0xE0 opens with *served descriptors*: `SYS_FD_SERVE` (224), `SYS_FD_SERVED` (225), `SYS_FD_HOLDS` (226), `SYS_FD_COOKIE` (227) and `SYS_FD_REAP` (229) let a server give a client a descriptor for one of its own objects — a file — which the kernel then counts, copies across `SYS_FORK`, keeps across `SYS_EXEC_SPACE` and reads and writes through like any other. Descriptor 64 is the program's working directory. `SYS_FD_FLAGS` (228) marks a descriptor to close when its program becomes another, and `SYS_EXEC_SPACE` closes those. Memory named by a descriptor may be mapped by whoever holds the descriptor, and lasts as long as a descriptor names it or somebody has it mapped, whoever made it. |
| 3.2 | What a shell asks of a kernel that is not about descriptors. `SYS_UMASK` (9): the permission bits a program leaves off what it makes. The kernel makes no files and never reads it; it keeps it because it must follow a program across `SYS_FORK` and `SYS_EXEC_SPACE`, which a C library's memory does not. `SYS_WAIT_FOR` (10): wait for one child in particular, or ask without waiting — `waitpid` with a process id, and `WNOHANG`. Three things change with no new number. `SYS_TASK_KILL` (5), the kill bit of `SYS_SIGNAL` (6) and a signal's deadline end the *program* the task named belongs to, and so does a fault: every task in the address space, where each used to end one task and leave its threads. A task of the caller's own program is still ended alone. A pty's line discipline acts on `ISIG` and on the erase, kill, word-erase and end-of-file characters. And a write to a pty's slave waits for room and writes everything, where it used to return what fitted — which could be nothing. |

3.1 was a change of behaviour and no change of number, so a minor: nothing built for
3.0 calls anything that means something else now. What it could have relied on
is a thread's copy of a descriptor outliving its sibling's `close`, and no
program did.

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
| 9 | `MemObject` | the object's id | access: 1 read, 2 write |

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
| 8 | `SYS_EXIT_PROGRAM` | arg0 = status | does not return | — |
| 9 | `SYS_UMASK` | arg0 = the new mask (nine bits), or `u64::MAX` to leave it | the mask as it was | — |
| 10 | `SYS_WAIT_FOR` | arg0 = a child's tid, or 0 for any; arg1 = flags (1 = do not wait) | `tid \| (exit_code << 32)`; 0 if asked not to wait and none has ended; `u64::MAX` if there is no such child. **Blocks** unless asked not to. | — |

A status is kept as its low eight bits, as Linux's wait status keeps it. A
negative status is how the kernel says a task was killed — `SYS_TASK_KILL`
reports -9, a fault the negated signal — so a task cannot set one and claim it
was.

`SYS_EXIT` is equivalent to `SYS_EXIT_CODE(0)`; it exists separately because
`syscall0` leaves RDI undefined, so the original call could not grow an
argument.

Both end the calling *task*. `SYS_EXIT_PROGRAM` ends the program: every other
task in the caller's address space is ended with the same status, and then
the caller. It is what a C library's `exit` and a Rust `main` returning mean.
A thread ending itself uses `SYS_EXIT_CODE`, and a program whose first task
does that goes on running in its other threads, with everything it has open.

`SYS_TASK_KILL` ends a *program*: every task in the address space `tid`
belongs to, each with status -9. Naming one task of another program and
ending only that one left a threaded program's other threads behind, with
everything it had open. The exception is a task of the caller's own program,
which is a thread it is ending, and is ended alone; and a task that has not
been started, which is in no program yet.

`SYS_WAIT_FOR` is `SYS_WAIT` with two things said. A child named is the only
one collected, and the only one whose ending wakes the caller: waiting for
one child by collecting whichever ends first would lose the status of every
other, and a shell running a pipeline wants each. And a caller that asks not
to wait is told 0 — nothing has ended yet — which is different from having no
children at all. `SYS_WAIT` is this with neither.

`SYS_UMASK` holds nine bits for the program and does nothing with them. They
are the permission bits its runtime leaves off a file or a directory it makes
— 022 until it says otherwise — and they are the kernel's to keep only
because they must go where the program goes: a forked child starts with its
parent's, and `SYS_EXEC_SPACE` leaves them alone. Whatever makes the file
applies them.

**Signals.** There are three, and they are bits: interrupt (`1 << 16`),
terminate (`1 << 17`) and kill (`1 << 18`). `SYS_SIGNAL` with the kill bit ends
the target's program at once, with status -9. Either of the others is raised
in the target's notification word, where the target finds it at its next
receive (see `SYS_NOTIFY` below); a call the target is blocked in is abandoned
and returns failure, so that a task waiting on a server gets to look; and its
program is killed 500 ticks later if the task is still there. A second signal
does not extend that. Tasks 0 and 1 cannot be signalled.

These are not POSIX signals. Nothing runs a handler in the target, there are no
masks and no process groups, and nothing is sent when a child exits or a
terminal changes size.

**A negative exit code means the kernel killed the task.** Its magnitude is the
signal Linux sends for the exception that did it: 4 for an invalid opcode, 5
for a debug trap or breakpoint, 7 for an alignment check, 8 for a divide error
or floating-point exception, and 11 for everything else — a page fault with no
pager, a general protection fault, a stack fault. `SYS_WAIT` reports it as it
reports any other status. A fault ends the *program* the task belongs to,
every task of it with that status, as the signal would on Linux: a thread
that faulted and went alone left the rest of its program waiting on it. A
killed task never halts the machine: only a fault taken in ring 0 does that.
`SYS_TASK_KILL` reports -9, as SIGKILL would.

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

**Notifications.** Every task has a notification word. `SYS_NOTIFY` ORs
`badge` into the destination's and does not wait; the destination receives the
word, and clears it, at its next `SYS_RECV` from sender 0 or from anybody, as a
message from sender 0 with tag `0xFFFF_0002` and the bits in `data[0]`. Bits
that were raised more than once before being collected arrive once.

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
| 41 | `SYS_ADDRSPACE_SELF` | — | the address space the caller is running in, as the CR3 the other address-space calls take / `u64::MAX` | — |
| 44 | `SYS_ADDRSPACE_DESTROY` | arg0 = cr3 the caller made, with no task in it | 0 / `u64::MAX` | — |
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
like anything else a program holds. Such a region is mapped through the
descriptor (`SYS_MMAP_FD`), by whoever holds one: no task is on a list for
it, and `SYS_SHMEM_MAP`, `SYS_SHMEM_GRANT` and `SYS_SHMEM_DESTROY` by handle
are refused. It
lasts as long as a descriptor names it or somebody has it mapped — whichever
task made it, and whether or not that task is still running.
`SYS_MEMFD_TRUNCATE` works only while exactly one descriptor names the region
and nothing has it mapped: a second descriptor is one that has been somewhere.

Destruction is deferred while any mapping remains. Futexes are keyed on physical
address, so a futex word inside a shared region is one object to every task that
maps it.

### File descriptors and pipes (0x40)

Sixty-four descriptors per program. 0, 1 and 2 are stdin, stdout and stderr by
convention. A descriptor is one of: unset, an IPC endpoint (a service TID plus a
tag), a pipe read end, a pipe write end, one end of a stream, shared memory, a
set of descriptors to wait on, one end of a pseudo-terminal, a timer, an event
counter, a network connection, or an object in a server. The calls that make
the last five are in the blocks they belong to — terminals, time,
synchronisation, sockets and "descriptors, continued" — and everything here
that reads, writes, waits on, copies or closes a descriptor takes any of them.

A descriptor names an object and holds a reference to it. Closing the last one
frees the object, and is what makes a pipe's reader see end-of-file.

**The table is the program's.** Every task running in one address space uses
the same table: a descriptor one thread makes is there for the others, and one
that a thread closes is closed. A task ending closes nothing unless it is the
program's last. `SYS_FORK` gives the child a table of its own holding a second
descriptor for everything in the parent's, and `SYS_EXEC_SPACE` keeps the
table and closes what was marked for it (`SYS_FD_FLAGS`, in block 0xE0). A
task a spawner has created and not started has an empty table, which the
spawner fills.

A call that is waiting on what a descriptor names keeps it: a read parked on
a pipe goes on waiting on that pipe if a sibling closes the descriptor, as it
would on Linux, and the pipe is freed when the read returns.

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 64 | `SYS_FD_READ` | arg0 = fd, arg1 = buf, arg2 = max len | bytes read, `0` = EOF, `u64::MAX` = error. **Blocks.** | — |
| 65 | `SYS_FD_WRITE` | arg0 = fd, arg1 = buf, arg2 = len | bytes written / `u64::MAX` | — |
| 66 | `SYS_FD_READ_NB` | arg0 = fd, arg1 = buf, arg2 = max len | bytes, `0` = EOF, **`0xFFFF_FFFE` = would block**, `u64::MAX` = error | — |
| 67 | `SYS_FD_SET` | arg0 = target tid, arg1 = fd, arg2 = service tid, arg3 = tag | 0 / `u64::MAX` | `TaskMgmt` |
| 68 | `SYS_FD_DUP` | arg0 = target tid, arg1 = target fd or `u64::MAX - 1` for any free one, arg2 = source fd, arg3 = lowest acceptable fd when arg1 asks for any | the fd it took / `u64::MAX` | `TaskMgmt` over the target, unless the target is the caller or a child it has not started |
| 69 | `SYS_PIPE_CREATE` | — | handle / `u64::MAX` | — (bounded per task) |
| 70 | `SYS_PIPE_FD_SET` | arg0 = target tid, arg1 = fd or `u64::MAX - 1` for any free one, arg2 = pipe handle, arg3 = 1 for write end | the fd it took / `u64::MAX` | `TaskMgmt` over the target, unless the target is the caller or a child it has not started |
| 71 | `SYS_FD_CLOSE` | arg0 = fd | 0 / `u64::MAX` | — |
| 72 | `SYS_SOCKETPAIR` | — | `(fd0 << 32) \| fd1`, both in the caller's table / `u64::MAX` | — |
| 73 | `SYS_FD_SEND` | arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to pass or `u64::MAX`, arg4 = flags (1 = do not wait) | bytes written, `0xFFFF_FFFE` if it would have blocked / `u64::MAX` | — |
| 74 | `SYS_FD_RECV` | arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to install a passed descriptor at, `u64::MAX - 1` for any free one, or `u64::MAX` to leave it queued, arg4 = flags (1 = do not wait) | `((fd + 1) << 32) \| bytes`, high half 0 if none arrived, `0xFFFF_FFFE` if it would have blocked / `u64::MAX` | — |
| 75 | `SYS_POLLSET_CREATE` | — | fd naming the set / `u64::MAX` | — |
| 76 | `SYS_POLLSET_CTL` | arg0 = set fd, arg1 = op (0 add, 1 modify, 2 remove), arg2 = fd, arg3 = events, arg4 = token | 0 / `u64::MAX` | — |
| 77 | `SYS_POLLSET_WAIT` | arg0 = set fd, arg1 = array of `(u64 token, u32 events, u32 pad)`, arg2 = capacity, arg3 = timeout in ticks | entries filled, 0 = timed out / `u64::MAX` | — |
| 78 | `SYS_POLL` | arg0 = array of `(u32 fd, u32 events, u32 revents, u32 pad)`, arg1 = count, arg2 = timeout in ticks | entries with non-zero `revents` / `u64::MAX` | — |
| 79 | `SYS_FD_WRITE_NB` | arg0 = fd, arg1 = buf, arg2 = len | bytes written, **`0xFFFF_FFFE` = would block**, `u64::MAX` = error | — |

**Streams.** `SYS_SOCKETPAIR` makes two connected ends and puts both in the
caller's table; moving one into another task is `SYS_FD_DUP` followed by closing
the caller's copy. An end is reference counted, so that last step does not tell
the peer the connection has gone.

**Copying onto a descriptor closes it first**, as `dup2` does: `SYS_FD_DUP`,
`SYS_FD_SET` and `SYS_PIPE_FD_SET` release whatever the target slot named, and
copying a descriptor onto itself changes nothing. A poll set cannot be copied or
sent at all: it counts no holders, so it has exactly one.

**Passing a descriptor needs no authority over the peer.** `SYS_FD_DUP` puts one
into a task that never asked, so it requires `TaskMgmt` over that task — unless
the task is a child the caller made and has not started, which is a spawner
wiring up what it is about to run.
`SYS_FD_SEND` hands one to a task that called `SYS_FD_RECV`: the sender chose to
send and the receiver asked to take, and consent on both sides is the whole
authorisation. Memory arriving this way is the receiver's to map: holding the
descriptor is the permission.

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

### Capabilities (0x50)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 80 | `SYS_CAP_MINT` | arg0 = slot, arg1 = type, arg2 = param0, arg3 = param1 | 0 / `u64::MAX` | must already hold one covering it; for an `Endpoint`, param0 is the destination's TID and the rule is ownership (above) |
| 81 | `SYS_CAP_GRANT` | arg0 = dest tid, arg1 = src slot, arg2 = dest slot or `u64::MAX - 1` for any | 0, or the slot used when any; `u64::MAX` on failure | `TaskMgmt` over dest, its consent, or dest is a child the caller has not started |
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
| 96 | `SYS_TASK_CREATE` | — | TID / `u64::MAX` | — for up to sixteen children at once; `TaskMgmt` for more |
| 97 | `SYS_TASK_START` | arg0 = tid, arg1 = rip, arg2 = rsp, arg3 = cr3 | 0 / `u64::MAX` | `TaskMgmt`; or none, for a child of the caller's started in the caller's own address space (a thread) or in one the caller made (a spawn) |
| 103 | `SYS_TASK_START_ARG` | as `SYS_TASK_START`, and arg4 = the value the task finds in RDI | 0 / `u64::MAX` | as `SYS_TASK_START` |
| 98 | `SYS_GET_UID` | — | current UID | — |
| 99 | `SYS_SET_UID` | arg0 = tid, arg1 = uid | 0 / `u64::MAX` | `SetUid` |
| 100 | `SYS_SET_GID` | arg0 = tid, arg1 = gid | 0 / `u64::MAX` | `SetUid` |
| 101 | `SYS_GET_TUID` | arg0 = tid | that task's UID | — |
| 102 | `SYS_SET_FS_BASE` | arg0 = the caller's new FS base, a user address | 0 / `u64::MAX` | — |
| 104 | `SYS_TASK_WATCH` | arg0 = tid | 0, or `u64::MAX` if that task is already gone | — |
| 106 | `SYS_SET_CLEAR_TID` | arg0 = address of a `u32`, or 0 | this task's id / `u64::MAX` | — |
| 107 | `SYS_TASK_SPACE` | arg0 = tid | that task's space id / `u64::MAX` | — |
| 108 | `SYS_SPACE_WATCH` | arg0 = space id | 0, or `u64::MAX` if no task of it is alive | — |
| 109 | `SYS_TASK_CREATE_IN` | arg0 = cr3 of an address space the caller created | TID / `u64::MAX` | as `SYS_TASK_CREATE` |
| 110 | `SYS_FORK` | — | the child's TID, `0` in the child / `u64::MAX` | — |
| 111 | `SYS_EXEC_SPACE` | arg0 = cr3 the caller made, arg1 = entry, arg2 = rsp | does not return / `u64::MAX` | — |
| 105 | `SYS_TASK_PRIORITY` | arg0 = tid, arg1 = band | 0 / `u64::MAX` | `TaskMgmt` for target, and the caller's own band or worse |

A program starts one of two ways. A parent may *build* one: it creates a task,
makes its address space, loads its image, sets its arguments, descriptors and
capabilities, and starts it. A task the caller created and has not started is
its own to fill — nobody else can name it, it holds nothing and it cannot run —
so none of that asks for `TaskMgmt`; the capability buys more than sixteen
children at once, and the right to touch a task that is already running. Or a
task may `SYS_FORK`, which copies it, and `SYS_EXEC_SPACE`, which keeps the
task and replaces the program it runs. TIDs are reused once a task is reaped,
so a TID identifies a task only for as long as that task lives — see the note
on TID reuse below.

`SYS_SET_FS_BASE` takes effect at once rather than at the next switch: the
caller is running and will use the register before it is scheduled again. A
base in the kernel's half of the address space is refused.

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
thread of it. It uses the program's descriptors — the same table, not a copy —
and starts with a copy of its creator's capabilities, each in a slot the
creator did not already fill for it, and the creator's band. Capabilities are
a task's: what either is granted or gives up afterwards, the other does not
see.

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
| 131 | `SYS_EVENT_CREATE` | arg0 = the count it starts at, arg1 = flags (1 = semaphore) | a descriptor readable while the counter is not zero / `u64::MAX` | — |

Futexes are keyed on **physical** address, so a word in shared memory is one
futex to every task that maps it, whatever virtual address each uses.

A timeout of 0 ticks makes `SYS_FUTEX_WAIT_TIMEOUT` a check rather than a wait:
it returns 1 if the value already differs and 2 if it does not, without
blocking. There is no way to ask for an unbounded wait through this call; that
is what `SYS_FUTEX_WAIT` is.

**Event counters.** `SYS_EVENT_CREATE` makes a 64-bit counter and returns a
descriptor for it. A write is eight bytes and adds them; a read is eight bytes
and takes the whole count, leaving zero — or takes one, leaving the rest, if
the counter was made with the semaphore flag. A read waits while the count is
zero and `SYS_FD_READ_NB` answers "would block" instead. The descriptor is
readable while the count is not zero. The largest count is `u64::MAX - 1`;
writing `u64::MAX`, or zero, is refused. There are sixteen in the machine.

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
| 146 | `SYS_TIMER_CREATE` | — | a descriptor that becomes readable when its deadline passes / `u64::MAX` | — |
| 147 | `SYS_TIMER_SET` | arg0 = fd, arg1 = ticks until it fires (0 disarms), arg2 = ticks between firings | 0 / `u64::MAX` | — |
| 148 | `SYS_TIMER_GET` | arg0 = fd | `(interval << 32) \| ticks left` / `u64::MAX` | — |

The PIT runs at 100 Hz, so one tick is 10 ms. Every timeout argument in this
ABI is in ticks. The time of day is `SYS_BOOT_TIME + SYS_TICKS / 100`: the
kernel reads the PC's battery-backed clock once, at boot, and never again.

**Timers.** A timer is a descriptor that becomes readable when its deadline
passes. A read is eight bytes: the number of times it has fired since it was
last read, which a read clears. It waits while that is zero, and
`SYS_FD_READ_NB` answers "would block" instead. A repeating timer that fell
behind its reader does not fire in a burst to catch up: the next deadline is
one interval after the tick it fired on. There are sixteen in the machine.

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

### Terminals (0xD0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 208 | `SYS_PTY_CREATE` | — | a descriptor for a new pty's master / `u64::MAX` | — |
| 209 | `SYS_PTY_CTL` | arg0 = a descriptor naming either end, arg1 = op (0 get termios, 1 set termios, 2 get window size, 3 set window size, 4 the pty's number), arg2 = the structure | per op / `u64::MAX` | — |
| 210 | `SYS_PTY_OPEN` | arg0 = a pty's number | a descriptor for its slave / `u64::MAX` | — |

A pseudo-terminal is a pair of descriptors with a line discipline between
them: what a terminal emulator holds, the master, and what the program in it
holds, the slave. `SYS_PTY_CREATE` makes the pair and returns the master; its
number comes from `SYS_PTY_CTL` op 4, and `SYS_PTY_OPEN` on that number returns
a slave, which any holder of the number may open while the master is held.
There are eight in the machine.

Either end is read, written, waited on and closed as any descriptor is. Before
a slave has been opened, a read of the master waits rather than reporting end
of file: the program that will hold the slave has not been started yet. After,
the last slave closing *is* end of file at the master, and a poll reports it as
readable with hangup.

The structures are Linux's, so a C library hands them through unchanged:

```
termios, 36 bytes: c_iflag, c_oflag, c_cflag, c_lflag (u32 each),
                   c_line (u8), c_cc (19 bytes)
winsize,  8 bytes: ws_row, ws_col, ws_xpixel, ws_ypixel (u16 each)
```

Of a `termios` the kernel acts on six bits and seven characters, and stores
the rest: `ICRNL` in `c_iflag` (a carriage return typed arrives as a newline),
`OPOST` with `ONLCR` in `c_oflag` (a newline written goes out as carriage
return and newline), and `ICANON`, `ECHO` and `ISIG` in `c_lflag`. A new pty
has all six set, and is 24 rows by 80 columns. The window size is stored and
handed back, and nobody is told when it changes.

With `ICANON`, input is held until a newline, and four characters from `c_cc`
edit what is being held: `VERASE` (and backspace and delete, whichever the
terminal sends) takes a character back, `VKILL` the line, `VWERASE` a word.
`VEOF` hands over what has been typed as it stands, with no newline — and
typed on an empty line that is a read of nothing, once, which is how a program
reading a terminal is told there is no more. A poll reports that as readable,
and not as a hangup. With `ECHO`, what is typed is written back to the master,
and what is taken back is rubbed out there.

With `ISIG`, `VINTR` and `VQUIT` are not input: the character is taken out,
the line it was typed into and anything not yet read are thrown away, and it
is echoed as `^C`. `VSUSP` is taken out and nothing else: stopping a job needs
jobs. A character set to 0 in `c_cc` is switched off.

A write to the slave — what a program prints — waits for room when the pty's
buffer (4096 bytes) is full, and returns when all of it has been taken, or
with what was taken if the master has gone (`u64::MAX` if that is nothing). A
write to the master is typing and never waits: it returns what was taken,
which may be less, or nothing.

### Descriptors, continued (0xE0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 224 | `SYS_FD_SERVE` | arg0 = client tid, arg1 = cookie, arg2 = where: a free descriptor number, `u64::MAX - 1` for the lowest free from 3, or 64 for the working directory | the descriptor / `u64::MAX` | the client is in a call to the caller |
| 225 | `SYS_FD_SERVED` | arg0 = one of the caller's descriptors (64 allowed), arg1 = two words to fill | 0, and `[server tid, cookie]` / `u64::MAX` if it is not a served descriptor or its server has gone | — |
| 226 | `SYS_FD_HOLDS` | arg0 = tid, arg1 = cookie | 1 if that task's program holds a descriptor for the caller's object `cookie`, else 0 | — |
| 227 | `SYS_FD_COOKIE` | arg0 = tid, arg1 = one of its descriptors (64 allowed) | the caller's cookie there / `u64::MAX` if what is there is not the caller's | — |
| 228 | `SYS_FD_FLAGS` | arg0 = fd, arg1 = 0 to read or 1 to set, arg2 = flags (1 = close when the program becomes another) | the flags, or 0 on a set / `u64::MAX` | — |
| 229 | `SYS_FD_REAP` | — | the cookie of one of the caller's objects that no descriptor names any more / `u64::MAX` when there is none | — |

**A served descriptor names an object in a server**: a file, most often. The
object is the server's, known to it by a number of its own choosing — the
*cookie* — and the kernel's part is to count who holds a descriptor for it.
That is what makes a file an ordinary descriptor: `SYS_FD_DUP` copies it,
`SYS_FORK` gives the child one, `SYS_EXEC_SPACE` keeps it, `SYS_FD_SEND`
passes it, `SYS_FD_CLOSE` drops it, and `SYS_POLL` reports it readable and
writable, always. None of those is a message to the server.

- **Making one.** `SYS_FD_SERVE` puts a descriptor for `cookie` in the table
  of a task that is in a call to the server. The call is the consent, as it is
  for `SYS_CAP_GRANT`: nobody is handed a descriptor unasked. The server says
  where — a number that must be free, the lowest free from 3, or the working
  directory, which it replaces — and tells the client in its reply.
- **Using one.** `SYS_FD_READ` and `SYS_FD_WRITE` (and their non-blocking
  forms, which are the same here) are a call the kernel makes to the server on
  the task's behalf: tag `0xFFFF_0009` to read or `0xFFFF_000A` to write,
  `data` = `[cookie, length]`, with the task's buffer lent for the server to
  fill or to read, at most a mebibyte at a time. The server answers tag 0 with
  the count in `data[0]`, and anything else is `u64::MAX` to the task. The
  task needs no `Endpoint` for the server: the descriptor is the permission.
  So anything that can write to descriptor 1 can write to a file put there.
- **Asking about one.** For anything else a client wants of the object, it
  finds out which server and which cookie (`SYS_FD_SERVED`) and asks the
  server in the server's own protocol, naming the cookie.
- **Believing a client.** A server asked to act on a cookie asks the kernel
  whether the task asking holds it: `SYS_FD_HOLDS`. A task holds a cookie only
  by having a descriptor for it, and gets a descriptor only from the server,
  from a parent, or over a stream from somebody who had one. `SYS_FD_COOKIE`
  is the same question about one particular descriptor, which is how a server
  learns a client's working directory. Both answer only about the caller's own
  objects.
- **Losing one.** When the last descriptor for a cookie goes — closed, or its
  program ended — the server is sent a notice: sender 0, tag `0xFFFF_0008`, no
  data. It then calls `SYS_FD_REAP` until that says there is nothing; each
  call hands back one cookie, which the kernel forgets as it does. The notice
  is a flag rather than a queue, so a server that was busy loses nothing: the
  cookies wait to be collected, and one notice covers however many there are.

A server is known by its endpoint, which no other task is ever given. When it
dies, every descriptor for its objects goes on existing and does nothing:
reads, writes and `SYS_FD_SERVED` fail, and the last close simply forgets it.

**Descriptor 64 is the working directory.** It is one more slot of the
program's table, past the ordinary numbers, and holds a served descriptor for
a directory or nothing. `SYS_FORK` copies it and `SYS_EXEC_SPACE` keeps it, so
a program's children start where it is without anybody telling a server that
one program became another. It can be the source or the destination of
`SYS_FD_DUP` — onto it is `fchdir`, and a spawner gives a child its directory
with `SYS_FD_DUP(child, 64, 64)` — and `SYS_FD_SERVED` and `SYS_FD_COOKIE`
answer for it. It cannot be read, written, waited on or closed, and no call
that chooses a number ever chooses it.

`SYS_FD_FLAGS`: a flag is the descriptor's, not the object's. A copy made by
`SYS_FD_DUP` starts unmarked; `SYS_FORK` carries a mark across, because the
child's table is a copy of the whole of the parent's. Putting something else
at a number clears its mark.

### ABI introspection (0xF0)

| # | Name | Arguments | Returns | Cap |
|---|---|---|---|---|
| 240 | `SYS_ABI_VERSION` | — | `(major << 16) \| minor` | — |

Its number has not moved since 1.0 and its block holds nothing else, so it is
the one call a program can make before it knows which ABI it is talking to.

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
saved on every switch, and its own capabilities; the descriptors and the
memory are the program's.

**Services are found by name.** This one is the userland's convention rather
than the kernel's, and quarkutils' to change. The nameserver is at a well-known TID and maps
names to TIDs (`TAG_LOOKUP`), and back (`TAG_LOOKUP_TID`). Every IPC server also
answers `TAG_PING` with an empty reply, which is the portable way to ask whether
one is alive. Note that not every service is an IPC server — the text console,
`qtty`, is driven by a pipe and does not answer.
