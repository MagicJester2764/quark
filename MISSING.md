# Missing Features

## What Quark has
Scheduler, synchronous IPC, address spaces, capabilities, fd table, IRQ delegation, PMM, heap, futex/mutex, ELF loading, nameserver, console, keyboard, input, disk, VFS, network, signals

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

17. ~~**Signal delivery**~~ — **Done.** `sys_signal(tid, sig)` (syscall 106) sends signals via the notification system. `SIG_INT` (Ctrl+C) and `SIG_TERM` deliver async notifications with a 2-second grace period before force-kill; `SIG_KILL` terminates immediately. Input server sends `SIG_INT` on Ctrl+C instead of instant kill. Shell `kill` builtin sends `SIG_TERM` by default, `kill -9` for `SIG_KILL`. `libquark::signal` provides `poll_signal()`, `extract_signal()`, and `default_handler()` for userspace signal handling.

18. ~~**TCP**~~ — **Done.** TCP protocol implemented in the net driver. 3-way handshake (active connect + passive listen/accept), data transfer with sequence numbers and ACKs, retransmission on timeout (3s), graceful close (FIN handshake), RST handling, TIME_WAIT. Up to 8 concurrent connections with 4 KiB send/receive buffers each. IPC: `TAG_TCP_CONNECT` (10), `TAG_TCP_LISTEN` (11), `TAG_TCP_SEND` (13), `TAG_TCP_RECV` (14), `TAG_TCP_CLOSE` (15). `libquark::net` provides `tcp_connect()`, `tcp_listen()`, `tcp_send()`, `tcp_recv()`, `tcp_close()`. `httpget` utility demonstrates TCP with HTTP/1.0 GET requests. QEMU: connect to external hosts via user-mode NAT.

19. ~~**DHCP client**~~ — **Done.** Built into net driver. Sends DHCP DISCOVER at startup, negotiates OFFER→REQUEST→ACK, applies IP/netmask/gateway. Falls back to static 10.0.2.15 after 5-second timeout. `TAG_NET_DHCP` (6) IPC for renewal. `libquark::net::dhcp_renew()` client API.

20. ~~**DNS resolver**~~ — **Done.** Built into net driver. Sends DNS A-record queries via UDP to configured DNS server (from DHCP option 6, default 10.0.2.3). 8-entry cache. `TAG_DNS_RESOLVE` (7) IPC with 48-byte hostname. `libquark::net::dns_resolve()` client API. `httpget` and `ping` accept hostnames via DNS fallback. 3-second query timeout.

21. ~~**Task listing (`ps`)**~~ — **Done.** `sys_task_info(tid)` (syscall 105) returns task state, UID, and parent TID. `ps` command lists all running tasks.

22. ~~**Filesystem permissions / ext2 driver**~~ — **Done.** VFS supports ext2 filesystem alongside FAT32 with auto-detection (superblock magic 0xEF53). ext2 provides native uid/gid/mode permission enforcement: VFS queries sender UID/GID via `sys_get_tuid()` and checks against inode permissions on open/write/create. Root (UID 0) bypasses all checks. Read-write support with block/inode allocation, indirect blocks, directory creation. New modules: `ext2.rs` (on-disk structures, inode/block operations), `ext2_dir.rs` (directory entries, path resolution), `ext2_alloc.rs` (bitmap allocation). FAT32 remains permissionless for backward compatibility. `ERR_PERMISSION(8)` error code added to VFS and libquark.

23. ~~**`shutdown` program**~~ — **Done.** Userspace utility (`user/shutdown`) for clean system shutdown. Phase 1: sends `SIG_TERM` to all user tasks (skip TID 0-1 and self), waits 2.5s for graceful exit. Phase 2: `SIG_KILL` any survivors. Phase 3: ACPI S5 power-off via PM1a_CNT port (0x604 / 0xB004). `-f` flag skips graceful phase and sends `SIG_KILL` immediately. Init grants `CAP_TASK_MGMT` + `CAP_IOPORT` (ports 0x604, 0xB004).
