# Missing Features

## What Quark has
Scheduler, synchronous IPC, address spaces, capabilities, fd table, IRQ delegation, PMM, heap, futex/mutex, ELF loading, nameserver, console, keyboard, input, disk, VFS, network

## High impact (blocking real programs)

1. ~~**User-space memory allocation**~~ — **Done.** `sys_mmap(vaddr, pages)` (syscall 70) allocates+maps frames. `libquark::allocator` provides `#[global_allocator]` backed by sys_mmap, enabling `Vec`, `String`, etc.

2. ~~**Async notifications / non-blocking IPC**~~ — **Done.** `sys_recv_timeout(from, msg, ticks)` (syscall 80) adds non-blocking poll and timed receive. `sys_notify(dest, badge)` (syscall 85) provides seL4-style async notifications: badge bits are OR'd into a per-task notification word, delivered as `TAG_NOTIFICATION` messages via `sys_recv`. Enables true multiplexed wait over IPC + IRQs + notifications.

3. ~~**Timers for userspace**~~ — **Done.** `sys_ticks()` (syscall 81) reads PIT counter. `libquark::syscall::sleep_ms(ms)` and `sleep_ticks(ticks)` provide blocking sleep via recv_timeout.

4. ~~**Page fault / exception forwarding**~~ — **Done.** User page faults are forwarded to a pager task via IPC (`TAG_PAGE_FAULT`, `sys_set_pager` syscall 82). If no pager, the faulting task is killed cleanly instead of triple-faulting. Enables demand paging, COW, and stack growth.

## Medium impact (needed for real workloads)

5. ~~**Write support in VFS/disk**~~ — **Done.** Disk driver supports `TAG_WRITE_SECTOR` (ATA PIO write). VFS supports `TAG_WRITE` (write file data with auto-extend) and `TAG_CREATE` (create files/directories with FAT32 8.3 entries). `libquark::vfs::write()` and `create()` provide the client API.

6. ~~**Process groups / wait**~~ — **Done.** `sys_wait()` (syscall 83) blocks parent until a child exits, returns child TID. Tasks track `parent_tid`; dead tasks become zombies until collected. `reap_dead()` respects parent/child relationships.

7. ~~**Per-task memory limits / quotas**~~ — **Done.** Tasks track `mem_pages` (current usage) and `mem_limit` (max, 0=unlimited). `sys_mmap` and `sys_phys_alloc` check the quota before allocating. `sys_set_mem_limit` (syscall 84) lets init set limits per task.

8. ~~**Proper `exec` / program arguments**~~ — **Done.** Init maps an argument page at `0x80_8000_0000` in child address spaces. `libquark::args::argc()` / `argv(n)` read from it. Init passes program name as argv[0] for all spawned tasks.

## Lower priority (completeness)

9. **SMP support** — Single-core only. Would need per-CPU scheduler state, IPI for cross-core scheduling, lock-aware context switch.

10. ~~**Network stack**~~ — **Done.** RTL8139 NIC driver as userspace service (`user/net`). PCI enumeration, DMA ring buffers, Ethernet framing, ARP (request/reply/cache), IPv4, ICMP echo reply (ping), UDP send/receive via IPC. `libquark::net` provides client API (`udp_send`, `udp_recv`, `configure`, `info`). Registers as "net" with nameserver. QEMU: add `-device rtl8139,netdev=n -netdev user,id=n`.

11. ~~**Shared memory**~~ — **Done.** `sys_shmem_create(pages)` (syscall 90) allocates a shared region, `sys_shmem_grant(handle, tid)` (92) grants access, `sys_shmem_map(handle, vaddr)` (91) maps into caller's space. Up to 32 regions, 16 pages each. Access tracked via per-region bitmask.

12. ~~**Capability transfer over IPC**~~ — **Done.** `sys_cap_transfer(dest, caps)` (syscall 93) lets any task transfer capabilities it holds to another task, without requiring CAP_TASK_MGMT. Services can now delegate their own capabilities to clients dynamically.

## Next features

13. ~~**Shell**~~ — **Done.** Interactive command interpreter (`user/shell`). Reads input, parses commands, loads ELFs from `/usr/bin/` via VFS, spawns tasks with fd wiring, waits for exit. Only builtin: `exit`. Loaded last by init so auto-run programs finish first.

14. ~~**`sys_kill` / signals**~~ — **Partially done.** `sys_task_kill(tid)` (syscall 104) terminates tasks from userspace. Requires `CAP_TASK_MGMT` or same UID. Shell has `kill <tid>` builtin. POSIX signals not implemented.

15. ~~**Pipes**~~ — **Partially done.** Kernel pipes (`sys_pipe_create`, `sys_pipe_fd_set`) exist and are used for console I/O transport. Shell `cmd1 | cmd2` syntax not yet implemented.

16. ~~**Userspace utilities (`ls`, `cat`, `echo`)**~~ — **Done.** `echo` prints arguments; `ls` lists directories via VFS (default `/usr/bin`); `cat` reads and prints files via VFS with phys page buffer. All loadable from shell.

17. **Signal delivery** — Instead of `sys_task_kill` immediately destroying a task, deliver an IPC notification (or kernel-injected signal) that the task can handle. Tasks would register a signal handler; the kernel or input server sends a "please exit" message rather than killing outright. Enables graceful shutdown (finish printing, flush buffers, release resources). Needed for Ctrl+C to not corrupt in-progress I/O.

18. **TCP** — Extend the net driver with TCP (3-way handshake, sliding window, retransmit). Enables HTTP, telnet, etc.

19. **DHCP client** — Auto-configure IP/netmask/gateway via DHCP instead of hardcoded 10.0.2.15.

20. **DNS resolver** — Name resolution so network programs can use hostnames.

21. ~~**Task listing (`ps`)**~~ — **Done.** `sys_task_info(tid)` (syscall 105) returns task state, UID, and parent TID. `ps` command lists all running tasks.

22. **Filesystem permissions** — FAT32 has no ownership/permissions. Options: sidecar `.permissions` files per directory, or switch to ext2. VFS would query caller UID via `SYS_GET_TUID(sender_tid)` and check against file ownership. Alternative: kernel includes sender UID in IPC messages (more secure). Also needed for setuid binaries.

23. **`shutdown` program** — Userspace utility to cleanly shut down the system. Would notify running services to flush and exit, then invoke ACPI power-off (or QEMU `isa-debug-exit`). Needs signal delivery (#17) for graceful service teardown.
