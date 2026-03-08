# Missing Features

## What Quark has
Scheduler, synchronous IPC, address spaces, capabilities, fd table, IRQ delegation, PMM, heap, futex/mutex, ELF loading, nameserver, console, keyboard, input, disk, VFS

## High impact (blocking real programs)

1. ~~**User-space memory allocation**~~ — **Done.** `sys_mmap(vaddr, pages)` (syscall 70) allocates+maps frames. `libquark::allocator` provides `#[global_allocator]` backed by sys_mmap, enabling `Vec`, `String`, etc.

2. ~~**Async notifications / non-blocking IPC**~~ — **Partially done.** `sys_recv_timeout(from, msg, ticks)` (syscall 80) adds non-blocking poll (ticks=0) and timed receive. Full notification word (seL4-style) still missing for true multiplexed wait.

3. ~~**Timers for userspace**~~ — **Done.** `sys_ticks()` (syscall 81) reads PIT counter. `libquark::syscall::sleep_ms(ms)` and `sleep_ticks(ticks)` provide blocking sleep via recv_timeout.

4. **Page fault / exception forwarding** — A page fault in userspace triple-faults. Forwarding exceptions to a designated pager task is a classic microkernel pattern and enables demand paging, copy-on-write, and stack growth.

## Medium impact (needed for real workloads)

5. **Write support in VFS/disk** — Disk driver is read-only. No file creation or modification.

6. **Process groups / wait** — No `sys_wait()` for parent to wait on child exit. Init can't know when a spawned task finishes. Reaping is fire-and-forget.

7. **Per-task memory limits / quotas** — Any task with `CAP_PHYS_ALLOC` can exhaust all RAM. No resource accounting.

8. **Proper `exec` / program arguments** — No way to pass argv/argc/environment to a spawned program. Init hardcodes everything.

## Lower priority (completeness)

9. **SMP support** — Single-core only. Would need per-CPU scheduler state, IPI for cross-core scheduling, lock-aware context switch.

10. **Network stack** — Typically a userspace service in a microkernel.

11. **Shared memory** — No way for two tasks to map the same physical pages. Needed for zero-copy IPC, mmap'd files, etc.

12. **Capability transfer over IPC** — Can't pass capabilities through messages. Init must pre-grant everything.
