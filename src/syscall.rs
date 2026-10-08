/// Syscall interface for the Quark microkernel.
///
/// Uses `syscall`/`sysret` via STAR/LSTAR/SFMASK MSRs.
/// Convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4, R9=arg5.
/// Return value in RAX.

use crate::{console, paging, scheduler};

const MSR_STAR: u32 = 0xC000_0081;
const MSR_LSTAR: u32 = 0xC000_0082;
const MSR_SFMASK: u32 = 0xC000_0084;
const MSR_EFER: u32 = 0xC000_0080;

const KERNEL_CS: u64 = 0x08;

// Syscall numbers are assigned in 16-slot blocks, one subsystem per block, so
// a new call lands beside its relatives instead of in whatever gap is free.
// The unused slots in each block are reserved for that subsystem.
//
//   0x00  0-15     process lifecycle
//   0x10  16-31    IPC
//   0x20  32-47    memory
//   0x30  48-63    shared memory
//   0x40  64-79    file descriptors and pipes
//   0x50  80-95    capabilities
//   0x60  96-111   task lifecycle and identity
//   0x70  112-127  hardware and drivers
//   0x80  128-143  synchronisation
//   0x90  144-159  time
//   0xA0  160-175  kernel debug console
//   0xB0  176-191  sockets
//   0xC0  192-207  memory, continued: reservations and memory objects
//   0xD0  208-223  terminals
//   0xE0  224-239  descriptors, continued: flags, and descriptors a server serves
//   0xF0  240-255  ABI introspection
//
// Numbers are stable within an ABI version: they are never reused, and a
// withdrawn call leaves its slot empty rather than being backfilled. See
// `docs/abi.md` for the full contract.

// --- 0x00  process lifecycle ---
pub const SYS_EXIT: u64 = 0;
/// Exit with a status. Separate from SYS_EXIT because `syscall0` leaves RDI
/// undefined, so the existing zero-argument SYS_EXIT cannot grow an argument.
pub const SYS_EXIT_CODE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_WAIT: u64 = 4;
pub const SYS_TASK_KILL: u64 = 5;
pub const SYS_SIGNAL: u64 = 6;
pub const SYS_TASK_INFO: u64 = 7;
/// End the caller's whole program: every task in its address space, with one
/// status. `SYS_EXIT_CODE` ends the calling task and leaves its siblings.
pub const SYS_EXIT_PROGRAM: u64 = 8;
/// Read or set the permission bits the caller's program leaves off what it
/// makes. Kept for it across `SYS_FORK` and `SYS_EXEC_SPACE`.
pub const SYS_UMASK: u64 = 9;
/// `SYS_WAIT` for one child in particular, or without waiting.
pub const SYS_WAIT_FOR: u64 = 10;
/// SYS_WAIT_FOR: answer 0 rather than wait for a child that has not ended.
const WAIT_NO_WAIT: u64 = 1;
/// SYS_WAIT_FOR: the child is named by its process id, there and back.
const WAIT_BY_PID: u64 = 2;
/// SYS_WAIT_FOR: answer for a child that has stopped, or been continued, as
/// well as for one that has ended.
const WAIT_STOPPED: u64 = 4;
const WAIT_CONTINUED: u64 = 8;
/// SYS_WAIT_FOR: what is named is a process group of children.
const WAIT_GROUP: u64 = 16;
/// What the caller's program does about a signal: default, ignore, or run a
/// handler of its own.
pub const SYS_SIG_ACTION: u64 = 11;
/// Raise a signal for the program a task belongs to.
pub const SYS_SIG_RAISE: u64 = 12;
/// The signals raised for the caller's program that it has a handler for.
pub const SYS_SIG_TAKE: u64 = 13;
/// The process id of the program a task belongs to: a number never reused.
pub const SYS_PID: u64 = 14;
/// SYS_SIG_RAISE: the program is named by its process id.
const RAISE_BY_PID: u64 = 1;
/// SYS_SIG_RAISE: what is named is a process group, and the signal is for
/// every program in it.
const RAISE_GROUP: u64 = 2;
/// SYS_SIG_RAISE: the signal is for the task named and no other — its
/// handler is run there, and held back there it waits there.
const RAISE_THREAD: u64 = 4;
/// Have SIGALRM raised for the caller's program after a time.
pub const SYS_SIG_ALARM: u64 = 15;
/// SYS_SIG_ALARM: say how the alarm stands and change nothing.
const ALARM_ASK: u64 = 1;

// --- 0x10  IPC ---
pub const SYS_SEND: u64 = 16;
pub const SYS_RECV: u64 = 17;
pub const SYS_CALL: u64 = 18;
pub const SYS_REPLY: u64 = 19;
pub const SYS_CALL_TIMEOUT: u64 = 20;
pub const SYS_RECV_TIMEOUT: u64 = 21;
pub const SYS_NOTIFY: u64 = 22;
/// `SYS_CALL`, lending the task called a buffer until it replies.
pub const SYS_CALL_LEND: u64 = 23;
/// `SYS_CALL`, offering the task called a copy of one capability, which it
/// may take with `SYS_CAP_TAKE` until it replies.
pub const SYS_CALL_OFFER: u64 = 24;
/// Copy out of, or into, a buffer a caller lent with the call being served.
pub const SYS_LENT_READ: u64 = 25;
pub const SYS_LENT_WRITE: u64 = 26;
/// `SYS_CALL` with any of a buffer lent, a capability offered and a deadline,
/// described by a [`CallWith`] the caller points at.
pub const SYS_CALL_WITH: u64 = 27;

/// What goes with a `SYS_CALL_WITH`, as the caller lays it out.
#[repr(C)]
#[derive(Clone, Copy)]
struct CallWith {
    /// A buffer to lend and its length with the `LEND_*` access bits, as
    /// `SYS_CALL_LEND` takes them; `len_access` 0 lends nothing.
    buf: u64,
    len_access: u64,
    /// The slot to offer, or `u64::MAX` for none.
    offer: u64,
    /// Ticks to wait for the reply; 0 waits for ever.
    ticks: u64,
}

// --- 0x20  memory ---
pub const SYS_MMAP: u64 = 32;
pub const SYS_MUNMAP: u64 = 33;
pub const SYS_PHYS_ALLOC: u64 = 34;
pub const SYS_PHYS_FREE: u64 = 35;
pub const SYS_ADDRSPACE_CREATE: u64 = 36;
pub const SYS_ADDRSPACE_MAP: u64 = 37;
pub const SYS_MAP_PHYS: u64 = 38;
pub const SYS_SET_MEM_LIMIT: u64 = 39;
pub const SYS_SET_PAGER: u64 = 40;
/// The caller's own address space. Threads need it to start a task in the
/// address space they are already running in; it grants nothing, since the
/// caller is executing there either way.
pub const SYS_ADDRSPACE_SELF: u64 = 41;
/// Move pages of the caller's own memory into an address space it made. They
/// become that address space's, and go when it does.
pub const SYS_ADDRSPACE_GIVE: u64 = 43;

// --- 0x30  shared memory ---
pub const SYS_MMAP_FD: u64 = 42;
pub const SYS_SHMEM_CREATE: u64 = 48;
pub const SYS_MEMFD_CREATE: u64 = 53;
/// Give memory named by a descriptor a new size. This is `ftruncate`, and the
/// reason it exists is that every Wayland client makes its buffer pool that
/// way: `memfd_create` then `ftruncate` then `mmap`.
pub const SYS_MEMFD_TRUNCATE: u64 = 54;
pub const SYS_SHMEM_MAP: u64 = 49;
pub const SYS_SHMEM_UNMAP: u64 = 50;
pub const SYS_SHMEM_GRANT: u64 = 51;
pub const SYS_SHMEM_DESTROY: u64 = 52;

// --- 0x40  file descriptors and pipes ---
pub const SYS_FD_READ: u64 = 64;
pub const SYS_FD_WRITE: u64 = 65;
pub const SYS_FD_READ_NB: u64 = 66;
pub const SYS_FD_SET: u64 = 67;
pub const SYS_FD_DUP: u64 = 68;
pub const SYS_PIPE_CREATE: u64 = 69;
pub const SYS_PIPE_FD_SET: u64 = 70;
pub const SYS_FD_CLOSE: u64 = 71;
pub const SYS_SOCKETPAIR: u64 = 72;
pub const SYS_FD_SEND: u64 = 73;
pub const SYS_FD_RECV: u64 = 74;
pub const SYS_POLLSET_CREATE: u64 = 75;
pub const SYS_POLLSET_CTL: u64 = 76;
pub const SYS_POLLSET_WAIT: u64 = 77;
pub const SYS_POLL: u64 = 78;
/// SYS_POLLSET_CTL's op: say why it was refused, with a small number
/// (`pollset::Refused`), rather than all ones.
const POLLSET_WHY: u64 = 1 << 8;
pub const SYS_FD_WRITE_NB: u64 = 79;

// --- 0x50  capabilities ---
pub const SYS_CAP_MINT: u64 = 80;
pub const SYS_CAP_GRANT: u64 = 81;
pub const SYS_CAP_REVOKE: u64 = 82;
pub const SYS_CAP_INSPECT: u64 = 83;
pub const SYS_CAP_DELETE: u64 = 84;
pub const SYS_CAP_TRANSFER: u64 = 85;
pub const SYS_GRANT_CAP: u64 = 86;
pub const SYS_GRANT_IOPORT: u64 = 87;
pub const SYS_GRANT_IRQ: u64 = 88;
pub const SYS_SET_USER_CAPS: u64 = 89;
pub const SYS_GET_USER_CAPS: u64 = 90;
/// Take the capability offered with the call being served.
pub const SYS_CAP_TAKE: u64 = 91;
/// One slot of a task's CSpace, whole: type, both parameters, and whether it
/// is still valid. `SYS_CAP_INSPECT` truncates the parameters to sixteen bits
/// and reads only the caller's own.
pub const SYS_CAP_READ: u64 = 92;

/// The destination slot for `SYS_CAP_GRANT` and `SYS_CAP_TAKE` that means
/// "wherever it fits": the kernel picks one in `cap::RECEIVED` and returns it.
/// Not `u64::MAX`, which is what a failed call returns.
pub const ANY_SLOT: u64 = u64::MAX - 1;

// --- 0x60  task lifecycle and identity ---
pub const SYS_TASK_CREATE: u64 = 96;
pub const SYS_TASK_START: u64 = 97;
pub const SYS_GET_UID: u64 = 98;
pub const SYS_SET_UID: u64 = 99;
pub const SYS_SET_GID: u64 = 100;
pub const SYS_GET_TUID: u64 = 101;
/// Set the calling task's FS base, where its thread-locals live. Per task and
/// self-directed, so it needs no capability: a task can already write any of
/// its own memory.
pub const SYS_SET_FS_BASE: u64 = 102;
/// As SYS_TASK_START, but also places a value in the new task's RDI.
///
/// A thread entry needs its closure, and SYS_TASK_START has nowhere to put
/// one. Added rather than extending SYS_TASK_START, whose callers pass four
/// arguments and would leave the fifth register undefined.
pub const SYS_TASK_START_ARG: u64 = 103;
/// Put a task in a scheduling band. Requires `TaskMgmt` over the target, and
/// refuses to grant a better band than the caller is in itself.
pub const SYS_TASK_PRIORITY: u64 = 105;
pub const SYS_SET_CLEAR_TID: u64 = 106;
/// Which program a task belongs to: its address space's id.
pub const SYS_TASK_SPACE: u64 = 107;
/// Be told when a program's last task has died.
pub const SYS_SPACE_WATCH: u64 = 108;
/// Make a task for an address space the caller created, to start later.
pub const SYS_TASK_CREATE_IN: u64 = 109;
pub const SYS_FORK: u64 = 110;
pub const SYS_EXEC_SPACE: u64 = 111;
pub const SYS_ADDRSPACE_DESTROY: u64 = 44;
/// Block 0xD0: terminals.
pub const SYS_PTY_CREATE: u64 = 208;
pub const SYS_PTY_CTL: u64 = 209;
pub const SYS_PTY_OPEN: u64 = 210;
/// Process groups and sessions: what a shell's jobs are made of.
pub const SYS_PGROUP: u64 = 211;
/// `SYS_PGROUP` operations.
const PGROUP_GET: u64 = 0;
const PGROUP_SET: u64 = 1;
const SESSION_GET: u64 = 2;
const SESSION_NEW: u64 = 3;
/// What a call about groups, sessions or a terminal's answers when the rules
/// say no, as distinct from there being nothing of the kind (`u64::MAX`).
const NOT_ALLOWED: u64 = u64::MAX - 1;
/// Who a task is, continued: block 0x60 is full.
///
/// The groups a task is in besides its own. Anybody may read them; a holder
/// of `SetUid` may set them, for itself or for a child it is preparing.
pub const SYS_GROUPS: u64 = 212;
/// `SYS_GROUPS` operations.
const GROUPS_GET: u64 = 0;
const GROUPS_SET: u64 = 1;
/// A holder of `SetUid` says who a task is — its user, its group and the
/// groups it is in, in one step — where that task is in a call to the
/// holder, or is a child such a task is still preparing.
pub const SYS_IDENTIFY: u64 = 213;
/// What a program was started as: its command line, set and read.
pub const SYS_PROGRAM_NAME: u64 = 214;
/// `SYS_PROGRAM_NAME` operations.
const NAME_SET: u64 = 0;
const NAME_GET: u64 = 1;
/// What a task is called — a thread's name, Linux's `comm` — set and read,
/// with `SYS_PROGRAM_NAME`'s operations.
pub const SYS_TASK_NAME: u64 = 215;
/// Timers, in the time block.
pub const SYS_TIMER_CREATE: u64 = 146;
pub const SYS_TIMER_SET: u64 = 147;
pub const SYS_TIMER_GET: u64 = 148;
/// `SYS_PTY_CTL` operations.
const PTY_GET_TERMIOS: u64 = 0;
const PTY_SET_TERMIOS: u64 = 1;
const PTY_GET_WINSIZE: u64 = 2;
const PTY_SET_WINSIZE: u64 = 3;
const PTY_NUMBER: u64 = 4;
/// Which process group is in front of the terminal, set and asked; making it
/// the caller's controlling terminal; and whose it is.
const PTY_SET_FRONT: u64 = 5;
const PTY_GET_FRONT: u64 = 6;
const PTY_SET_SESSION: u64 = 7;
const PTY_GET_SESSION: u64 = 8;
/// With `PTY_SET_FRONT`, above the group: the caller is not to be stopped
/// for asking from behind. Its runtime says so for a program that has
/// blocked the signal, which the kernel has no way to see.
const PTY_FRONT_QUIETLY: u64 = 1 << 63;

/// Be told when a task dies, so that whatever it was lent can be taken back.
/// Takes no capability: SYS_TASK_INFO already answers the same question by
/// polling, so this only removes the polling.
pub const SYS_TASK_WATCH: u64 = 104;

// --- 0x70  hardware and drivers ---
pub const SYS_IRQ_REGISTER: u64 = 112;
pub const SYS_IRQ_ACK: u64 = 113;
pub const SYS_IOPORT: u64 = 114;
pub const SYS_IOPORT_REP: u64 = 115;
/// Random bytes from the kernel's generator.
pub const SYS_GETRANDOM: u64 = 116;
/// How many processors the system is running on, and which of them the
/// caller was on when it asked.
pub const SYS_CPUS: u64 = 117;
pub const SYS_MSI_ALLOC: u64 = 118;
/// `SYS_PHYS_ALLOC`: frames below four gigabytes.
const PHYS_LOW: u64 = 1;
/// Turn the machine off, or start it again. For a holder of `Power`.
pub const SYS_POWER: u64 = 119;
/// What signals the calling task holds back.
pub const SYS_SIG_MASK: u64 = 120;
/// A handler the kernel ran has finished.
pub const SYS_SIG_RETURN: u64 = 121;
/// A stack for handlers to be run on.
pub const SYS_SIG_STACK: u64 = 122;
/// Take a signal that is waiting, or wait for one, without its handler.
pub const SYS_SIG_WAIT: u64 = 123;
/// What a program, its children or a task has used of the machine.
pub const SYS_USAGE: u64 = 124;
/// How nice a program is to the rest of its band.
pub const SYS_NICE: u64 = 125;
/// How long a program may run.
pub const SYS_CPU_LIMIT: u64 = 126;
/// A device is the caller's program's to drive, and reaches its memory only.
pub const SYS_DEVICE_CLAIM: u64 = 127;
/// `SYS_POWER`: which.
const POWER_OFF: u64 = 0;
const POWER_RESTART: u64 = 1;

// --- 0x80  synchronisation ---
pub const SYS_FUTEX_WAIT: u64 = 128;
pub const SYS_FUTEX_WAKE: u64 = 129;
pub const SYS_FUTEX_WAIT_TIMEOUT: u64 = 130;
/// A counter with a descriptor. In the synchronisation block rather than the
/// descriptor one because that block is full and because this is what it is
/// for: one task adds, another waits. `eventfd`, in one call — the flags a
/// program passes to the Linux call are the argument here.
pub const SYS_EVENT_CREATE: u64 = 131;
/// Where the caller's robust list is: the mutexes it holds that must not
/// stay held if it dies. Linux's `set_robust_list`.
pub const SYS_ROBUST_LIST: u64 = 132;

// --- 0x88  signals, continued again ---
//
// The top half of synchronisation's block: the signals' own two are full.
/// Raise a signal that carries a value — a real-time one queues.
pub const SYS_SIG_QUEUE: u64 = 136;
/// A descriptor read for signals: `signalfd`.
pub const SYS_SIGNAL_FD: u64 = 137;
/// Where the caller's program makes its system calls from: one made
/// anywhere else raises SIGSYS. Linux's syscall user dispatch.
pub const SYS_SYSCALL_TRAP: u64 = 138;

// --- 0x90  time ---
//
// A span of time handed to any call is a count of ticks, hundredths of a
// second, or with its top bit set a count of nanoseconds (`clock::span`).
pub const SYS_TICKS: u64 = 144;
/// Seconds since 1970 when the clock was started, from the CMOS clock; 0 if
/// the machine has none. The time now is this plus `SYS_TICKS / 100`, to
/// the second; `SYS_CLOCK` says it to the nanosecond.
pub const SYS_BOOT_TIME: u64 = 145;
/// What time it is, in nanoseconds: since boot, or since 1970.
pub const SYS_CLOCK: u64 = 149;
/// `SYS_CLOCK`: the time since 1970, rather than since boot.
const CLOCK_WALL: u64 = 1;
/// Say what time it is. For a holder of `Clock`.
pub const SYS_CLOCK_SET: u64 = 150;
/// A program's timer that raises a signal: POSIX's `timer_create` and the
/// rest, by what arg0 says.
pub const SYS_PTIMER: u64 = 151;
const PTIMER_CREATE: u64 = 0;
const PTIMER_SET: u64 = 1;
const PTIMER_GET: u64 = 2;
const PTIMER_DELETE: u64 = 3;

// --- 0xB0  sockets ---
/// Bind a net-server connection handle to a file descriptor.
pub const SYS_SOCK_FD: u64 = 176;
/// Recover the (net_tid, handle) behind a socket fd.
pub const SYS_SOCK_INFO: u64 = 177;
/// A local socket that is nothing yet: to be named and listened on, or
/// connected by a name (`local.rs`).
pub const SYS_SOCKET: u64 = 178;
/// A server names a local socket a task calling it holds.
pub const SYS_SOCKET_BIND: u64 = 179;
/// A named local socket listens.
pub const SYS_SOCKET_LISTEN: u64 = 180;
/// A server connects a local socket a task calling it holds to whatever
/// listens at a name of its own.
pub const SYS_SOCKET_CONNECT: u64 = 181;
/// Take a connection that waits on a listener.
pub const SYS_SOCKET_ACCEPT: u64 = 182;
/// Who is at the other end of a stream.
pub const SYS_SOCKET_PEER: u64 = 183;
/// A socket's option: whether it is told who sent what it receives.
pub const SYS_SOCKET_OPTION: u64 = 184;

/// Operation tags a socket fd sends to the net server. The connection handle
/// rides in the tag's upper 32 bits, because the payload fills every data word
/// and the tag is the only field left to carry it.
pub const TAG_SOCK_WRITE: u64 = 16;
pub const TAG_SOCK_READ: u64 = 17;
const SOCK_HANDLE_SHIFT: u32 = 32;

const fn sock_tag(op: u64, handle: usize) -> u64 {
    op | ((handle as u64) << SOCK_HANDLE_SHIFT)
}

// --- 0xA0  kernel debug console ---
pub const SYS_WRITE: u64 = 160;
pub const SYS_CONSOLE_POS: u64 = 161;

// --- 0xA8  devices: the block 0x70 had no room left for ---
/// What the kernel found of a PCI device the caller holds.
pub const SYS_PCI_DEVICE: u64 = 168;
/// Read a PCI device's configuration.
pub const SYS_PCI_READ: u64 = 169;
/// Write it, less what the kernel keeps.
pub const SYS_PCI_WRITE: u64 = 170;
/// Memory for a display device's screen, nobody's, as a `PhysRange`.
pub const SYS_DISPLAY_MEMORY: u64 = 171;

// --- 0xC0  memory, continued: reservations and memory objects ---
/// Reserve anonymous memory, backed when first touched.
pub const SYS_MAP_ANON: u64 = 192;
/// Free frames in the machine, and pages charged to the caller.
pub const SYS_MEM_INFO: u64 = 193;
/// The most one SYS_MAP_ANON reserves: 512 GiB.
const MAP_ANON_MAX: usize = 1 << 27;
/// SYS_MAP_ANON: give every page its memory now.
const MAP_ANON_POPULATE: u64 = 1;
/// SYS_MAP_ANON: refuse a reservation bigger than the machine's memory, as
/// Linux's overcommit heuristic refuses one without MAP_NORESERVE.
const MAP_ANON_ACCOUNT: u64 = 2;
/// Make a memory object and become its pager.
pub const SYS_OBJECT_CREATE: u64 = 194;
/// Map pages of a memory object a capability names.
pub const SYS_OBJECT_MAP: u64 = 195;
/// A pager's operations on its object.
pub const SYS_OBJECT_CTL: u64 = 196;
/// Have what was written through shared mappings in a range reach the files.
pub const SYS_OBJECT_SYNC: u64 = 197;
/// Give up pages of one's own: have them written out now and their frames
/// given back.
pub const SYS_PAGE_OUT: u64 = 198;
/// SYS_OBJECT_MAP's flags.
const OBJECT_MAP_WRITE: u64 = 1;
const OBJECT_MAP_SHARED: u64 = 2;
const OBJECT_MAP_EXEC: u64 = 4;
/// Objects one pager may have at once, so that no one task fills the table:
/// half of the 2,047 an entry can name. There were 128, of 256.
const OBJECTS_PER_PAGER: usize = 1024;

// --- 0xE0  descriptors, continued ---
/// A server gives the task calling it a descriptor for one of its objects.
pub const SYS_FD_SERVE: u64 = 224;
/// Which server and which of its objects one of the caller's descriptors names.
pub const SYS_FD_SERVED: u64 = 225;
/// A server asks whether a task's program holds a descriptor for an object.
pub const SYS_FD_HOLDS: u64 = 226;
/// A server asks which of its objects one particular descriptor of a task names.
pub const SYS_FD_COOKIE: u64 = 227;
/// Read or set what is true of a descriptor rather than of what it names:
/// whether it is closed when the program becomes another.
pub const SYS_FD_FLAGS: u64 = 228;
/// A server collects an object of its own that no descriptor names any more.
pub const SYS_FD_REAP: u64 = 229;
/// What kind of thing a descriptor names, and whether its other end has gone.
pub const SYS_FD_KIND: u64 = 230;
/// A server gives a task that is calling it an end of the pipe a key of the
/// server's names: how a named pipe is opened.
pub const SYS_FD_SERVE_PIPE: u64 = 231;
/// Wait for somebody to hold the other end of a pipe.
pub const SYS_PIPE_PEER: u64 = 232;
/// A server says what an object of its own is ready for, to a poll.
pub const SYS_FD_READY: u64 = 233;
/// A connected pair whose writes are messages, each read whole:
/// `socketpair` of `SOCK_SEQPACKET`.
pub const SYS_PACKET_PAIR: u64 = 234;
/// A program's descriptor limits: read them, set what it may have, lower how
/// far it may raise that (`RLIMIT_NOFILE`).
pub const SYS_FD_LIMIT: u64 = 235;
/// The first task at or past a number: how every task is found, one call a
/// task, now that there are 32,768 numbers to find them among.
pub const SYS_TASK_NEXT: u64 = 236;
/// Wake some of a futex word's waiters and move the rest to wait on another:
/// Linux's `FUTEX_REQUEUE`, and with a value to compare, `FUTEX_CMP_REQUEUE`.
pub const SYS_FUTEX_REQUEUE: u64 = 237;
/// How one task is scheduled: its niceness, and its class — ordinary, or
/// real-time FIFO or round-robin, at a priority.
pub const SYS_SCHED: u64 = 238;
/// Lock, try to lock, or unlock a priority-inheriting word: Linux's
/// FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI and FUTEX_UNLOCK_PI.
pub const SYS_FUTEX_PI: u64 = 239;
/// `SYS_FD_SERVE`'s flag: the server will say when the object is ready.
const SERVE_SAYS_READY: u64 = 1;
/// SYS_FD_SERVE_PIPE: the writing end, and only if the other end is held.
const SERVE_PIPE_WRITE: u64 = 1;
const SERVE_PIPE_PEER: u64 = 2;
/// SYS_FD_KIND: nothing is left at the other end.
const FD_KIND_GONE: u64 = 0x100;
/// SYS_FD_FLAGS: close this descriptor on `SYS_EXEC_SPACE`.
const FD_FLAG_CLOEXEC: u64 = 1;

// --- 0xF0  ABI introspection ---
pub const SYS_ABI_VERSION: u64 = 240;

/// Current syscall ABI version, as (major << 16) | minor.
///
/// Major changes when a call's meaning or signature changes incompatibly;
/// minor when calls are added. User space can refuse to run against a major it
/// does not know, which is the point of exposing it at all.
pub const ABI_VERSION_MAJOR: u64 = 4;
pub const ABI_VERSION_MINOR: u64 = 5;

/// How many tasks a program may have with no capability at all: its own, and
/// the children it has made and not collected, by `SYS_TASK_CREATE`,
/// `SYS_TASK_CREATE_IN` and `SYS_FORK` alike. A browser's or a compiler's
/// threads, and an eighth of the machine's tasks: one program is refused
/// long before the machine is full. `TaskMgmt` lifts it, which is what a
/// spawner holds.
///
/// It was sixteen, counted for each task that made them and not for its
/// program, with a fork not counted at all.
pub const A_PROGRAMS_TASKS: usize = 4_096;

/// Call itself until the kernel stack has run out (`SYS_MEM_INFO` 6, in a
/// kernel built with `stacktest`): each call keeps half a kilobyte, and adds
/// to what the next returns, so that none of it can be left out.
#[cfg(feature = "stacktest")]
#[inline(never)]
fn run_out_of_stack(depth: u64) -> u64 {
    let mut kept = [0u8; 512];
    kept[(depth % 512) as usize] = depth as u8;
    core::hint::black_box(&mut kept);
    run_out_of_stack(depth + 1).wrapping_add(kept[(depth % 512) as usize] as u64)
}

/// May `caller` set `tid` up?
///
/// A task it created and has not started is its own to fill: nobody else can
/// name it, it holds nothing, and it cannot run. That is the window a spawner
/// works in — make an address space, put the program in it, wire the
/// descriptors, hand over the capabilities, start it — and it needs authority
/// over nobody, because until the last step there is nobody there.
///
/// It is also strictly less than `fork`, which hands a child every capability
/// and every descriptor the caller holds and asks for nothing at all. A rule
/// that let a program copy itself but not build a smaller child would be
/// pushing programs towards the bigger hammer.
///
/// Once it starts, it is a task like any other and touching it needs
/// `TaskMgmt`.
fn may_prepare(caller: usize, tid: usize) -> bool {
    if tid == caller {
        return false;
    }
    unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => t.parent_tid == caller && t.cr3 == 0,
            None => false,
        }
    }
}









/// What a `syscall` clears of the caller's flags: IF, DF and AC, which are
/// the kernel's (see `idt.rs`); and TF and NT, which were the program's to
/// set and the kernel's to fall over. With TF, single-stepping, every
/// instruction of the call trapped in ring 0, which is a kernel fault. With
/// NT, the `iretq` a signal handler's return goes out by faulted in ring 0:
/// a handler that set it and returned halted the machine. `sysret` gives
/// the program its own flags back.
const SFMASK_VALUE: u64 = (1 << 8) | (1 << 9) | (1 << 10) | (1 << 14) | (1 << 18); // TF | IF | DF | NT | AC

fn read_msr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nostack, nomem)
        );
    }
    (hi as u64) << 32 | lo as u64
}

pub fn write_msr(msr: u32, val: u64) {
    let lo = val as u32;
    let hi = (val >> 32) as u32;
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") lo,
            in("edx") hi,
            options(nostack, nomem)
        );
    }
}

unsafe extern "C" {
    fn syscall_entry();
}

/// Initialize the syscall/sysret mechanism.
///
/// # Safety
/// Must be called after GDT is set up with user segments.
pub unsafe fn init() {
    unsafe { init_processor() };
    console::puts(b"Syscall/sysret initialized.\n");
}

/// What `syscall` does is said in MSRs, and a processor has its own: every
/// one that will run a program is told the same.
///
/// # Safety
/// As [`init`], on the processor being set up.
pub unsafe fn init_processor() {
    let efer = read_msr(MSR_EFER);
    write_msr(MSR_EFER, efer | 1); // SCE

    // STAR[47:32] = kernel CS for syscall, STAR[63:48] = the base sysret does
    // arithmetic on: CS = base + 16, SS = base + 8.
    //
    // The base carries RPL 3 itself rather than relying on the CPU to add it.
    // Intel's SYSRET ORs 3 into both selectors; **AMD's does not do so for
    // SS**, so a base of 0x20 gives CS = 0x33 and SS = 0x28 — a stack selector
    // with RPL 0 while running at CPL 3. Nothing complains until a hardware
    // interrupt arrives in user mode: the CPU pushes that SS, and the `iretq`
    // returning to ring 3 faults because the return SS and CS disagree about
    // privilege. #GP inside the kernel, on an AMD machine only, at whatever
    // moment a timer tick happened to land after a system call.
    //
    // 0x23 is not a selector anything loads — only the number sysret adds to.
    // GDT: [0x28] = user data, [0x30] = user code, so 0x23 + 8 = 0x2B and
    // 0x23 + 16 = 0x33, both already RPL 3, on either vendor.
    let star = (0x0023_u64 << 48) | (KERNEL_CS << 32);
    write_msr(MSR_STAR, star);

    write_msr(MSR_LSTAR, syscall_entry as *const () as u64);
    write_msr(MSR_SFMASK, SFMASK_VALUE);
}

const USER_ADDR_LIMIT: u64 = paging::USER_ADDR_LIMIT;

#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Validate that a user pointer range is entirely in user space *and* actually
/// mapped in the calling task's address space.
///
/// The range check alone is not enough. The kernel dereferences user pointers
/// directly (it runs on the caller's CR3), so an in-range but unmapped address
/// faults inside the kernel — frequently with a spin lock held and interrupts
/// disabled, where the fault path cannot safely reschedule.
///
/// `write` additionally requires the pages be writable, so a syscall cannot be
/// tricked into writing through a read-only user mapping.
fn validate_user_range(addr: u64, len: u64, write: bool) -> bool {
    if len == 0 {
        return true;
    }
    if addr == 0 {
        return false;
    }
    match addr.checked_add(len) {
        Some(end) if end <= USER_ADDR_LIMIT => {}
        _ => return false,
    }
    let cr3 = paging::read_cr3();
    // Reserved pages are given their memory before the kernel touches them,
    // and pages that were written out are brought back.
    let ok = unsafe {
        paging::back_range(cr3, addr, len, write).is_ok() && paging::user_range_accessible(cr3, addr, len, write)
    };
    if ok {
        // And they stay, until this call returns: it may wait, and then
        // copy with a lock held, where a page that had gone in the
        // meantime could not be waited for (`reclaim.rs`).
        scheduler::pin(addr, len);
    }
    ok
}

/// Read-only user buffer check.
/// `SYS_FD_RECV`'s `at` when the caller wants any free descriptor rather than
/// a particular one.
pub const ANY_FD: u64 = u64::MAX - 1;

/// `SYS_FD_SEND` and `SYS_FD_RECV` flag: return rather than park.
///
/// This is `MSG_DONTWAIT`, and it is not an optimisation. libwayland reads in
/// a loop until a read says there is nothing left, so a receive that parks on
/// an empty stream never returns and the client hangs holding data it has
/// already been given.
pub const FD_DONTWAIT: u64 = 1;
/// `SYS_FD_SEND` and `SYS_FD_RECV`: arg3 is where an array of descriptors
/// is, `u32`s, as many as bits 8 to 15 of the flags say — to send, or room
/// to write those received.
pub const FD_MANY: u64 = 2;
/// The most one send carries, and one receive takes: as many as the eight
/// bits that say how many can say. Linux's is 253 (`SCM_MAX_FD`). It was 32.
const FD_MANY_MOST: usize = 255;

/// The set a descriptor names, or `None` if it names something else.
fn pollset_of(tid: usize, fd: usize) -> Option<usize> {
    if fd >= crate::task::FD_MOST {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        crate::task::FdKind::PollSet { set } => Some(set),
        _ => None,
    }
}

/// The stream and end a descriptor names, or `None` if it names something else.
fn stream_end_of(tid: usize, fd: usize) -> Option<(usize, u8)> {
    if fd >= crate::task::FD_MOST {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        crate::task::FdKind::StreamEnd { stream, end } => Some((stream, end)),
        _ => None,
    }
}

fn validate_user_ptr(addr: u64, len: u64) -> bool {
    validate_user_range(addr, len, false)
}

/// Writable user buffer check.
pub(crate) fn validate_user_ptr_mut(addr: u64, len: u64) -> bool {
    validate_user_range(addr, len, true)
}

/// Authority to map a physical range into a page table.
///
/// Owning the frames is sufficient. `sys_phys_alloc` records the caller as
/// their owner, so handing back memory the allocator just gave you conveys no
/// authority you did not already hold. That is what almost every mapper is
/// doing: init, the shell and login allocate a frame, map it to stage an ELF
/// page or a stack, then map it into the child.
///
/// A `PhysRange` capability is therefore only needed for frames the allocator
/// never owned — device MMIO and the framebuffer — and for a page another
/// task allocated and passed over IPC for DMA. Separating the two is what lets
/// those grants be narrow, instead of the blanket 0-4 GiB that every mapper
/// previously had to hold.
fn may_map_phys(tid: usize, phys: usize, pages: usize) -> bool {
    crate::pmm::owns_range(phys, pages, tid) || crate::cap::task_has_phys_range(tid, phys, pages)
}

/// Report an IPC destination the caller lacks an Endpoint capability for.
///
/// Bounded: serial output busy-waits on the UART, so a task looping on a
/// forbidden destination could otherwise stall the machine by spamming this.
/// The first few reports are what matter when diagnosing a policy gap.
fn deny_ipc(caller: usize, dest: usize, what: &[u8]) -> u64 {
    const MAX_REPORTS: u32 = 32;
    static REPORTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    if REPORTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed) >= MAX_REPORTS {
        return u64::MAX;
    }

    crate::serial::puts(b"[cap] tid ");
    crate::serial::put_usize(caller);
    crate::serial::puts(b" denied ");
    crate::serial::puts(what);
    crate::serial::puts(b" -> tid ");
    crate::serial::put_usize(dest);
    crate::serial::puts(b"\n");
    u64::MAX
}

/// Unmap `pages` pages starting at `vaddr`, returning owned frames to the PMM.
/// Used to roll back a partially completed mapping loop.
fn unmap_range_owned(cr3: usize, vaddr: usize, pages: usize) {
    for i in 0..pages {
        unsafe { paging::unmap_page_owned(cr3, vaddr + i * 4096) };
    }
}

/// Maximum bytes per IPC write message (5 data words × 8 bytes).
const FD_WRITE_MAX_CHUNK: usize = 40;

/// Whether the task an IPC descriptor was set to is still the one with
/// `target_tid`. Asked before every call: the target can die while a write
/// waits for an answer, and its number be given to the next task made.
fn ipc_target_is(target_tid: usize, endpoint: u64) -> bool {
    endpoint != 0 && crate::cap::endpoint_of(target_tid) == endpoint
}

/// Send a write via IPC to a service, chunking data into 40-byte messages.
/// Returns bytes written.
fn fd_write_ipc(target_tid: usize, endpoint: u64, tag: u64, ptr: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    let mut offset = 0usize;
    while offset < len {
        if !ipc_target_is(target_tid, endpoint) {
            return offset as u64;
        }
        let chunk = (len - offset).min(FD_WRITE_MAX_CHUNK);

        // Snapshot this chunk while the SMAP window is open, then close it
        // before doing anything that can block.
        let mut staged = [0u8; FD_WRITE_MAX_CHUNK];
        {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe {
                core::ptr::copy_nonoverlapping(ptr.add(offset), staged.as_mut_ptr(), chunk);
            }
        }

        // Pack bytes into data[1..6]
        let mut data = [0u64; 6];
        data[0] = chunk as u64;
        for i in 0..5 {
            let base = i * 8;
            let mut w = [0u8; 8];
            for j in 0..8 {
                if base + j < chunk {
                    w[j] = staged[base + j];
                }
            }
            data[i + 1] = u64::from_le_bytes(w);
        }
        let msg = crate::ipc::Message {
            sender: 0,
            tag,
            data,
        };
        match crate::ipc::sys_call(target_tid, &msg) {
            Ok(_) => {}
            Err(_) => return offset as u64,
        }
        offset += chunk;
    }
    len as u64
}

/// Send a read request via IPC to a service, copy response into user buffer.
/// Returns bytes read, or u64::MAX on error.
fn fd_read_ipc(target_tid: usize, endpoint: u64, tag: u64, ptr: *mut u8, max_len: usize) -> u64 {
    if !ipc_target_is(target_tid, endpoint) {
        return u64::MAX;
    }
    let request_len = max_len.min(FD_WRITE_MAX_CHUNK);
    let msg = crate::ipc::Message {
        sender: 0,
        tag,
        data: [request_len as u64, 0, 0, 0, 0, 0],
    };
    match crate::ipc::sys_call(target_tid, &msg) {
        Ok(reply) => {
            let actual = (reply.data[0] as usize).min(request_len);
            // Unpack bytes from reply.data[1..6] into a staging buffer, then
            // copy out under a short SMAP window.
            let mut staged = [0u8; FD_WRITE_MAX_CHUNK];
            for i in 0..5 {
                let base = i * 8;
                let bytes = reply.data[i + 1].to_le_bytes();
                for j in 0..8 {
                    if base + j < actual {
                        staged[base + j] = bytes[j];
                    }
                }
            }
            {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    core::ptr::copy_nonoverlapping(staged.as_ptr(), ptr, actual);
                }
            }
            actual as u64
        }
        Err(_) => u64::MAX,
    }
}

/// Called from assembly with 6 args mapped from user registers.
///
/// This is the way into the kernel from a program, and the way back out:
/// the kernel lock is taken before anything is looked at and given up when
/// there is an answer (`klock.rs`). Interrupts are off on arrival — `SFMASK`
/// saw to that — and are turned on only once the lock is held; they are off
/// again before it is let go, and stay off through the stub's `sysretq`.
///
/// A call that does not come back this way gives the lock up where it
/// leaves: `exec` in `enter_usermode`. One that ends the task never does,
/// and the lock goes on with the processor to whatever it runs next.
#[unsafe(no_mangle)]
extern "C" fn syscall_dispatch(
    nr: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> u64 {
    // What the caller had in R9, which the stub kept: interrupts have been
    // off since, so it is this call's.
    let r9 = crate::percpu::syscall_r9();
    crate::klock::acquire();
    unsafe { crate::usage::entered(scheduler::current_tid()) };
    // It may have waited at the door for that, and whoever had the lock may
    // have ended this task or stopped it. One that was ended makes no call.
    scheduler::arrived();
    unsafe { core::arch::asm!("sti", options(nostack, nomem)) };
    // A call made from where its program has said none is made is not made:
    // the task goes to its handler for SIGSYS instead (`SYS_SYSCALL_TRAP`).
    let answer = match crate::signal::trap_call(nr, [arg0, arg1, arg2, arg3, arg4, r9]) {
        Some(answer) => answer,
        None => dispatch(nr, arg0, arg1, arg2, arg3, arg4),
    };
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    // What the call checked of its program's memory is its to lose again.
    scheduler::unpin();
    // A real-time task it made ready, better placed than itself: it runs
    // now, not at the next tick.
    scheduler::preempt_if_asked();
    // A handler the kernel runs is run on the way out: the task goes back
    // to the handler, with where it was going on its stack.
    let answer = crate::signal::leaving_call(answer);
    unsafe { core::arch::asm!("cli", options(nostack, nomem)) };
    // Never a `sysret` to what is not a program's address. Every way the
    // frame's RIP is set says so already — the call's own return, a handler's
    // entry, a program's start — and this is where a new one that forgot
    // would make Intel's `sysret` fault in ring 0 on the program's stack.
    if scheduler::current_user_frame_mut().is_some_and(|f| f.rip >= USER_ADDR_LIMIT) {
        scheduler::exit_program(-11);
    }
    unsafe { crate::usage::leaving(scheduler::current_tid()) };
    crate::klock::release();
    answer
}

/// The system calls.
fn dispatch(
    nr: u64,
    arg0: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
) -> u64 {
    match nr {
        SYS_EXIT => {
            scheduler::exit()
        }
        SYS_EXIT_CODE => {
            // The low eight bits, as Linux's wait status keeps them. A
            // negative status is how *this* kernel says a task was killed by
            // a signal, so a task that could set one would be able to claim
            // it had been — and every program that reads a child's status
            // would believe it.
            scheduler::exit_with((arg0 & 0xFF) as i32)
        }
        SYS_EXIT_PROGRAM => {
            // The low eight bits, for the reason `SYS_EXIT_CODE` keeps only
            // them. This took what it was given whole, so a program whose
            // `main` returned -1 — which is how a C program says it failed —
            // was reported as ended by signal 1, and one that wanted to be
            // believed killed had only to say so.
            scheduler::exit_program((arg0 & 0xFF) as i32);
        }
        SYS_UMASK => {
            // arg0 = the new mask, or u64::MAX to leave it. Returns the old.
            let new = if arg0 == u64::MAX { None } else { Some(arg0 as u16) };
            crate::fdtable::umask(scheduler::current_tid(), new) as u64
        }
        SYS_FD_READY => {
            // arg0 = a cookie of the caller's, an object it made saying it
            // would say when it is ready; arg1 = what it is ready for, a
            // poll's bits.
            if crate::served::set_ready(scheduler::current_tid(), arg0, arg1 as u32 & 0x7) { 0 } else { u64::MAX }
        }
        SYS_FD_SERVE => {
            // arg0 = the client, arg1 = cookie, arg2 = where: a number,
            // ANY_FD for the lowest free from 3, or the working directory's;
            // arg3 = flags: SERVE_SAYS_READY, that the caller will say when
            // the object is ready rather than its always being, as a file is.
            //
            // Putting a descriptor into a task is handing it something, and
            // the rule is the one a capability grant has: the task consents by
            // being in a call to the server that is doing it.
            let me = scheduler::current_tid();
            let client = arg0 as usize;
            if client == me || !crate::ipc::is_calling(client, me) {
                return u64::MAX;
            }
            let obj = match crate::served::create(me, arg1, arg3 & SERVE_SAYS_READY != 0) {
                Some(o) => o,
                None => return u64::MAX,
            };
            let kind = crate::task::FdKind::Served { obj };
            let cwd = crate::fdtable::FD_CWD;
            let placed = if arg2 == ANY_FD {
                crate::fdtable::install(client, kind, 3)
            } else if arg2 == cwd as u64 {
                // The directory a program is in is replaced, not refused:
                // that is what `chdir` is.
                match crate::fdtable::replace(client, cwd, kind) {
                    Ok(old) => {
                        if !old.is_empty() {
                            crate::pipe::release_fd(&old);
                        }
                        Some(cwd)
                    }
                    Err(()) => None,
                }
            } else if (arg2 as usize) < crate::task::FD_MOST
                && crate::fdtable::get(client, arg2 as usize).is_empty()
            {
                // A number of the server's choosing has to be free. It closes
                // nothing of the client's that the client did not ask closed.
                scheduler::set_fd(client, arg2 as usize, kind).ok().map(|_| arg2 as usize)
            } else {
                None
            };
            match placed {
                Some(fd) => fd as u64,
                None => {
                    crate::served::discard(obj);
                    u64::MAX
                }
            }
        }
        SYS_FD_SERVED => {
            // arg0 = one of the caller's descriptors, arg1 = two words out:
            // the server's TID and its cookie.
            let fd = arg0 as usize;
            if (fd != crate::fdtable::FD_CWD && fd >= crate::task::FD_MOST) || !validate_user_ptr_mut(arg1, 16) {
                return u64::MAX;
            }
            let me = scheduler::current_tid();
            let named = match crate::fdtable::get(me, fd) {
                crate::task::FdKind::Served { obj } => crate::served::of(obj),
                _ => None,
            };
            match named {
                Some((server, cookie)) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe {
                        let out = arg1 as *mut u64;
                        core::ptr::write_unaligned(out, server as u64);
                        core::ptr::write_unaligned(out.add(1), cookie);
                    }
                    0
                }
                None => u64::MAX,
            }
        }
        SYS_FD_HOLDS => {
            // arg0 = a task, arg1 = one of the caller's cookies. Answers only
            // about what the caller serves, so it says nothing about anybody
            // else's descriptors.
            let me = scheduler::current_tid();
            let held = crate::fdtable::any(arg0 as usize, |kind| match kind {
                crate::task::FdKind::Served { obj } => {
                    crate::served::cookie_for(*obj, me) == Some(arg1)
                }
                _ => false,
            });
            held as u64
        }
        SYS_FD_COOKIE => {
            // arg0 = a task, arg1 = one of its descriptors. The caller's
            // cookie there, if what is there is the caller's.
            let me = scheduler::current_tid();
            let fd = arg1 as usize;
            if fd != crate::fdtable::FD_CWD && fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            match crate::fdtable::get(arg0 as usize, fd) {
                crate::task::FdKind::Served { obj } => {
                    crate::served::cookie_for(obj, me).unwrap_or(u64::MAX)
                }
                _ => u64::MAX,
            }
        }
        SYS_FD_REAP => crate::served::reap(scheduler::current_tid()).unwrap_or(u64::MAX),
        SYS_FD_SERVE_PIPE => {
            // arg0 = the client, arg1 = a key of the caller's own choosing,
            // arg2 = which end and how: bit 0 for the writing end, bit 1 to
            // give it only if somebody holds the other.
            //
            // The rule is SYS_FD_SERVE's: a task is handed a descriptor only
            // while it is in a call to whoever is handing it over. What is
            // handed over here is an ordinary pipe's end, of the one pipe the
            // key names for this server while anybody holds an end of it.
            let me = scheduler::current_tid();
            let client = arg0 as usize;
            if client == me || !crate::ipc::is_calling(client, me) {
                return u64::MAX;
            }
            let write = arg2 & SERVE_PIPE_WRITE != 0;
            let server = crate::cap::endpoint_of(me);
            let (pipe, wait) =
                match crate::pipe::open_named(server, arg1, write, arg2 & SERVE_PIPE_PEER != 0) {
                    crate::pipe::Opened::End(pipe, wait) => (pipe, wait),
                    crate::pipe::Opened::NoPeer => return crate::pipe::WOULD_BLOCK,
                    crate::pipe::Opened::Full => return u64::MAX,
                };
            let kind = if write {
                crate::task::FdKind::PipeWrite(pipe)
            } else {
                crate::task::FdKind::PipeRead(pipe)
            };
            // The end was counted before it is in anybody's table: a
            // descriptor is never seen naming an end that is not held.
            match crate::fdtable::install(client, kind, 3) {
                Some(fd) => fd as u64 | wait << 32,
                None => {
                    crate::pipe::drop_ref(pipe, write);
                    u64::MAX
                }
            }
        }
        SYS_PIPE_PEER => {
            // arg0 = a descriptor for one end of a named pipe, arg1 = what
            // came with it when it was given. Waits until somebody opens the
            // other end, if nobody has since then.
            let me = scheduler::current_tid();
            let peer = |handle: usize, is_write: bool| match crate::pipe::wait_peer(handle, is_write, arg1) {
                crate::pipe::Peer::There => 0,
                crate::pipe::Peer::Interrupted => crate::signal::INTERRUPTED,
                crate::pipe::Peer::Failed => u64::MAX,
            };
            // Held while it waits: the descriptor is the program's, and a
            // sibling may close it.
            let done = match crate::fdtable::hold(me, arg0 as usize) {
                crate::task::FdKind::PipeRead(handle) => peer(handle, false),
                crate::task::FdKind::PipeWrite(handle) => peer(handle, true),
                _ => u64::MAX,
            };
            crate::fdtable::unhold(me);
            done
        }
        SYS_FD_KIND => {
            // arg0 = one of the caller's descriptors. What it is, as a number,
            // with a bit above it if the other end has gone: the difference
            // between a write that failed because nobody is reading and one
            // that failed because there is no such descriptor.
            use crate::task::FdKind;
            let (kind, gone) = match crate::fdtable::get(scheduler::current_tid(), arg0 as usize) {
                FdKind::Empty => return u64::MAX,
                FdKind::Ipc { target_tid, endpoint, .. } => (1, !ipc_target_is(target_tid, endpoint)),
                FdKind::PipeRead(handle) => (2, crate::pipe::no_writers(handle)),
                FdKind::PipeWrite(handle) => (3, crate::pipe::no_readers(handle)),
                FdKind::StreamEnd { stream, end } => (4, crate::stream::peer_gone(stream, end)),
                FdKind::PtyEnd { pty, end: 0 } => (5, crate::pty::other_end_gone(pty, 0)),
                FdKind::PtyEnd { pty, end } => (6, crate::pty::other_end_gone(pty, end)),
                FdKind::Timer { .. } => (7, false),
                FdKind::Event { .. } => (8, false),
                FdKind::PollSet { .. } => (9, false),
                FdKind::MemFd { .. } => (10, false),
                FdKind::Socket { net_tid, endpoint, .. } => (11, !ipc_target_is(net_tid, endpoint)),
                FdKind::Served { obj } => (12, crate::served::of(obj).is_none()),
                FdKind::Signals { .. } => (13, false),
                FdKind::Local { .. } => (14, false),
            };
            kind | if gone { FD_KIND_GONE } else { 0 }
        }
        SYS_FD_LIMIT => {
            // arg0 = 0 to read, answering (how far it may go << 32) | what
            // the program may have; 1 to set what it may have to arg1, no
            // higher than how far; 2 to lower how far to arg1, no lower than
            // what it may have. Its own program's, and needs nothing.
            let me = scheduler::current_tid();
            match arg0 {
                0 => crate::fdtable::limit_of(me).map_or(u64::MAX, |(soft, hard)| ((hard as u64) << 32) | soft as u64),
                1 if crate::fdtable::set_soft_limit(me, arg1 as usize) => 0,
                2 if crate::fdtable::lower_hard_limit(me, arg1 as usize) => 0,
                _ => u64::MAX,
            }
        }
        SYS_FD_FLAGS => {
            // arg0 = fd, arg1 = 0 to read or 1 to set, arg2 = the flags
            let fd = arg0 as usize;
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            match arg1 {
                0 => match crate::fdtable::cloexec(tid, fd) {
                    Some(true) => FD_FLAG_CLOEXEC,
                    Some(false) => 0,
                    None => u64::MAX,
                },
                1 => {
                    if crate::fdtable::set_cloexec(tid, fd, arg2 & FD_FLAG_CLOEXEC != 0) {
                        0
                    } else {
                        u64::MAX
                    }
                }
                _ => u64::MAX,
            }
        }
        SYS_SET_CLEAR_TID => {
            // Register a word to clear and wake when a task exits, which is
            // Linux's CLONE_CHILD_CLEARTID and `set_tid_address`. Nothing to
            // check but the address: the word is in the memory of the task
            // that exits, and clearing it harms nobody else.
            //
            // arg1 = whose: the caller's (0), or a task the caller has made
            // and not started. The second is how a thread's creator says it
            // before the thread exists to be asked about: a thread that
            // registered its own word was, until it had run, a child like
            // any other, and a wait that found it so went on waiting for
            // something that would be joined and never waited for.
            let addr = arg0;
            if addr != 0 && (addr >= USER_ADDR_LIMIT || addr % 4 != 0) {
                return u64::MAX;
            }
            let me = scheduler::current_tid();
            let tid = if arg1 == 0 || arg1 == me as u64 {
                me
            } else if arg1 < crate::task::MAX_TASKS as u64 && may_prepare(me, arg1 as usize) {
                arg1 as usize
            } else {
                return u64::MAX;
            };
            match unsafe { scheduler::get_task_mut(tid) } {
                Some(t) => {
                    t.clear_child_tid = addr;
                }
                None => return u64::MAX,
            }
            if addr != 0 {
                scheduler::word_given(tid);
            }
            tid as u64
        }
        SYS_SET_FS_BASE => {
            let base = arg0;
            // Must be a user address: FS is used by user code, and a kernel
            // base would let a task read through it with SMAP inactive.
            if base >= USER_ADDR_LIMIT {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            match unsafe { scheduler::get_task_mut(tid) } {
                Some(t) => {
                    t.fs_base = base;
                    // Effective now, not at the next switch: the caller is
                    // running and will use it before it is scheduled again.
                    crate::cpu::set_fs_base(base);
                    0
                }
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_SELF => {
            match unsafe { scheduler::get_task_mut(scheduler::current_tid()) } {
                Some(t) => t.cr3 as u64,
                None => u64::MAX,
            }
        }
        SYS_TASK_WATCH => {
            // No capability: SYS_TASK_INFO already answers "is that task
            // alive" for anybody, so this only spares the asking.
            match crate::ipc::sys_task_watch(scheduler::current_tid(), arg0 as usize) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_ROBUST_LIST => {
            // arg0 = where the caller's robust list's head is in its memory,
            // 0 for none, u64::MAX to ask. Returns where it was.
            crate::threads::robust_list(scheduler::current_tid(), arg0)
        }
        SYS_TASK_NAME => {
            // arg0 = a task, arg1 = NAME_SET or NAME_GET, arg2 = the name or
            // where it goes, arg3 = its length or the room, arg4 = for a
            // server, a client in a call to it, whose program's task this
            // is to be — 0 for the caller's own. Read by anybody, as `ps`
            // reads it; none is its program's name.
            let caller = scheduler::current_tid();
            let tid = arg0 as usize;
            if tid >= crate::task::MAX_TASKS || tid == 0 || !scheduler::task_is_live(tid) {
                return u64::MAX;
            }
            match arg1 {
                NAME_SET => {
                    let asker = if arg4 == 0 { caller } else { arg4 as usize };
                    if asker != caller && (asker >= crate::task::MAX_TASKS || !crate::ipc::is_calling(asker, caller)) {
                        return u64::MAX;
                    }
                    if scheduler::space_of_task(tid) != scheduler::space_of_task(asker) {
                        return u64::MAX;
                    }
                    let len = (arg3 as usize).min(crate::threads::NAME_MAX);
                    let mut name = [0u8; crate::threads::NAME_MAX];
                    if len > 0 {
                        if !validate_user_ptr(arg2, len as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { core::ptr::copy_nonoverlapping(arg2 as *const u8, name.as_mut_ptr(), len) };
                    }
                    crate::threads::set_name(tid, &name[..len]);
                    0
                }
                NAME_GET => {
                    let (name, len) = crate::threads::name_of(tid);
                    let n = len.min(arg3 as usize);
                    if n > 0 {
                        if !validate_user_ptr_mut(arg2, n as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, n) };
                    }
                    len as u64
                }
                _ => u64::MAX,
            }
        }
        SYS_PROGRAM_NAME => {
            // arg0 = a task, arg1 = NAME_SET or NAME_GET, arg2 = a buffer,
            // arg3 = its length. What its program was started as: the
            // arguments, each ended by a nought. Set by the program itself,
            // or for a child its caller made and has not started — the
            // spawner, which knows what it started — or by a holder of
            // `TaskMgmt`; read by anybody, as `ps` reads it.
            let caller = scheduler::current_tid();
            let tid = arg0 as usize;
            let len = (arg3 as usize).min(crate::fdtable::CMDLINE);
            match arg1 {
                NAME_SET => {
                    let own = tid == caller
                        || (scheduler::space_of_task(tid) == scheduler::space_of_task(caller) && tid != 0);
                    if !own && !may_prepare(caller, tid) && !crate::cap::task_has_task_mgmt(caller, tid) {
                        return u64::MAX;
                    }
                    let mut name = [0u8; crate::fdtable::CMDLINE];
                    if len > 0 {
                        if !validate_user_ptr(arg2, len as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { core::ptr::copy_nonoverlapping(arg2 as *const u8, name.as_mut_ptr(), len) };
                    }
                    if crate::fdtable::set_cmdline(tid, &name[..len]) { 0 } else { u64::MAX }
                }
                NAME_GET => {
                    let mut name = [0u8; crate::fdtable::CMDLINE];
                    let Some(n) = crate::fdtable::cmdline_of(tid, &mut name) else {
                        return u64::MAX;
                    };
                    let n = n.min(len);
                    if n > 0 {
                        if !validate_user_ptr_mut(arg2, n as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), arg2 as *mut u8, n) };
                    }
                    n as u64
                }
                _ => u64::MAX,
            }
        }
        SYS_TASK_SPACE => {
            // No capability, for the reason SYS_TASK_INFO needs none: which
            // program a task belongs to tells nobody anything they could not
            // work out, and every server that keeps things per program needs
            // to ask it of each caller.
            match scheduler::space_of_task(arg0 as usize) {
                0 => u64::MAX,
                space => space,
            }
        }
        SYS_SPACE_WATCH => {
            match crate::ipc::sys_space_watch(scheduler::current_tid(), arg0) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_TASK_PRIORITY => {
            let tid = arg0 as usize;
            let band = arg1;
            let caller = scheduler::current_tid();
            // Asked rather than told: which band the task was put in.
            // Anybody may ask, as anybody may ask what state a task is in.
            if band == u64::MAX {
                return scheduler::base_priority_of(tid).map_or(u64::MAX, |b| b as u64);
            }
            // A child the caller has made and not started is its own to put
            // in a band, as it is its own to fill (`may_prepare`): a spawner
            // with no authority over anybody — the device manager — gives
            // what it starts the band its manifest asks for. Without it, every
            // driver the device manager started ran as an ordinary program,
            // whose memory is taken when memory is short: a disk's driver
            // written out to the disk it drives.
            if !crate::cap::task_has_task_mgmt(caller, tid) && !may_prepare(caller, tid) {
                return u64::MAX;
            }
            // The same rule capabilities follow: a spawner may narrow what it
            // holds and never widen it. A shell running as an ordinary program
            // cannot promote what it starts into a driver band, so a program
            // asking for one gets it only from a spawner that is already there.
            //
            // The band it is in, not the one it runs at: while a better task
            // waits on it — the framebuffer device saying the display has gone
            // — it is lent that one, and a child put in it then kept it for
            // good, in front of every ordinary program and never written out.
            let own = scheduler::base_priority_of(caller).map_or(scheduler::NUM_PRIORITIES, |b| b as usize);
            if band as usize >= scheduler::NUM_PRIORITIES || (band as usize) < own {
                return u64::MAX;
            }
            match scheduler::set_priority(tid, band as u8) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_ABI_VERSION => (ABI_VERSION_MAJOR << 16) | ABI_VERSION_MINOR,
        SYS_YIELD => {
            scheduler::yield_now();
            0
        }
        SYS_WRITE => {
            let ptr = arg0 as *const u8;
            let len = arg1 as usize;
            if len == 0 {
                return 0;
            }
            if !validate_user_ptr(arg0, arg1) {
                return u64::MAX;
            }
            let _ua = crate::cpu::UserAccess::begin();
            let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
            console::puts(slice);
            len as u64
        }
        SYS_CONSOLE_POS => {
            let (row, col) = console::cursor_pos_and_disable();
            ((row as u64) << 32) | (col as u64)
        }
        SYS_GETPID => scheduler::current_tid() as u64,
        SYS_SEND => {
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"send");
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_send(dest, &msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_RECV => {
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr_mut(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv(from) {
                Ok(msg) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *msg_ptr = msg };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL_TIMEOUT => {
            // arg0 = dest, arg1 = msg, arg2 = reply, arg3 = how long to wait.
            // Returns 0 on reply, 1 on timeout, u64::MAX on error — a timeout
            // is an answer ("nobody responded"), not a failure to ask.
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"call");
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size)
                || !validate_user_ptr_mut(arg2, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_call_timeout(dest, &msg, crate::clock::span(arg3)) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *reply_ptr = reply };
                    0
                }
                Err(crate::ipc::IpcError::Timeout) => 1,
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL => {
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"call");
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size)
                || !validate_user_ptr_mut(arg2, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_call(dest, &msg) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *reply_ptr = reply };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL_LEND => {
            // arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = buffer,
            // arg4 = length | LEND_READ | LEND_WRITE
            let caller = scheduler::current_tid();
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(caller, dest) {
                return deny_ipc(caller, dest, b"call");
            }
            let access = arg4 & (crate::lend::LEND_READ | crate::lend::LEND_WRITE);
            let len = (arg4 & crate::lend::LEND_LEN_MASK) as usize;
            if access == 0 || len == 0 || len > crate::lend::LEND_MAX {
                return u64::MAX;
            }
            // Checked now, so that a buffer the caller cannot lend is the
            // caller's error and never reaches the server.
            if !validate_user_range(arg3, len as u64, access & crate::lend::LEND_WRITE != 0) {
                return u64::MAX;
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) || !validate_user_ptr_mut(arg2, msg_size) {
                return u64::MAX;
            }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            let lent = crate::ipc::Lent { addr: arg3 as usize, len, access, frame: false };
            match crate::ipc::sys_call_lend(dest, &msg, lent) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *reply_ptr = reply };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL_OFFER => {
            // arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = slot offered
            let caller = scheduler::current_tid();
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(caller, dest) {
                return deny_ipc(caller, dest, b"call");
            }
            // Checked now, so that offering nothing is the caller's error and
            // never reaches the server. The take checks again: the capability
            // can be revoked while the call waits.
            let slot = arg3 as usize;
            let offered = crate::cap::slot(caller, slot).is_some_and(|c| crate::cap::slot_is_valid(&c));
            if !offered {
                return u64::MAX;
            }
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let reply_ptr = arg2 as *mut crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) || !validate_user_ptr_mut(arg2, msg_size) {
                return u64::MAX;
            }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_call_offer(dest, &msg, slot) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *reply_ptr = reply };
                    0
                }
                Err(_) => u64::MAX,
            }
        }
        SYS_CALL_WITH => {
            // arg0 = dest, arg1 = msg, arg2 = reply out, arg3 = CallWith.
            // Each part is checked as the call that has only that part checks
            // it, and the result is SYS_CALL_TIMEOUT's.
            let caller = scheduler::current_tid();
            let dest = arg0 as usize;
            if !crate::cap::task_has_endpoint(caller, dest) {
                return deny_ipc(caller, dest, b"call");
            }
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            let with_size = core::mem::size_of::<CallWith>() as u64;
            if !validate_user_ptr(arg1, msg_size)
                || !validate_user_ptr_mut(arg2, msg_size)
                || !validate_user_ptr(arg3, with_size)
            {
                return u64::MAX;
            }
            let (msg, with) = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { (*(arg1 as *const crate::ipc::Message), *(arg3 as *const CallWith)) }
            };
            let lent = if with.len_access == 0 {
                None
            } else {
                let access = with.len_access & (crate::lend::LEND_READ | crate::lend::LEND_WRITE);
                let len = (with.len_access & crate::lend::LEND_LEN_MASK) as usize;
                if access == 0 || len == 0 || len > crate::lend::LEND_MAX {
                    return u64::MAX;
                }
                if !validate_user_range(with.buf, len as u64, access & crate::lend::LEND_WRITE != 0) {
                    return u64::MAX;
                }
                Some(crate::ipc::Lent { addr: with.buf as usize, len, access, frame: false })
            };
            let offer = if with.offer == u64::MAX {
                None
            } else {
                let slot = with.offer as usize;
                let valid = crate::cap::slot(caller, slot).is_some_and(|c| crate::cap::slot_is_valid(&c));
                if !valid {
                    return u64::MAX;
                }
                Some(slot)
            };
            match crate::ipc::sys_call_with(dest, &msg, crate::clock::span(with.ticks), lent, offer) {
                Ok(reply) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *(arg2 as *mut crate::ipc::Message) = reply };
                    0
                }
                Err(crate::ipc::IpcError::Timeout) => 1,
                Err(_) => u64::MAX,
            }
        }
        SYS_LENT_READ | SYS_LENT_WRITE => {
            // arg0 = the caller that lent it, arg1 = offset into what it lent,
            // arg2 = this task's buffer, arg3 = length
            let me = scheduler::current_tid();
            // A pager names the task the kernel called for with the pager bit.
            let client = (arg0 & !crate::ipc::PAGER_BIT) as usize;
            let offset = arg1 as usize;
            let local = arg2;
            let len = arg3 as usize;
            let into_lent = nr == SYS_LENT_WRITE;
            if len == 0 {
                return 0;
            }
            if len > crate::lend::COPY_MAX {
                return u64::MAX;
            }
            // Read from when writing into the lent buffer, written to when
            // reading out of it.
            let local_ok = if into_lent {
                validate_user_ptr(local, len as u64)
            } else {
                validate_user_ptr_mut(local, len as u64)
            };
            if !local_ok {
                return u64::MAX;
            }
            // From finding what was lent to the end of the copy is one
            // step, with interrupts off: the lender's pages are found in
            // its tables and reached by their frames, and a thread of the
            // lender run in between could unmap one, and the frame be
            // somebody else's by the time it was written. (A system call
            // had interrupts off when this was written, and said so here
            // long after it had stopped being true.)
            let flags = irq_save();
            let copied = (|| {
                let Some((lent, cr3)) = crate::ipc::lent_to(client, me) else {
                    return false;
                };
                let need = if into_lent { crate::lend::LEND_WRITE } else { crate::lend::LEND_READ };
                if lent.access & need == 0 {
                    return false;
                }
                match offset.checked_add(len) {
                    Some(end) if end <= lent.len => {}
                    _ => return false,
                }
                if lent.frame {
                    unsafe { crate::lend::copy_frame(lent.addr + offset, local as usize, len, into_lent) }
                } else {
                    unsafe { crate::lend::copy(cr3, lent.addr + offset, local as usize, len, into_lent) }
                }
            })();
            irq_restore(flags);
            if copied {
                len as u64
            } else {
                u64::MAX
            }
        }
        SYS_REPLY => {
            let dest = arg0 as usize;
            let msg_ptr = arg1 as *const crate::ipc::Message;
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr(arg1, msg_size) { return u64::MAX; }
            let msg = {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { *msg_ptr }
            };
            match crate::ipc::sys_reply(dest, &msg) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_IRQ_REGISTER => {
            // One of the sixteen ISA interrupts. The numbers above them are
            // given out, not asked for (`SYS_MSI_ALLOC`).
            let irq = arg0 as u8;
            if arg0 >= crate::irq_dispatch::FIRST_MESSAGE as u64
                || !crate::cap::task_has_irq(scheduler::current_tid(), irq)
            {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            crate::irq_dispatch::register_irq_handler(irq, tid);
            crate::intc::enable(irq);
            0
        }
        SYS_IRQ_ACK => {
            // arg0 = IRQ number
            let irq = arg0 as u8;
            // Acking an IRQ you do not own lets any task interfere with the
            // controller's state and stall another driver's interrupts.
            if !crate::cap::task_has_irq(scheduler::current_tid(), irq) {
                return u64::MAX;
            }
            crate::intc::ack(irq);
            0
        }
        SYS_IOPORT => {
            // arg0=port, arg1=op (0=read8,1=write8,2=read16,3=write16,4=read32,5=write32), arg2=value (for writes)
            let port = arg0 as u16;
            // The window every PCI device is configured through is the
            // kernel's, whoever holds the port (`pci::config_port`).
            let width = match arg1 {
                0 | 1 => 1,
                2 | 3 => 2,
                _ => 4,
            };
            if crate::pci::config_port(port, width)
                || !crate::cap::task_has_ioport(scheduler::current_tid(), port)
            {
                return u64::MAX;
            }
            match arg1 {
                0 => unsafe { crate::io::inb(port) as u64 },
                1 => { unsafe { crate::io::outb(port, arg2 as u8) }; 0 }
                2 => unsafe { crate::io::inw(port) as u64 },
                3 => { unsafe { crate::io::outw(port, arg2 as u16) }; 0 }
                4 => unsafe { crate::io::inl(port) as u64 },
                5 => { unsafe { crate::io::outl(port, arg2 as u32) }; 0 }
                _ => u64::MAX,
            }
        }
        SYS_IOPORT_REP => {
            // arg0=port, arg1=user_buf_ptr, arg2=count (words), arg3=op (0=insw, 1=outsw)
            let port = arg0 as u16;
            if crate::pci::config_port(port, 2) || !crate::cap::task_has_ioport(scheduler::current_tid(), port) {
                return u64::MAX;
            }
            let buf = arg1;
            let count = arg2 as usize;
            let op = arg3;
            if count == 0 {
                return 0;
            }
            // insw writes into the buffer, outsw only reads it.
            let bytes = (count as u64).saturating_mul(2);
            let ok = match op {
                0 => validate_user_ptr_mut(buf, bytes),
                1 => validate_user_ptr(buf, bytes),
                _ => return u64::MAX,
            };
            if !ok {
                return u64::MAX;
            }
            let _ua = crate::cpu::UserAccess::begin();
            match op {
                0 => {
                    unsafe { crate::io::rep_insw(port, buf as *mut u16, count) };
                    0
                }
                1 => {
                    unsafe { crate::io::rep_outsw(port, buf as *const u16, count) };
                    0
                }
                _ => u64::MAX,
            }
        }
        SYS_MAP_PHYS => {
            let phys = arg0 as usize;
            let virt = arg1 as usize;
            let pages = arg2 as usize;
            if !paging::user_range_ok(virt, pages) {
                return u64::MAX;
            }
            if phys & 0xFFF != 0 || phys.checked_add(pages * 4096).is_none() {
                return u64::MAX;
            }
            if !may_map_phys(scheduler::current_tid(), phys, pages) {
                return u64::MAX;
            }
            let pml4 = paging::read_cr3();
            // No OWNED bit: these frames belong to a device, not to this
            // address space. Freeing them on unmap/teardown would push MMIO
            // addresses into the frame allocator.
            let flags = paging::PRESENT | paging::WRITABLE | paging::USER;
            for i in 0..pages {
                let p = phys + i * 4096;
                let v = virt + i * 4096;
                if unsafe { paging::map_page(pml4, v, p, flags) }.is_err() {
                    // Roll back the pages mapped so far.
                    for j in 0..i {
                        let _ = unsafe { paging::unmap_page(pml4, virt + j * 4096) };
                    }
                    return u64::MAX;
                }
            }
            0
        }
        SYS_TASK_CREATE => {
            // A thread is not a new principal. A task started in the address
            // space you are already in *is* you: it can read what you can
            // read, call what you can call, and do nothing you could not do
            // yourself. Requiring authority over other tasks in order to make
            // one is the wrong check, and it is why a C program had to be
            // handed the right to kill anything in order to call
            // pthread_create.
            //
            // So this is allowed without a capability and bounded instead. The
            // capability still buys the unbounded form, which is what a
            // spawner needs. What decides whether the new task may actually
            // *run* is SYS_TASK_START, which checks the address space.
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) && scheduler::program_tasks(caller) >= A_PROGRAMS_TASKS {
                return u64::MAX;
            }
            match scheduler::create_empty_task() {
                Some(tid) => tid as u64,
                None => u64::MAX,
            }
        }
        SYS_FORK => {
            // A copy of this task in a copy of this address space, which
            // returns 0 there and the child's id here. No capability: a task
            // may always make a copy of itself, because everything the copy
            // gets is already the caller's — its pages are copied out of the
            // caller's own and charged to the child, and the descriptors and
            // capabilities are the ones the caller holds. Counted as any task
            // a program makes is.
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) && scheduler::program_tasks(caller) >= A_PROGRAMS_TASKS {
                return u64::MAX;
            }
            match scheduler::fork_current() {
                Some(tid) => tid as u64,
                None => u64::MAX,
            }
        }
        SYS_EXEC_SPACE => {
            // arg0 = an address space the caller made and filled, arg1 = where
            // to start in it, arg2 = its stack. The caller becomes the program
            // in it, keeping its id, its descriptors and its capabilities.
            //
            // No capability: everything here is the caller's own. It made the
            // address space, it moved its own pages into it, and what it is
            // replacing is itself.
            match scheduler::exec_into(arg0 as usize, arg1, arg2) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_TASK_CREATE_IN => {
            // arg0 = an address space the caller made. The task belongs to
            // that program before it runs, which is what lets a spawner give
            // it things — a working directory — that servers keep per program.
            let caller = scheduler::current_tid();
            let cr3 = arg0 as usize;
            if !crate::userspace::is_owned_address_space(caller, cr3) {
                return u64::MAX;
            }
            // As `SYS_TASK_CREATE`: allowed without a capability and bounded
            // instead, because a task made in an address space the caller
            // built holds nothing until the caller puts something in it. The
            // capability buys the unbounded form, which is what a spawner
            // that runs many programs needs.
            if !crate::cap::task_has_task_mgmt(caller, 0) && scheduler::program_tasks(caller) >= A_PROGRAMS_TASKS {
                return u64::MAX;
            }
            match scheduler::create_task_in(cr3) {
                Some(tid) => tid as u64,
                None => u64::MAX,
            }
        }
        SYS_TIMER_CREATE => {
            // A descriptor that becomes readable when its deadline passes.
            // Nothing to ask for: it is the caller's own clock, and waiting on
            // it is waiting on a descriptor the caller already holds.
            let tid = scheduler::current_tid();
            let Some(timer) = crate::timerfd::create(tid) else {
                return u64::MAX;
            };
            match scheduler::current_alloc_fd(crate::task::FdKind::Timer { timer }) {
                Ok(fd) => {
                    crate::timerfd::retain(timer);
                    fd as u64
                }
                Err(()) => {
                    crate::timerfd::cleanup_orphans(tid);
                    u64::MAX
                }
            }
        }
        SYS_SOCKET => {
            // arg0 = what kind: 0, a local stream. One that is nothing yet.
            if arg0 != 0 {
                return u64::MAX;
            }
            let Some(l) = crate::local::create() else { return u64::MAX };
            match scheduler::current_alloc_fd(crate::task::FdKind::Local { l }) {
                Ok(fd) => fd as u64,
                Err(()) => {
                    crate::local::release(l);
                    u64::MAX
                }
            }
        }
        SYS_SOCKET_BIND | SYS_SOCKET_CONNECT => {
            // arg0 = a task that is calling the caller; arg1 = its
            // descriptor, a local socket that is nothing yet; arg2 = a key
            // of the caller's choosing — the name, as the caller knows it.
            // The rule is SYS_FD_SERVE's: what a server does to a task's
            // table, it does while the task is in a call to it.
            let me = scheduler::current_tid();
            let client = arg0 as usize;
            let fd = arg1 as usize;
            if client == me || !crate::ipc::is_calling(client, me) {
                return u64::MAX;
            }
            let crate::task::FdKind::Local { l } = crate::fdtable::get(client, fd) else { return u64::MAX };
            let server = crate::cap::endpoint_of(me);
            if nr == SYS_SOCKET_BIND {
                return match crate::local::bind(l, server, arg2) {
                    Ok(()) => 0,
                    Err(crate::local::Refused::Taken) => 1,
                    Err(_) => u64::MAX,
                };
            }
            if !crate::local::unbound(l) {
                return u64::MAX;
            }
            match crate::local::connect(server, arg2, crate::stream::Creds::of(client), client) {
                Ok(stream) => {
                    // The connector's socket is end 0 of the stream now, in the
                    // same slot, close-on-exec mark and all.
                    let end = crate::task::FdKind::StreamEnd { stream, end: 0 };
                    if crate::fdtable::swap_if(client, fd, crate::task::FdKind::Local { l }, end) {
                        crate::local::release(l);
                        0
                    } else {
                        // A sibling closed it in between: whoever accepts
                        // finds nobody at the other end.
                        crate::stream::close_end(stream, 0);
                        u64::MAX
                    }
                }
                Err(crate::local::Refused::Nobody) => 1,
                Err(crate::local::Refused::Full) => crate::pipe::WOULD_BLOCK,
                Err(_) => u64::MAX,
            }
        }
        SYS_SOCKET_LISTEN => {
            // arg0 = a named local socket of the caller's; arg1 = how many
            // connections may wait to be accepted.
            let me = scheduler::current_tid();
            let crate::task::FdKind::Local { l } = crate::fdtable::get(me, arg0 as usize) else { return u64::MAX };
            if crate::local::listen(l, arg1 as usize, crate::stream::Creds::of(me)) { 0 } else { u64::MAX }
        }
        SYS_SOCKET_ACCEPT => {
            // arg0 = a listening socket of the caller's; arg1 = flags (1 =
            // do not wait). A descriptor for the connection, the lowest free
            // from 3. Held while it waits, as a read holds what it reads.
            let me = scheduler::current_tid();
            let crate::task::FdKind::Local { l } = crate::fdtable::hold(me, arg0 as usize) else {
                crate::fdtable::unhold(me);
                return u64::MAX;
            };
            let answer = loop {
                // Somewhere to put it before it is taken: a connection is not
                // thrown away because the table is full.
                if crate::local::readable(l) && scheduler::lowest_free_fd(me).is_none() {
                    break u64::MAX;
                }
                if let Some(stream) = crate::local::take(l) {
                    match crate::fdtable::install(me, crate::task::FdKind::StreamEnd { stream, end: 1 }, 3) {
                        Some(fd) => break fd as u64,
                        None => {
                            crate::stream::close_end(stream, 1);
                            break u64::MAX;
                        }
                    }
                }
                if arg1 & 1 != 0 {
                    break crate::pipe::WOULD_BLOCK;
                }
                if crate::signal::ends_wait(me) {
                    break crate::signal::INTERRUPTED;
                }
                if !crate::local::wait(l) && !crate::local::readable(l) && !crate::signal::ends_wait(me) {
                    // It does not listen.
                    break u64::MAX;
                }
            };
            crate::fdtable::unhold(me);
            answer
        }
        SYS_SOCKET_PEER => {
            // arg0 = a stream of the caller's; arg1 = where to write who is
            // at the other end: three u32s, process id, user, group.
            if !validate_user_ptr_mut(arg1, 12) {
                return u64::MAX;
            }
            let crate::task::FdKind::StreamEnd { stream, end } = crate::fdtable::get(scheduler::current_tid(), arg0 as usize)
            else {
                return u64::MAX;
            };
            let Some(peer) = crate::stream::peer_of(stream, end) else { return u64::MAX };
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(arg1 as *mut [u32; 3], [peer.pid, peer.uid, peer.gid]) };
            0
        }
        SYS_SOCKET_OPTION => {
            // arg0 = a socket of the caller's, a stream or a local socket not
            // yet connected; arg1 = which: 0, to be told who sent what it
            // receives; arg2 = 0 off, 1 on, u64::MAX to ask. Answers what it
            // was.
            let set = match arg2 {
                0 => Some(false),
                1 => Some(true),
                u64::MAX => None,
                _ => return u64::MAX,
            };
            if arg1 != 0 {
                return u64::MAX;
            }
            let was = match crate::fdtable::get(scheduler::current_tid(), arg0 as usize) {
                crate::task::FdKind::StreamEnd { stream, end } => crate::stream::passcred(stream, end, set),
                crate::task::FdKind::Local { l } => crate::local::passcred(l, set),
                _ => None,
            };
            was.map_or(u64::MAX, |on| on as u64)
        }
        SYS_SIGNAL_FD => {
            // arg0 = a signal descriptor of the caller's to change, or
            // u64::MAX for a new one; arg1 = the signals it is read for,
            // less 9 and 19, which are nobody's to read.
            let me = scheduler::current_tid();
            let mask = arg1 & !((1 << 8) | (1 << 18));
            if arg0 != u64::MAX {
                return match crate::fdtable::get(me, arg0 as usize) {
                    crate::task::FdKind::Signals { sfd } => {
                        crate::sigfd::set_mask(sfd, mask);
                        arg0
                    }
                    _ => u64::MAX,
                };
            }
            let Some(sfd) = crate::sigfd::create(mask) else { return u64::MAX };
            match scheduler::current_alloc_fd(crate::task::FdKind::Signals { sfd }) {
                Ok(fd) => fd as u64,
                Err(()) => {
                    crate::sigfd::release(sfd);
                    u64::MAX
                }
            }
        }
        SYS_EVENT_CREATE => {
            // arg0 = the count it starts at, arg1 = flags (1 = semaphore).
            //
            // The descriptor is the counter's only name, so it is installed in
            // the caller's own table here rather than handed back as a handle
            // somebody then has to place.
            let me = scheduler::current_tid();
            let semaphore = arg1 & 1 != 0;
            let ev = match crate::eventfd::create(me, arg0, semaphore) {
                Some(e) => e,
                None => return u64::MAX,
            };
            match scheduler::current_alloc_fd(crate::task::FdKind::Event { ev }) {
                Ok(fd) => {
                    crate::eventfd::retain(ev);
                    fd as u64
                }
                Err(()) => {
                    crate::eventfd::cleanup_orphans(me);
                    u64::MAX
                }
            }
        }
        SYS_TIMER_SET => {
            // arg0 = fd, arg1 = how long until the first expiration (no time
            // disarms), arg2 = how long between them afterwards.
            let tid = scheduler::current_tid();
            let Some(timer) = crate::timerfd::of_fd(tid, arg0 as usize) else {
                return u64::MAX;
            };
            if crate::timerfd::set(timer, crate::clock::span(arg1), crate::clock::span(arg2)) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_TIMER_GET => {
            // arg0 = fd, arg1 = where to write what is left and the interval,
            // in nanoseconds, or 0. Answers with both in ticks, packed as
            // (interval << 32) | until, each rounded up and no more than
            // thirty-two bits of it.
            let tid = scheduler::current_tid();
            let Some(timer) = crate::timerfd::of_fd(tid, arg0 as usize) else {
                return u64::MAX;
            };
            if arg1 != 0 && (arg1 & 7 != 0 || !validate_user_ptr_mut(arg1, 16)) {
                return u64::MAX;
            }
            match crate::timerfd::get(timer) {
                Some((left, interval)) => {
                    if arg1 != 0 {
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { *(arg1 as *mut [u64; 2]) = [left, interval] };
                    }
                    let ticks = |ns: u64| crate::clock::ticks_of(ns).min(u32::MAX as u64);
                    (ticks(interval) << 32) | ticks(left)
                }
                None => u64::MAX,
            }
        }
        SYS_PTY_CREATE => {
            // A new pair, and a descriptor for its master. The slave is opened
            // separately, by number, because that is the shape `openpty` has:
            // open the multiplexer, ask which pty it gave you, open that one.
            // The pair is kept alive by the master until then.
            let tid = scheduler::current_tid();
            let Some(pty) = crate::pty::create(tid) else {
                return u64::MAX;
            };
            match scheduler::current_alloc_fd(crate::task::FdKind::PtyEnd { pty, end: 0 }) {
                Ok(master) => {
                    crate::pty::retain(pty, 0);
                    master as u64
                }
                Err(()) => {
                    crate::pty::cleanup_orphans(tid);
                    u64::MAX
                }
            }
        }
        SYS_PTY_OPEN => {
            // arg0 = a pty's number: a descriptor for its slave end. Refused
            // for a pty whose master has gone, which is a number naming
            // nothing rather than a terminal to talk to.
            let pty = arg0 as usize;
            if !crate::pty::slave_openable(pty) {
                return u64::MAX;
            }
            // A number is not a key: the terminal is its session's.
            if !crate::pty::slave_is_for(pty, scheduler::current_tid()) {
                return u64::MAX;
            }
            match scheduler::current_alloc_fd(crate::task::FdKind::PtyEnd { pty, end: 1 }) {
                Ok(fd) => {
                    crate::pty::retain(pty, 1);
                    fd as u64
                }
                Err(()) => u64::MAX,
            }
        }
        SYS_PTY_CTL => {
            // arg0 = a descriptor naming either end, arg1 = operation,
            // arg2 = a structure to read or write.
            let tid = scheduler::current_tid();
            let Some((pty, end)) = crate::pty::of_fd(tid, arg0 as usize) else {
                return u64::MAX;
            };
            // How a terminal behaves and how big it is are changed through
            // its master, or by whoever its slave is for: a program left
            // holding the slave of a session that has ended may still ask.
            let changes = matches!(arg1, PTY_SET_TERMIOS | PTY_SET_WINSIZE);
            if changes && end == 1 && !crate::pty::slave_is_for(pty, tid) {
                return u64::MAX;
            }
            // And not by a job behind, which is stopped for trying.
            if changes && end == 1 {
                if let Some(answer) = change_from_behind(pty) {
                    return answer;
                }
            }
            match arg1 {
                PTY_NUMBER => pty as u64,
                PTY_GET_TERMIOS => {
                    let Some(t) = crate::pty::get_termios(pty) else {
                        return u64::MAX;
                    };
                    let size = core::mem::size_of::<crate::pty::Termios>();
                    if !validate_user_ptr_mut(arg2, size as u64) {
                        return u64::MAX;
                    }
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { core::ptr::write_unaligned(arg2 as *mut crate::pty::Termios, t) };
                    drop(_ua);
                    0
                }
                PTY_SET_TERMIOS => {
                    let size = core::mem::size_of::<crate::pty::Termios>();
                    if !validate_user_ptr(arg2, size as u64) {
                        return u64::MAX;
                    }
                    let _ua = crate::cpu::UserAccess::begin();
                    let t = unsafe { core::ptr::read_unaligned(arg2 as *const crate::pty::Termios) };
                    drop(_ua);
                    if crate::pty::set_termios(pty, &t) { 0 } else { u64::MAX }
                }
                PTY_GET_WINSIZE => {
                    let Some(w) = crate::pty::get_winsize(pty) else {
                        return u64::MAX;
                    };
                    let size = core::mem::size_of::<crate::pty::WinSize>();
                    if !validate_user_ptr_mut(arg2, size as u64) {
                        return u64::MAX;
                    }
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { core::ptr::write_unaligned(arg2 as *mut crate::pty::WinSize, w) };
                    drop(_ua);
                    0
                }
                PTY_SET_WINSIZE => {
                    let size = core::mem::size_of::<crate::pty::WinSize>();
                    if !validate_user_ptr(arg2, size as u64) {
                        return u64::MAX;
                    }
                    let _ua = crate::cpu::UserAccess::begin();
                    let w = unsafe { core::ptr::read_unaligned(arg2 as *const crate::pty::WinSize) };
                    drop(_ua);
                    let was = crate::pty::get_winsize(pty);
                    if !crate::pty::set_winsize(pty, &w) {
                        return u64::MAX;
                    }
                    // A program that draws itself to the terminal's size is
                    // told it has changed: SIGWINCH, for whoever is in
                    // front, which does nothing to one that has not asked.
                    let same = was.is_some_and(|was| {
                        let a = unsafe { core::slice::from_raw_parts(&was as *const _ as *const u8, size) };
                        let b = unsafe { core::slice::from_raw_parts(&w as *const _ as *const u8, size) };
                        a == b
                    });
                    if !same {
                        crate::signal::from_terminal(pty, crate::signal::SIGWINCH);
                    }
                    0
                }
                PTY_SET_SESSION => {
                    // The caller's session takes this terminal as its own,
                    // with the caller's group in front. For the session's
                    // leader to ask, of a terminal that is nobody's.
                    let session = crate::job::sid_of(tid);
                    if session != scheduler::pid_of(tid) {
                        return NOT_ALLOWED;
                    }
                    if crate::pty::set_session(pty, session, crate::job::pgid_of(tid)) {
                        0
                    } else {
                        NOT_ALLOWED
                    }
                }
                PTY_GET_SESSION | PTY_GET_FRONT | PTY_SET_FRONT => {
                    // All three are about the caller's own terminal: the one
                    // its session controls. Of any other there is nothing to
                    // say to it.
                    let session = crate::job::sid_of(tid);
                    let front = match crate::pty::job(pty) {
                        Some((s, front)) if s != 0 && s == session => front,
                        _ => return u64::MAX,
                    };
                    if arg1 == PTY_GET_SESSION {
                        return session;
                    }
                    if arg1 == PTY_GET_FRONT {
                        return front;
                    }
                    let group = arg2 & !PTY_FRONT_QUIETLY;
                    if !crate::job::group_in_session(group, session) {
                        return NOT_ALLOWED;
                    }
                    // Asked from behind, it is the asker that is stopped —
                    // a job in the background does not put itself in front —
                    // unless it has said the signal is not to stop it.
                    let mine = crate::job::pgid_of(tid);
                    if mine != front && arg2 & PTY_FRONT_QUIETLY == 0 {
                        let ttou = crate::signal::SIGTTOU;
                        let ignored = crate::signal::action(tid, ttou as u64, u64::MAX) == 1;
                        // Or held back by the task that asks, which is how
                        // a shell says it on Unix, and which the kernel can
                        // see now that the mask is the kernel's to keep.
                        let held = crate::signal::mask_of(tid) & (1 << (ttou - 1)) != 0;
                        if !ignored && !held {
                            if crate::job::orphaned(mine) {
                                return NOT_ALLOWED;
                            }
                            crate::job::raise_for_group(mine, ttou);
                            // Stopped, and started again; or told, with a
                            // handler to run. Either way it is asked again.
                            return crate::signal::INTERRUPTED;
                        }
                    }
                    if crate::pty::set_front(pty, group) { 0 } else { u64::MAX }
                }
                _ => u64::MAX,
            }
        }
        SYS_ADDRSPACE_DESTROY => {
            // An address space the caller made and nothing is running in.
            // What it frees is what the caller moved into it, which was the
            // caller's own; a spawn or an exec that fails part-way has one of
            // these and nothing else to do with it.
            let caller = scheduler::current_tid();
            let cr3 = arg0 as usize;
            if cr3 == 0
                || cr3 == paging::read_cr3()
                || !crate::userspace::is_owned_address_space(caller, cr3)
            {
                return u64::MAX;
            }
            if scheduler::space_in_use(crate::userspace::space_of(cr3)) {
                return u64::MAX;
            }
            scheduler::drop_unused_space(cr3);
            0
        }
        SYS_ADDRSPACE_CREATE => {
            // No capability. A frame for a page table, registered to the
            // caller, which confers authority over nothing: filling it needs
            // pages the caller already owns, and starting a task in it is
            // `SYS_TASK_CREATE_IN`, which does need `TaskMgmt`. A program
            // replacing itself with `execve` makes one of these, and asking it
            // to hold the capability that starts other people's tasks would be
            // asking for far more than it is doing.
            match crate::userspace::create_address_space() {
                Some(cr3) => cr3 as u64,
                None => u64::MAX,
            }
        }
        SYS_ADDRSPACE_MAP => {
            // arg0=cr3, arg1=virt, arg2=phys, arg3=pages, arg4=flags
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let cr3 = arg0 as usize;
            let virt = arg1 as usize;
            let phys = arg2 as usize;
            let pages = arg3 as usize;
            let flags = arg4;
            if !paging::user_range_ok(virt, pages) {
                return u64::MAX;
            }
            if phys & 0xFFF != 0 || phys.checked_add(pages * 4096).is_none() {
                return u64::MAX;
            }
            // The caller supplies both the target address space and the
            // backing frames, so it must actually hold authority over that
            // physical range — otherwise CAP_TASK_MGMT silently implied full
            // physical read/write.
            if !may_map_phys(scheduler::current_tid(), phys, pages) {
                return u64::MAX;
            }
            // cr3 must be an address space this task created, not an arbitrary
            // physical address reinterpreted as a PML4.
            if !crate::userspace::may_use_address_space(scheduler::current_tid(), cr3) {
                return u64::MAX;
            }
            // No OWNED bit: the frames came from the caller (via sys_phys_alloc),
            // which stays responsible for them.
            let pte_flags = paging::PRESENT | paging::USER
                | if flags & 1 != 0 { paging::WRITABLE } else { 0 };
            for i in 0..pages {
                let v = virt + i * 4096;
                let p = phys + i * 4096;
                if unsafe { paging::map_page(cr3, v, p, pte_flags) }.is_err() {
                    for j in 0..i {
                        let _ = unsafe { paging::unmap_page(cr3, virt + j * 4096) };
                    }
                    return u64::MAX;
                }
            }
            0
        }
        SYS_ADDRSPACE_GIVE => {
            // arg0=cr3, arg1=virt there, arg2=virt here, arg3=pages, arg4=flags
            //
            // Moves memory rather than lending it. SYS_ADDRSPACE_MAP leaves
            // the frames the caller's, so a spawner's program was freed when
            // the *spawner* exited — under the child, if it was still running
            // — and never when the child did: every program a shell ran cost
            // its image and a megabyte of stack for as long as the shell
            // lived. A moved page is the target's, OWNED there, and goes with
            // the address space that uses it.
            //
            // Moving, rather than handing over a frame named by its address,
            // is what keeps that sound. The caller cannot keep a mapping of
            // what it gave, so nothing is left pointing at the frame once the
            // child is gone and the allocator has handed it to someone else.
            // No capability, for the reason `SYS_ADDRSPACE_CREATE` has none:
            // the pages are the caller's own and the address space is one it
            // made, which `may_use_address_space` below is what checks.
            let caller = scheduler::current_tid();
            let cr3 = arg0 as usize;
            let virt = arg1 as usize;
            let from = arg2 as usize;
            let pages = arg3 as usize;
            if pages > 256
                || !paging::user_range_ok(virt, pages)
                || !paging::user_range_ok(from, pages)
            {
                return u64::MAX;
            }
            let own = paging::read_cr3();
            if cr3 == own || !crate::userspace::may_use_address_space(caller, cr3) {
                return u64::MAX;
            }
            // What was written out since the caller filled it comes back
            // first, and stays until this is done: it is the page that is
            // given, not a promise of one.
            if unsafe { paging::back_range(own, from as u64, (pages * 4096) as u64, false) }.is_ok() {
                scheduler::pin(from as u64, (pages * 4096) as u64);
            }
            // All of it is checked before any of it moves. Only memory the
            // caller owns may go — not a device, not shared memory, not a
            // frame somebody lent it — and nothing already mapped at the far
            // end is replaced.
            for i in 0..pages {
                let ours = unsafe { paging::leaf_flags(own, from + i * 4096) }
                    .is_some_and(|f| f & (paging::OWNED | paging::USER) == paging::OWNED | paging::USER);
                if !ours || unsafe { paging::translate(cr3, virt + i * 4096) }.is_some() {
                    return u64::MAX;
                }
            }
            let pte_flags = paging::PRESENT | paging::USER | paging::OWNED
                | if arg4 & 1 != 0 { paging::WRITABLE } else { 0 };
            let mut moved = 0;
            for i in 0..pages {
                let here = from + i * 4096;
                // A page the caller shares since a fork is its alone before
                // it goes, or what is moved is somebody else's page too.
                if unsafe { paging::unshare(own, here) }.is_err() {
                    break;
                }
                let Some(frame) = (unsafe { paging::translate(own, here) }) else {
                    break;
                };
                // Mapping can fail, for want of a page table, so it goes
                // first, while the page is still the caller's. Whatever moved
                // before a failure stays moved: it is the target's, and is
                // freed with it.
                if unsafe { paging::map_page(cr3, virt + i * 4096, frame, pte_flags) }.is_err() {
                    break;
                }
                let _ = unsafe { paging::unmap_page(own, here) };
                moved += 1;
            }
            // No longer in the caller's address space, so no longer on its
            // account — as for SYS_MUNMAP.
            scheduler::current_task_uncharge_mem(moved);
            if moved == pages { 0 } else { u64::MAX }
        }
        SYS_TASK_START | SYS_TASK_START_ARG => {
            // arg0=tid, arg1=rip, arg2=rsp, arg3=cr3
            //
            // Without `TaskMgmt` this may only start a task in the caller's
            // *own* address space — a thread of itself — and only one it
            // created. Starting somebody else's task, or one in an address
            // space built for it, is spawning, and that needs the capability.
            let caller = scheduler::current_tid();
            let privileged = crate::cap::task_has_task_mgmt(caller, 0);
            let tid = arg0 as usize;
            if !privileged {
                let own_cr3 = unsafe { scheduler::get_task_mut(caller).map(|t| t.cr3) };
                let is_child = unsafe {
                    scheduler::get_task_mut(tid).map(|t| t.parent_tid) == Some(caller)
                };
                // A thread of the caller's own address space, or a child it
                // made in an address space it built and has not started yet —
                // which is a spawn, and needs no authority over anybody for
                // the reason `may_prepare` gives.
                let own_space = own_cr3 == Some(arg3 as usize);
                let built = crate::userspace::is_owned_address_space(caller, arg3 as usize)
                    && may_prepare(caller, tid);
                if !is_child || (!own_space && !built) {
                    return u64::MAX;
                }
            }
            let rip = arg1;
            // Ensure RSP ≡ 8 mod 16 for x86_64 ABI (as if call pushed return addr).
            // Align DOWN to 16, then subtract 8 — never go above the caller's value.
            // `checked_sub` because arg2 < 8 used to wrap to a kernel address.
            let rsp = match (arg2 & !0xF).checked_sub(8) {
                Some(r) => r,
                None => return u64::MAX,
            };
            // Entry point and stack must both live in user space.
            if rip >= USER_ADDR_LIMIT || rsp >= USER_ADDR_LIMIT {
                return u64::MAX;
            }
            let cr3 = arg3 as usize;
            if !crate::userspace::may_use_address_space(scheduler::current_tid(), cr3) {
                return u64::MAX;
            }
            // arg4 carries the value for RDI; SYS_TASK_START leaves it zero
            // because syscall4 never sets that register.
            let entry_arg = if nr == SYS_TASK_START_ARG { arg4 } else { 0 };
            // A task started in the caller's own address space is a thread of
            // it, and holds what the caller holds.
            let own_cr3 = unsafe { scheduler::get_task_mut(caller).map(|t| t.cr3) };
            if own_cr3 == Some(cr3) {
                scheduler::inherit_from_creator(tid, caller, true);
                // And uses what the program has open, rather than a copy of
                // it: a descriptor is the program's. So is its process id.
                crate::fdtable::share(tid, caller);
                scheduler::join_process(tid, caller);
                // It holds back the signals the thread that made it does.
                crate::signal::task_like(tid, caller);
            }
            match scheduler::start_task(tid, rip, rsp, cr3, entry_arg) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_PHYS_ALLOC => {
            // arg0 = number of contiguous pages to allocate; arg1 = 1 for
            // frames below four gigabytes, which is what a device that is
            // told their address in a register of thirty-two bits can
            // reach. Without it they come from wherever ordinary memory
            // does, which on a machine with more memory than that is above.
            if !crate::cap::task_has_phys_alloc(scheduler::current_tid()) {
                return u64::MAX;
            }
            let count = arg0 as usize;
            if count == 0 || count > 1024 {
                return u64::MAX;
            }
            // Check memory quota
            if !scheduler::current_task_check_mem(count) {
                return u64::MAX;
            }
            let frame = crate::pmm::alloc_contiguous(count, arg1 & PHYS_LOW != 0);
            let base = match frame {
                Some(f) => f.address(),
                None => return u64::MAX,
            };
            // Record who owns these frames so sys_phys_free can verify the
            // caller actually holds them, and so reaping can reclaim them.
            crate::pmm::set_owner(base, count, scheduler::current_tid());
            scheduler::current_task_charge_mem(count);
            base as u64
        }
        SYS_PHYS_FREE => {
            // arg0 = phys addr, arg1 = count
            if !crate::cap::task_has_phys_alloc(scheduler::current_tid()) {
                return u64::MAX;
            }
            let addr = arg0 as usize;
            let count = arg1 as usize;
            if count == 0 || addr & 0xFFF != 0 {
                return u64::MAX;
            }
            if count.checked_mul(4096).and_then(|l| addr.checked_add(l)).is_none() {
                return u64::MAX;
            }
            // Every frame in the range must belong to this task. Anything
            // else — a kernel frame, another task's frames, or a partially
            // owned range — is rejected rather than pushed into the allocator.
            if !crate::pmm::owns_range(addr, count, scheduler::current_tid()) {
                return u64::MAX;
            }
            crate::pmm::clear_owner(addr, count);
            for i in 0..count {
                crate::pmm::free(crate::pmm::PhysFrame::from_address(addr + i * 4096));
            }
            // Refund the quota charged by sys_phys_alloc.
            scheduler::current_task_uncharge_mem(count);
            0
        }
        SYS_GRANT_IOPORT => {
            // arg0 = tid to grant CAP_IOPORT
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) {
                return u64::MAX;
            }
            // Must hold both ends of the range being delegated, not just port 0.
            if !crate::cap::task_has_ioport(caller, 0)
                || !crate::cap::task_has_ioport(caller, 0xFFFF)
            {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            if crate::cap::grant_slot(
                tid,
                crate::cap::CapType::IoPort,
                0,
                0xFFFF,
                caller,
            ) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_GRANT_IRQ => {
            // arg0 = tid, arg1 = irq
            let caller = scheduler::current_tid();
            if !crate::cap::task_has_task_mgmt(caller, 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let irq = arg1 as u8;
            // Delegate exactly the IRQ named in arg1. This used to hand over
            // the blanket CAP_IRQ bit, which expands to the 0xFF wildcard --
            // so delegating IRQ 1 delegated every IRQ on the machine.
            if !crate::cap::task_has_irq(caller, irq) {
                return u64::MAX;
            }
            if crate::cap::grant_slot(
                tid,
                crate::cap::CapType::Irq,
                irq as u64,
                0,
                caller,
            ) {
                0
            } else {
                u64::MAX
            }
        }
        SYS_GRANT_CAP => {
            // arg0 = tid, arg1 = capability bits to grant
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let caps = arg1 as u32;
            // The granter must hold every bit it delegates. UID 0 used to skip
            // this entirely, which made the check meaningless for the only
            // tasks that call it.
            let caller_caps = scheduler::current_task_caps();
            if caps & !caller_caps != 0 {
                return u64::MAX;
            }
            match scheduler::grant_cap(tid, caps) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_FD_WRITE => {
            // arg0 = fd, arg1 = buf ptr, arg2 = len
            let fd = arg0 as usize;
            let ptr = arg1 as *const u8;
            let len = arg2 as usize;
            if len > 0 && !validate_user_ptr(arg1, arg2) {
                return u64::MAX;
            }
            // Held for the length of the call. The descriptor is the
            // program's, and a sibling thread may close it while this one is
            // parked on what it names.
            let me = scheduler::current_tid();
            let done = match crate::fdtable::hold(me, fd) {
                crate::task::FdKind::Ipc { target_tid, endpoint, tag } => {
                    fd_write_ipc(target_tid, endpoint, tag, ptr, len)
                }
                crate::task::FdKind::PipeWrite(handle) => {
                    crate::pipe::write(handle, ptr, len)
                }
                crate::task::FdKind::PipeRead(_) => u64::MAX,
                // What a program prints waits for room, all of it; what a
                // terminal emulator types is taken or it is not.
                crate::task::FdKind::PtyEnd { pty, end: 1 } if !crate::pty::slave_is_for(pty, me) => u64::MAX,
                crate::task::FdKind::PtyEnd { pty, end: 1 } => match write_from_behind(pty) {
                    Some(answer) => answer,
                    None => pty_write(pty, ptr, len),
                },
                crate::task::FdKind::PtyEnd { pty, .. } => pty_type(pty, ptr, len) as u64,
                // A timer is armed, not written to.
                crate::task::FdKind::Timer { .. } => u64::MAX,
                crate::task::FdKind::Event { ev } => event_write(ev, ptr, len, true),
                crate::task::FdKind::StreamEnd { stream, end } => {
                    match crate::stream::pipes_for(stream, end) {
                        Some((_, wr)) => crate::pipe::write(wr, ptr, len),
                        None => u64::MAX,
                    }
                }
                // A set is waited on, and signals are read: neither is
                // written to.
                crate::task::FdKind::PollSet { .. }
                | crate::task::FdKind::Signals { .. }
                | crate::task::FdKind::Local { .. } => u64::MAX,
                // Memory is mapped, not written through. A stream of bytes is
                // the wrong shape for it, and answering as if it were would
                // put the caller's data somewhere it will never look.
                crate::task::FdKind::MemFd { .. } => u64::MAX,
                crate::task::FdKind::Socket { net_tid, endpoint, handle } => {
                    fd_write_ipc(net_tid, endpoint, sock_tag(TAG_SOCK_WRITE, handle), ptr, len)
                }
                crate::task::FdKind::Served { obj } => {
                    crate::served::io(obj, true, ptr as usize, len, true)
                }
                crate::task::FdKind::Empty => {
                    // fd not connected — fall back to kernel console for fd 1/2
                    if (fd == 1 || fd == 2) && len > 0 {
                        let _ua = crate::cpu::UserAccess::begin();
                        let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                        crate::console::puts(slice);
                        len as u64
                    } else {
                        u64::MAX
                    }
                }
            };
            crate::fdtable::unhold(me);
            done
        }
        SYS_FD_READ => {
            // arg0 = fd, arg1 = buf ptr, arg2 = max len
            let fd = arg0 as usize;
            let ptr = arg1 as *mut u8;
            let max_len = arg2 as usize;
            if max_len > 0 && !validate_user_ptr_mut(arg1, arg2) {
                return u64::MAX;
            }
            // Held for the length of the call. The descriptor is the
            // program's, and a sibling thread may close it while this one is
            // parked on what it names.
            let me = scheduler::current_tid();
            let done = match crate::fdtable::hold(me, fd) {
                crate::task::FdKind::Ipc { target_tid, endpoint, tag } => {
                    fd_read_ipc(target_tid, endpoint, tag, ptr, max_len)
                }
                crate::task::FdKind::PipeRead(handle) => {
                    crate::pipe::read(handle, ptr, max_len)
                }
                crate::task::FdKind::PipeWrite(_) => u64::MAX,
                crate::task::FdKind::PtyEnd { pty, end } => pty_read(pty, end, ptr, max_len),
                crate::task::FdKind::Timer { timer } => timer_read(timer, ptr, max_len),
                crate::task::FdKind::Event { ev } => event_read(ev, ptr, max_len),
                crate::task::FdKind::Signals { sfd } => {
                    crate::signal::read_for(me, crate::sigfd::mask(sfd), ptr, max_len, true)
                }
                // Not connected: nothing to read from.
                crate::task::FdKind::Local { .. } => u64::MAX,
                crate::task::FdKind::StreamEnd { stream, end } => {
                    match crate::stream::pipes_for(stream, end) {
                        Some((rd, _)) => crate::pipe::read(rd, ptr, max_len),
                        None => u64::MAX,
                    }
                }
                crate::task::FdKind::PollSet { .. } => u64::MAX,
                // As with write: it is mapped, not read.
                crate::task::FdKind::MemFd { .. } => u64::MAX,
                crate::task::FdKind::Socket { net_tid, endpoint, handle } => {
                    fd_read_ipc(net_tid, endpoint, sock_tag(TAG_SOCK_READ, handle), ptr, max_len)
                }
                crate::task::FdKind::Served { obj } => {
                    crate::served::io(obj, false, ptr as usize, max_len, true)
                }
                crate::task::FdKind::Empty => u64::MAX,
            };
            crate::fdtable::unhold(me);
            done
        }
        SYS_FD_READ_NB => {
            // Non-blocking fd read. Only supports pipe fds.
            let fd = arg0 as usize;
            let ptr = arg1 as *mut u8;
            let max_len = arg2 as usize;
            if max_len > 0 && !validate_user_ptr_mut(arg1, arg2) {
                return u64::MAX;
            }
            // Held for the length of the call. The descriptor is the
            // program's, and a sibling thread may close it while this one is
            // parked on what it names.
            let me = scheduler::current_tid();
            let done = match crate::fdtable::hold(me, fd) {
                crate::task::FdKind::PipeRead(handle) => {
                    crate::pipe::read_nonblock(handle, ptr, max_len)
                }
                // A terminal without waiting, which is what a program that
                // polls first and reads second asks for.
                crate::task::FdKind::PtyEnd { pty, end: 1 } if !crate::pty::slave_is_for(pty, me) => u64::MAX,
                crate::task::FdKind::PtyEnd { pty, end } => {
                    let mut buf = [0u8; 256];
                    let want = max_len.min(buf.len());
                    match crate::pty::read(pty, end, &mut buf[..want]) {
                        Ok(0) => 0,
                        Ok(n) => {
                            let _ua = crate::cpu::UserAccess::begin();
                            unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, n) };
                            drop(_ua);
                            n as u64
                        }
                        Err(()) => crate::pipe::WOULD_BLOCK,
                    }
                }
                crate::task::FdKind::StreamEnd { stream, end } => {
                    match crate::stream::pipes_for(stream, end) {
                        Some((rd, _)) => crate::pipe::read_nonblock(rd, ptr, max_len),
                        None => u64::MAX,
                    }
                }
                crate::task::FdKind::Event { ev } => {
                    if max_len < 8 {
                        u64::MAX
                    } else {
                        match crate::eventfd::take(ev) {
                            Some(n) => {
                                let _ua = crate::cpu::UserAccess::begin();
                                unsafe { core::ptr::write_unaligned(ptr as *mut u64, n) };
                                drop(_ua);
                                8
                            }
                            None => crate::pipe::WOULD_BLOCK,
                        }
                    }
                }
                crate::task::FdKind::Timer { timer } => {
                    if max_len < 8 {
                        u64::MAX
                    } else {
                        match crate::timerfd::take(timer) {
                            Some(n) => {
                                let _ua = crate::cpu::UserAccess::begin();
                                unsafe { core::ptr::write_unaligned(ptr as *mut u64, n) };
                                drop(_ua);
                                8
                            }
                            None => crate::pipe::WOULD_BLOCK,
                        }
                    }
                }
                crate::task::FdKind::Signals { sfd } => {
                    crate::signal::read_for(me, crate::sigfd::mask(sfd), ptr, max_len, false)
                }
                // A file answers at once whichever way it is asked; what is
                // not a file is told the caller will not wait.
                crate::task::FdKind::Served { obj } => {
                    crate::served::io(obj, false, ptr as usize, max_len, false)
                }
                _ => u64::MAX,
            };
            crate::fdtable::unhold(me);
            done
        }
        SYS_FD_WRITE_NB => {
            // The mirror of SYS_FD_READ_NB. A descriptor a program has marked
            // non-blocking must not park it in a write either: a main loop that
            // writes to a peer which has stopped reading would otherwise stop
            // serving everybody else.
            let fd = arg0 as usize;
            let ptr = arg1 as *const u8;
            let len = arg2 as usize;
            if len > 0 && !validate_user_ptr(arg1, arg2) {
                return u64::MAX;
            }
            // Held for the length of the call. The descriptor is the
            // program's, and a sibling thread may close it while this one is
            // parked on what it names.
            let me = scheduler::current_tid();
            let done = match crate::fdtable::hold(me, fd) {
                crate::task::FdKind::PipeWrite(handle) => {
                    crate::pipe::write_nonblock(handle, ptr, len)
                }
                crate::task::FdKind::StreamEnd { stream, end } => {
                    match crate::stream::pipes_for(stream, end) {
                        Some((_, wr)) => crate::pipe::write_nonblock(wr, ptr, len),
                        None => u64::MAX,
                    }
                }
                // A terminal's buffer is drained by whoever is at the other
                // end; a write that does not fit returns what did, which is a
                // short write and not a block.
                crate::task::FdKind::PtyEnd { pty, end: 1 } if !crate::pty::slave_is_for(pty, me) => u64::MAX,
                crate::task::FdKind::PtyEnd { pty, end: 1 } if write_from_behind(pty).is_some() => {
                    crate::signal::INTERRUPTED
                }
                crate::task::FdKind::PtyEnd { pty, end } => {
                    let n = if end == 0 {
                        pty_type(pty, ptr, len)
                    } else {
                        let _ua = crate::cpu::UserAccess::begin();
                        let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
                        crate::pty::write(pty, end, slice)
                    };
                    if n == 0 && len > 0 { crate::pipe::WOULD_BLOCK } else { n as u64 }
                }
                crate::task::FdKind::Event { ev } => event_write(ev, ptr, len, false),
                crate::task::FdKind::Served { obj } => {
                    crate::served::io(obj, true, ptr as usize, len, false)
                }
                _ => u64::MAX,
            };
            crate::fdtable::unhold(me);
            done
        }
        SYS_FD_SET => {
            // arg0 = target task tid, arg1 = fd, arg2 = service tid, arg3 = tag
            // Requires CAP_TASK_MGMT
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let fd = arg1 as usize;
            let service_tid = arg2 as usize;
            let tag = arg3;
            // The task as it is now: once it is gone the descriptor names
            // nothing, whoever is given its number.
            let endpoint = crate::cap::endpoint_of(service_tid);
            if endpoint == 0 {
                return u64::MAX;
            }
            let entry = crate::task::FdKind::Ipc {
                target_tid: service_tid,
                endpoint,
                tag,
            };
            match scheduler::set_fd(tid, fd, entry) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_PIPE_CREATE => {
            // Create a kernel pipe, returns handle index
            match crate::pipe::create() {
                Some(handle) => handle as u64,
                None => u64::MAX,
            }
        }
        SYS_PIPE_FD_SET => {
            // arg0 = target tid, arg1 = fd index or ANY_FD, arg2 = pipe handle,
            // arg3 = is_write (0 = read, 1 = write)
            //
            // Putting a pipe end into *another* task's table hands it something
            // it never asked for, and needs `TaskMgmt`. Putting one into your
            // own is `pipe(2)`, which is an ordinary thing for any program to
            // do and needs nothing — the same rule, and for the same reason, as
            // duplicating one of your own descriptors.
            let me = scheduler::current_tid();
            let tid = arg0 as usize;
            if tid != me
                && !crate::cap::task_has_task_mgmt(me, tid)
                && !may_prepare(me, tid)
            {
                return u64::MAX;
            }
            let handle = arg2 as usize;
            let is_write = arg3 != 0;
            if crate::pipe::add_ref(handle, is_write).is_err() {
                return u64::MAX;
            }
            let kind = if is_write {
                crate::task::FdKind::PipeWrite(handle)
            } else {
                crate::task::FdKind::PipeRead(handle)
            };
            // Choosing a number and filling it are one step: the table is the
            // program's, and a sibling could take the number in between.
            let placed = if arg1 == ANY_FD {
                crate::fdtable::install(tid, kind, 3)
            } else {
                scheduler::set_fd(tid, arg1 as usize, kind).ok().map(|_| arg1 as usize)
            };
            match placed {
                // The number, since the caller may have let us choose it.
                Some(fd) => fd as u64,
                None => {
                    crate::pipe::release_fd(&kind);
                    u64::MAX
                }
            }
        }
        SYS_SOCK_FD => {
            // arg0 = net server tid, arg1 = connection handle
            //
            // The handle is not checked here: the net server owns connections
            // and refuses one that does not belong to the sender, which the
            // kernel stamps on every message. What is checked is the right to
            // talk to that server at all, once, here, rather than on every
            // read and write through the descriptor.
            let net_tid = arg0 as usize;
            let handle = arg1 as usize;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), net_tid) {
                return u64::MAX;
            }
            let endpoint = crate::cap::endpoint_of(net_tid);
            match scheduler::current_alloc_fd(crate::task::FdKind::Socket { net_tid, endpoint, handle }) {
                Ok(fd) => fd as u64,
                Err(()) => u64::MAX,
            }
        }
        SYS_SOCK_INFO => {
            // arg0 = fd. Returns (net_tid << 32) | handle, so a program can
            // close the connection it is about to drop the descriptor for.
            match scheduler::current_fd(arg0 as usize) {
                crate::task::FdKind::Socket { net_tid, handle, .. } => {
                    ((net_tid as u64) << 32) | (handle as u64)
                }
                _ => u64::MAX,
            }
        }
        SYS_FD_DUP => {
            // arg0 = target tid, arg1 = target fd or ANY_FD, arg2 = source fd
            // (from current task), arg3 = lowest acceptable fd when arg1 asks
            // for any. Copies the caller's source fd into the target's table.
            //
            // Putting a descriptor into *another* task hands it authority it
            // never asked for, and needs `TaskMgmt`. Putting one into your own
            // needs nothing: a second name for something you already hold is
            // not more authority, and `dup` is a libc's most ordinary call.
            let me = scheduler::current_tid();
            let target_tid = arg0 as usize;
            if target_tid != me
                && !crate::cap::task_has_task_mgmt(me, 0)
                && !may_prepare(me, target_tid)
            {
                return u64::MAX;
            }
            let source_fd = arg2 as usize;
            // The working directory (`FD_CWD`, beside the numbered ones) can
            // be copied to and from: `fchdir` is a copy onto it, and a
            // spawner gives a child its directory by copying its own there.
            // Only something a server serves can be a directory.
            let cwd = crate::fdtable::FD_CWD;
            if source_fd != cwd && source_fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            if (source_fd == cwd || arg1 == cwd as u64)
                && !matches!(
                    crate::fdtable::get(me, source_fd),
                    crate::task::FdKind::Served { .. }
                )
            {
                return u64::MAX;
            }
            // dup2 onto itself changes nothing, and must not close the
            // descriptor on the way.
            if target_tid == me && arg1 != ANY_FD && arg1 as usize == source_fd {
                return if crate::fdtable::get(me, source_fd).is_empty() {
                    u64::MAX
                } else {
                    source_fd as u64
                };
            }
            // A second descriptor for one object is a second reference to it,
            // taken in the same step as the descriptor is read: the table is
            // the program's, and a sibling closing the source in between
            // would leave this retaining something already freed.
            let kind = match crate::fdtable::get_retained(me, source_fd) {
                Some(k) => k,
                None => return u64::MAX,
            };
            let placed = if arg1 == ANY_FD {
                crate::fdtable::install(target_tid, kind, (arg3 as usize).max(3))
            } else if arg1 == cwd as u64 {
                match crate::fdtable::replace(target_tid, cwd, kind) {
                    Ok(old) => {
                        if !old.is_empty() {
                            crate::pipe::release_fd(&old);
                        }
                        Some(cwd)
                    }
                    Err(()) => None,
                }
            } else {
                scheduler::set_fd(target_tid, arg1 as usize, kind).ok().map(|_| arg1 as usize)
            };
            match placed {
                // The number, since the caller may have let us choose it.
                Some(fd) => fd as u64,
                None => {
                    crate::pipe::release_fd(&kind);
                    u64::MAX
                }
            }
        }
        SYS_SOCKETPAIR | SYS_PACKET_PAIR => {
            // Both ends land in the caller's own table, the way socketpair(2)
            // works. Moving one into a child is sys_fd_dup followed by closing
            // our copy, which is why an end is reference counted. A pair made
            // by SYS_PACKET_PAIR keeps each write whole, for a read to take
            // whole: a socketpair of SOCK_SEQPACKET.
            let tid = scheduler::current_tid();
            let s = match crate::stream::create(tid, nr == SYS_PACKET_PAIR) {
                Some(s) => s,
                None => return u64::MAX,
            };
            let a = scheduler::install_fd(tid, crate::task::FdKind::StreamEnd { stream: s, end: 0 });
            let b = scheduler::install_fd(tid, crate::task::FdKind::StreamEnd { stream: s, end: 1 });
            match (a, b) {
                (Some(a), Some(b)) => ((a as u64) << 32) | b as u64,
                _ => {
                    if let Some(fd) = a {
                        let _ = scheduler::clear_fd(tid, fd);
                    }
                    if let Some(fd) = b {
                        let _ = scheduler::clear_fd(tid, fd);
                    }
                    crate::stream::close_end(s, 0);
                    crate::stream::close_end(s, 1);
                    u64::MAX
                }
            }
        }
        SYS_POLL => {
            // arg0 = array of (u32 fd, u32 events, u32 revents, u32 pad),
            // arg1 = count, arg2 = how long to wait, and with arg4 = 1 the
            // signals to hold back while it waits in arg3 (ppoll, pselect):
            // put on in the same step as the wait begins, and taken off as
            // the call ends — after a handler it let through has run.
            if arg4 & 1 != 0 {
                crate::signal::wait_under(scheduler::current_tid(), arg3);
            }
            //
            // A set built inside the kernel and thrown away. The saving is in
            // the syscall count, which is where it is actually spent:
            // libwayland polls two descriptors once per dispatch, and making
            // it create, fill and destroy a set from user space would be three
            // calls where one will do. Waiting cannot be done without a set,
            // because that is what a pipe becoming ready looks for.
            let tid = scheduler::current_tid();
            // As many as the program may have descriptors, as on Linux;
            // there were thirty-two.
            let n = arg1 as usize;
            if n > crate::fdtable::limit_of(tid).map_or(0, |(soft, _)| soft) {
                return u64::MAX;
            }
            if n == 0 {
                // Waiting on nothing at all is a sleep, and a main loop whose
                // sources are all timeouts does exactly that. Returning at once
                // turned that loop into a spin.
                let deadline = crate::clock::now().saturating_add(crate::clock::span(arg2));
                loop {
                    let now = crate::clock::now();
                    if now >= deadline {
                        break;
                    }
                    if let Err(crate::ipc::IpcError::Interrupted) =
                        crate::ipc::sys_recv_timeout(tid, deadline - now)
                    {
                        return crate::signal::INTERRUPTED;
                    }
                }
                return 0;
            }
            if !validate_user_ptr_mut(arg0, (n * 16) as u64) {
                return u64::MAX;
            }
            // The entries are read, and what each reports written, where the
            // program has them: there may be as many as it has descriptors.
            let entry = |i: usize| -> (usize, u32) {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    let p = (arg0 as *const u8).add(i * 16);
                    (*(p as *const u32) as usize, *(p.add(4) as *const u32))
                }
            };
            let report = |i: usize, events: u32, add: bool| {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    let p = (arg0 as *mut u8).add(i * 16 + 8) as *mut u32;
                    *p = if add { *p | events } else { events };
                }
            };

            let set = match crate::pollset::create(tid, true) {
                Some(s) => s,
                None => return u64::MAX,
            };
            let mut invalid = 0;
            for i in 0..n {
                let (fd, want) = entry(i);
                if crate::pollset::watchable(tid, fd) {
                    // The index is the token, so a hit names its own entry.
                    let events = want & (crate::pollset::READABLE | crate::pollset::WRITABLE);
                    let _ = crate::pollset::ctl(set, tid, 0, fd, events, i as u64);
                    report(i, 0, false);
                } else {
                    report(i, crate::pollset::INVALID, false);
                    invalid += 1;
                }
            }
            let mut found = |token: u64, events: u32| {
                if (token as usize) < n {
                    report(token as usize, events, true);
                }
            };

            let deadline = crate::clock::now().saturating_add(crate::clock::span(arg2));
            let mut hits = 0usize;
            let mut interrupted = false;
            loop {
                let mut got = crate::pollset::scan(set, tid, n, &mut found);
                // An invalid entry is an answer, so do not sleep on top of it.
                let now = crate::clock::now();
                if got == 0 && invalid == 0 && now < deadline {
                    // The last look, parked: what it finds is the answer.
                    crate::pollset::park(set, tid);
                    got = crate::pollset::scan(set, tid, n, &mut found);
                    if got > 0 {
                        crate::pollset::unpark(set, tid);
                    }
                }
                if got > 0 {
                    hits = got;
                    break;
                }
                if invalid > 0 || now >= deadline {
                    break;
                }
                let slept = crate::ipc::sys_recv_timeout(tid, deadline - now);
                crate::pollset::unpark(set, tid);
                if let Err(crate::ipc::IpcError::Interrupted) = slept {
                    interrupted = true;
                    break;
                }
            }
            crate::pollset::unpark(set, tid);
            crate::pollset::destroy(set);
            if interrupted {
                return crate::signal::INTERRUPTED;
            }
            (hits + invalid) as u64
        }
        SYS_POLLSET_CREATE => {
            let tid = scheduler::current_tid();
            match crate::pollset::create(tid, false) {
                Some(set) => match scheduler::install_fd(tid, crate::task::FdKind::PollSet { set }) {
                    Some(fd) => fd as u64,
                    None => {
                        crate::pollset::destroy(set);
                        u64::MAX
                    }
                },
                None => u64::MAX,
            }
        }
        SYS_POLLSET_CTL => {
            // arg0 = set fd, arg1 = op (0 add, 1 modify, 2 remove), with
            // POLLSET_WHY to be told why not; arg2 = fd, arg3 = events, arg4
            // = token.
            let tid = scheduler::current_tid();
            let why = arg1 & POLLSET_WHY != 0;
            let refused = |r: crate::pollset::Refused| if why { r as u64 } else { u64::MAX };
            let set = match pollset_of(tid, arg0 as usize) {
                Some(s) => s,
                None => return refused(crate::pollset::Refused::NotOne),
            };
            let op = arg1 & !POLLSET_WHY;
            let target = arg2 as usize;
            // Refuse what can never become ready rather than accept it and go
            // quiet.
            if op != 2 && !crate::pollset::watchable(tid, target) {
                return refused(crate::pollset::Refused::Cannot);
            }
            match crate::pollset::ctl(set, tid, op, target, arg3 as u32, arg4) {
                Ok(()) => 0,
                Err(r) => refused(r),
            }
        }
        SYS_POLLSET_WAIT => {
            // arg0 = set fd, arg1 = out array of (u64 token, u32 events,
            // u32 pad), arg2 = capacity, arg3 = how long to wait, and arg4
            // the signals to hold back while it waits (epoll_pwait), with
            // signal 9's bit set to say there are some: that one is never
            // held back, so it is free to mean that.
            let tid = scheduler::current_tid();
            if arg4 & (1 << 8) != 0 {
                crate::signal::wait_under(tid, arg4 & !(1 << 8));
            }
            let set = match pollset_of(tid, arg0 as usize) {
                Some(s) => s,
                None => return u64::MAX,
            };
            // As many as there is room for, and no more than a set can watch.
            // There were sixty-four.
            let cap = (arg2 as usize).min(crate::task::FD_MOST);
            if cap == 0 || !validate_user_ptr_mut(arg1, (cap * 16) as u64) {
                return u64::MAX;
            }
            // Each written where the program has room for it, as it is found.
            let found = |at: &mut usize, token: u64, events: u32| {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe {
                    let p = (arg1 as *mut u8).add(*at * 16);
                    *(p as *mut u64) = token;
                    *(p.add(8) as *mut u32) = events;
                    *(p.add(12) as *mut u32) = 0;
                }
                *at += 1;
            };

            let deadline = crate::clock::now().saturating_add(crate::clock::span(arg3));
            let out = loop {
                let mut at = 0;
                let n = crate::pollset::scan(set, tid, cap, |token, events| found(&mut at, token, events));
                if n > 0 {
                    break n as u64;
                }
                let now = crate::clock::now();
                if now >= deadline {
                    break 0;
                }

                // Register as the waiter *before* the last look. Anything that
                // becomes ready after this either happened before that scan,
                // so the scan sees it and we never block, or after it — and
                // then `note_pipe` finds us parked and wakes us. There is no
                // window between looking and sleeping. What the last look
                // finds is the answer: a scan takes the edges it reports.
                crate::pollset::park(set, tid);
                let mut at = 0;
                let n = crate::pollset::scan(set, tid, cap, |token, events| found(&mut at, token, events));
                if n > 0 {
                    crate::pollset::unpark(set, tid);
                    break n as u64;
                }

                // There is no `sleep` in this kernel. A task sleeps by
                // receiving from its own TID with a timeout — nobody can send
                // to that, so only the deadline or `wake_sleeper` ends it —
                // and `sleep_ticks` in quark-rt is exactly this. Reusing it
                // means the existing timeout sweep abandons the block and no
                // second sweep had to be written.
                let slept = crate::ipc::sys_recv_timeout(tid, deadline - now);
                crate::pollset::unpark(set, tid);
                if let Err(crate::ipc::IpcError::Interrupted) = slept {
                    return crate::signal::INTERRUPTED;
                }
            };
            crate::pollset::unpark(set, tid);
            out
        }
        SYS_FD_SEND => {
            // arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = fd to pass or
            // u64::MAX, arg4 = flags. This needs no authority over the peer: it takes
            // delivery by calling recv. That is the difference from
            // SYS_FD_DUP, which puts a descriptor into a task that never asked
            // and therefore requires TaskMgmt over it.
            let fd = arg0 as usize;
            let len = arg2 as usize;
            let pass = arg3;
            let tid = scheduler::current_tid();
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            if len > 0 && !validate_user_ptr(arg1, arg2) {
                return u64::MAX;
            }
            let (stream, end) = match stream_end_of(tid, fd) {
                Some(p) => p,
                None => return u64::MAX,
            };

            // What to pass: one descriptor, or with FD_MANY an array of
            // them.
            let count = if arg4 & FD_MANY != 0 {
                let count = ((arg4 >> 8) & 0xFF) as usize;
                if count > FD_MANY_MOST || (count > 0 && !validate_user_ptr(pass, count as u64 * 4)) {
                    return u64::MAX;
                }
                count
            } else {
                (pass != u64::MAX) as usize
            };
            let nth = |i: usize| -> usize {
                if arg4 & FD_MANY != 0 {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { core::ptr::read_unaligned((pass as *const u32).add(i)) as usize }
                } else {
                    pass.min(u32::MAX as u64) as usize
                }
            };
            // Held on the queue's behalf, each of them. Without this the
            // sender closing its own copy frees the object underneath a
            // descriptor still travelling. Gathered on the heap: as many as
            // a send may carry are more than a kernel stack should hold.
            let mut passed = crate::grow::Grow::new(crate::task::FdKind::Empty);
            for i in 0..count {
                let kind = crate::fdtable::get_retained(tid, nth(i));
                let room = kind.is_some() && passed.ensure(i, count, count).is_ok();
                match kind {
                    Some(kind) if room => {
                        if let Some(at) = passed.get_mut(i) {
                            *at = kind;
                        }
                    }
                    _ => {
                        if let Some(kind) = kind {
                            crate::pipe::release_fd(&kind);
                        }
                        for kind in passed.iter().take(i) {
                            crate::pipe::release_fd(kind);
                        }
                        return u64::MAX;
                    }
                }
            }
            let passed: &[crate::task::FdKind] =
                if count > 0 { unsafe { core::slice::from_raw_parts(passed.get(0).unwrap(), count) } } else { &[] };
            // The descriptors go on the queue before the bytes, so a peer
            // that reads the bytes never has to wonder whether they are
            // still coming. No more wait there than the sender may hold —
            // Linux's bound on what is in flight.
            let most = crate::fdtable::limit_of(tid).map_or(0, |(soft, _)| soft);
            if count > 0 && !crate::stream::push_fds(stream, end, passed, most) {
                for kind in passed {
                    crate::pipe::release_fd(kind);
                }
                return u64::MAX;
            }

            let (_, wr) = match crate::stream::pipes_for(stream, end) {
                Some(p) => p,
                None => return u64::MAX,
            };
            let sent = if arg4 & FD_DONTWAIT != 0 {
                crate::pipe::write_nonblock(wr, arg1 as *const u8, len)
            } else {
                crate::pipe::write(wr, arg1 as *const u8, len)
            };
            // A signal ended the wait with nothing sent: nor are the
            // descriptors, which the call made again would send a second time.
            if sent == crate::signal::INTERRUPTED && count > 0 && crate::stream::take_back_fds(stream, end, passed) {
                for kind in passed {
                    crate::pipe::release_fd(kind);
                }
            }
            sent
        }
        SYS_FD_RECV => {
            // arg0 = stream fd, arg1 = buf, arg2 = len, arg3 = where to
            // install any attached descriptor, or u64::MAX to leave it queued,
            // arg4 = flags.
            let fd = arg0 as usize;
            let len = arg2 as usize;
            let at = arg3;
            let tid = scheduler::current_tid();
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            if len > 0 && !validate_user_ptr_mut(arg1, arg2) {
                return u64::MAX;
            }
            let many = arg4 & FD_MANY != 0;
            let room = if many { (((arg4 >> 8) & 0xFF) as usize).min(FD_MANY_MOST) } else { 0 };
            if room > 0 && !validate_user_ptr_mut(at, room as u64 * 4) {
                return u64::MAX;
            }
            let (stream, end) = match stream_end_of(tid, fd) {
                Some(p) => p,
                None => return u64::MAX,
            };
            let (rd, _) = match crate::stream::pipes_for(stream, end) {
                Some(p) => p,
                None => return u64::MAX,
            };
            // A zero-length receive asks for a descriptor and nothing else.
            // It never blocks and never reports end of file, because a caller
            // draining the descriptor queue has to be able to ask once more
            // after the bytes have run out — and the alternative, parking on an
            // empty stream, is exactly the hang that made this call take flags.
            let n = if len == 0 {
                0
            } else if arg4 & FD_DONTWAIT != 0 {
                crate::pipe::read_nonblock(rd, arg1 as *mut u8, len)
            } else {
                crate::pipe::read(rd, arg1 as *mut u8, len)
            };
            if n == u64::MAX {
                return u64::MAX;
            }
            // Nothing arrived and the caller asked not to wait. Report that
            // before touching the descriptor queue: a queued descriptor
            // belongs with the bytes it was sent alongside, and taking it now
            // would deliver it on a call the caller is about to treat as
            // having delivered nothing.
            if n == crate::pipe::WOULD_BLOCK {
                return crate::pipe::WOULD_BLOCK;
            }
            // Nor, for the same reason, when a signal ended the wait.
            if n == crate::signal::INTERRUPTED {
                return n;
            }

            // `at` names a slot, or asks for any free one. Asking is what a
            // caller wants when it is translating `recvmsg`: Linux chooses the
            // number and reports it, and a caller that had to guess would have
            // to probe — which cannot be done without reading, and reading is
            // the thing it is trying to do exactly once.
            // With FD_MANY: as many as there are and room for, each where the
            // table has a slot, in the order they were sent. Whether there is
            // a slot is asked before each is taken, as below.
            if many {
                // Each number written as it lands, into what the call
                // checked and holds in memory.
                let mut k = 0;
                while k < room && scheduler::lowest_free_fd(tid).is_some() {
                    let Some(kind) = crate::stream::pop_fd(stream, end) else { break };
                    match crate::fdtable::install(tid, kind, 3) {
                        Some(fd) => {
                            let _ua = crate::cpu::UserAccess::begin();
                            unsafe { core::ptr::write_unaligned((at as *mut u32).add(k), fd as u32) };
                            k += 1;
                        }
                        None => {
                            crate::pipe::release_fd(&kind);
                            break;
                        }
                    }
                }
                return (k as u64) << 32 | n;
            }
            let mut got = 0u64;
            if at != u64::MAX {
                // Check there is somewhere to put it *before* taking the
                // descriptor off the queue. Popping first and failing to
                // install destroys something the sender handed over and the
                // receiver asked for, and neither of them is told.
                let room = if at == ANY_FD {
                    scheduler::lowest_free_fd(tid).is_some()
                } else {
                    (at as usize) < crate::task::FD_MOST
                        && crate::fdtable::get(tid, at as usize).is_empty()
                };
                if room {
                    if let Some(kind) = crate::stream::pop_fd(stream, end) {
                        // The queue's reference becomes the descriptor's:
                        // nothing is retained and nothing released, because a
                        // reference belongs to no task. Memory arriving this
                        // way is the receiver's to map — the sender chose to
                        // send and the receiver asked to take.
                        let landed = if at == ANY_FD {
                            crate::fdtable::install(tid, kind, 3)
                        } else if crate::fdtable::get(tid, at as usize).is_empty() {
                            scheduler::set_fd(tid, at as usize, kind).ok().map(|_| at as usize)
                        } else {
                            None
                        };
                        match landed {
                            // The number, not merely the fact: a caller that
                            // asked for any slot has no other way to learn it.
                            Some(fd) => got = fd as u64 + 1,
                            // A sibling took the last slot in between.
                            None => crate::pipe::release_fd(&kind),
                        }
                    }
                }
            }
            (got << 32) | n
        }
        SYS_MEMFD_CREATE => {
            // Memory a program can name and hand over. The region is exactly
            // what SYS_SHMEM_CREATE makes; the descriptor is what lets it
            // travel, be inherited, and be closed like anything else.
            let pages = arg0 as usize;
            let handle = crate::shmem::create_fd(pages);
            if handle == u64::MAX {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            let kind = crate::task::FdKind::MemFd { handle: handle as usize };
            match scheduler::install_fd(tid, kind) {
                Some(fd) => fd as u64,
                None => {
                    crate::shmem::fd_release(handle as usize);
                    u64::MAX
                }
            }
        }
        SYS_MEMFD_TRUNCATE => {
            // arg0 = descriptor naming memory, arg1 = size in bytes
            let fd = arg0 as usize;
            let bytes = arg1 as usize;
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            let handle = match crate::fdtable::get(tid, fd) {
                crate::task::FdKind::MemFd { handle } => handle,
                _ => return u64::MAX,
            };
            match crate::shmem::resize(handle, bytes.div_ceil(4096)) {
                u64::MAX => u64::MAX,
                pages => pages * 4096,
            }
        }
        SYS_MMAP_FD => {
            // arg0 = descriptor naming memory, arg1 = where to map it
            let fd = arg0 as usize;
            let vaddr = arg1 as usize;
            let tid = scheduler::current_tid();
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            let handle = match crate::fdtable::get(tid, fd) {
                crate::task::FdKind::MemFd { handle } => handle,
                _ => return u64::MAX,
            };
            // The size comes back, not merely success. A receiver of a
            // descriptor knows nothing about how big the memory behind it is,
            // and the sender's word for it is the one thing it must not take:
            // a client that says its pool is sixteen megabytes when it is one
            // page is asking the compositor to read memory that is not there.
            // Holding the descriptor is the permission.
            match crate::shmem::map_held(handle, vaddr) {
                u64::MAX => u64::MAX,
                pages => pages * 4096,
            }
        }
        SYS_FD_CLOSE => {
            // Releasing a descriptor is releasing whatever it refers to: a
            // pipe loses a reader or a writer, and a reader reaching zero is
            // what turns the peer's next read into end-of-file. Nothing here
            // needs a capability — a task may always drop its own.
            let fd = arg0 as usize;
            if fd >= crate::task::FD_MOST {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            let kind = crate::fdtable::take(tid, fd);
            if kind.is_empty() {
                return u64::MAX;
            }
            crate::pipe::release_fd(&kind);
            0
        }
        SYS_FUTEX_WAIT => {
            // arg0 = addr, arg1 = expected value
            crate::futex::futex_wait(arg0, arg1 as u32)
        }
        SYS_FUTEX_WAKE => {
            // arg0 = addr, arg1 = max_wake
            crate::futex::futex_wake(arg0, arg1)
        }
        SYS_FUTEX_PI => {
            // arg0 = op (0 lock, 1 try, 2 unlock), arg1 = the word, arg2 =
            // for a lock how long to wait at most, a span, 0 for no end.
            match arg0 {
                0 => crate::futex::lock_pi(arg1, arg2, false),
                1 => crate::futex::lock_pi(arg1, 0, true),
                2 => crate::futex::unlock_pi(arg1),
                _ => u64::MAX,
            }
        }
        SYS_SCHED => {
            // arg0 = op (0 nice, 1 set nice, 2 set class, 3 class), arg1 =
            // the task (0 for the caller), arg2 and arg3 what the op takes.
            crate::usage::sched(scheduler::current_tid(), arg0, arg1, arg2, arg3)
        }
        SYS_FUTEX_REQUEUE => {
            // arg0 = the first word, arg1 = the second, arg2 = (how many to
            // wake << 32) | how many to move, arg3 = what the first must
            // still hold, arg4 = flags: bit 0, compare it.
            let expected = (arg4 & 1 != 0).then_some(arg3 as u32);
            crate::futex::requeue(arg0, arg1, arg2 >> 32, arg2 & 0xFFFF_FFFF, expected)
        }
        SYS_FUTEX_WAIT_TIMEOUT => {
            // arg0 = addr, arg1 = expected value, arg2 = how long to wait
            crate::futex::futex_wait_timeout(arg0, arg1 as u32, crate::clock::span(arg2))
        }
        SYS_MMAP => {
            // arg0 = vaddr, arg1 = pages
            // Allocates physical frames and maps them into the caller's address space.
            // No capability required — every task can grow its own heap.
            let vaddr = arg0 as usize;
            let pages = arg1 as usize;
            if pages == 0 || pages > 256 {
                return u64::MAX;
            }
            // Page-aligned, no overflow, and clear of PML4[0] (whose page
            // directories are shared with the kernel).
            if !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }

            let cr3 = paging::read_cr3();

            // Refuse to map over anything already there. map_page would
            // overwrite the entry, which loses the old frame — leaked, since
            // nothing tracks it any more — and silently replaces whatever the
            // owner had in it with zeroes. That is invisible to both parties:
            // the previous owner keeps using addresses whose contents have
            // been swapped out from under it. Failing here means a caller that
            // guessed at a base address finds out, rather than corrupting the
            // task it collided with.
            if !unsafe { paging::range_is_free(cr3, vaddr, pages) } {
                return u64::MAX;
            }

            // Charge up front so a partial failure can't leave pages mapped
            // but unaccounted; the rollback path refunds.
            if !scheduler::current_task_check_mem(pages) {
                return u64::MAX;
            }
            scheduler::current_task_charge_mem(pages);
            // OWNED: anonymous memory this address space must free on teardown.
            let flags =
                paging::PRESENT | paging::WRITABLE | paging::USER | paging::OWNED;
            for i in 0..pages {
                let v = vaddr + i * 4096;
                // A frame, waiting for one if memory is short and waiting
                // can produce one: this is what starts a program, and a
                // program that could not be started for want of memory that
                // was only being written out is one that was ended by it.
                let phys = loop {
                    match crate::reclaim::frame() {
                        Some(frame) => break Some(frame),
                        None if crate::reclaim::wait() => {}
                        None => break None,
                    }
                };
                let Some(phys) = phys else {
                    unmap_range_owned(cr3, vaddr, i);
                    scheduler::current_task_uncharge_mem(pages);
                    return u64::MAX;
                };
                // Zero the frame (identity-mapped)
                unsafe { core::ptr::write_bytes(phys as *mut u8, 0, 4096) };
                if unsafe { paging::map_page(cr3, v, phys, flags) }.is_err() {
                    crate::pmm::free(crate::pmm::PhysFrame::from_address(phys));
                    unmap_range_owned(cr3, vaddr, i);
                    scheduler::current_task_uncharge_mem(pages);
                    return u64::MAX;
                }
            }
            0
        }
        SYS_MUNMAP => {
            // arg0 = vaddr, arg1 = pages
            // Unmaps pages and frees their physical frames.
            let vaddr = arg0 as usize;
            let pages = arg1 as usize;
            if pages == 0 || pages > 256 {
                return u64::MAX;
            }
            if !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            let cr3 = paging::read_cr3();
            // Mappings and reservations alike. Only frames this address space
            // owns are returned to the PMM: shared-memory pages and device
            // MMIO are unmapped but never freed — otherwise munmap
            // double-frees a shmem region or hands the allocator a device
            // physical address.
            let freed = unsafe { paging::clear_range(cr3, vaddr, pages) };
            if freed > 0 {
                scheduler::current_task_uncharge_mem(freed);
            }
            freed as u64
        }
        SYS_MAP_ANON => {
            // arg0 = vaddr, arg1 = pages, arg2 = flags (bit 0: back it now;
            // bit 1: no bigger than the machine).
            // Memory promised rather than given: each page gets a frame when
            // it is first touched. No capability, as for SYS_MMAP.
            let vaddr = arg0 as usize;
            let pages = arg1 as usize;
            if pages == 0 || pages > MAP_ANON_MAX || !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            if arg2 & !(MAP_ANON_POPULATE | MAP_ANON_ACCOUNT) != 0 {
                return u64::MAX;
            }
            if arg2 & MAP_ANON_ACCOUNT != 0 && pages > crate::pmm::total_count() {
                return u64::MAX;
            }
            let cr3 = paging::read_cr3();
            if !unsafe { paging::range_is_free(cr3, vaddr, pages) } {
                return u64::MAX;
            }
            let entry = paging::marker_entry(true, false);
            if unsafe { paging::reserve_range(cr3, vaddr, pages, entry) }.is_err() {
                unsafe { paging::clear_range(cr3, vaddr, pages) };
                return u64::MAX;
            }
            if arg2 & MAP_ANON_POPULATE != 0 {
                let len = (pages * 4096) as u64;
                if unsafe { paging::back_range(cr3, vaddr as u64, len, true) }.is_err() {
                    let freed = unsafe { paging::clear_range(cr3, vaddr, pages) };
                    scheduler::current_task_uncharge_mem(freed);
                    return u64::MAX;
                }
            }
            0
        }
        SYS_OBJECT_CREATE => {
            // arg0 = cookie, arg1 = bytes, arg2 = slot for the capability.
            let caller = scheduler::current_tid();
            let slot = arg2 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let free = crate::cap::slot(caller, slot).is_some_and(|c| c.cap_type == crate::cap::CapType::Empty);
            if !free || crate::memobj::objects_of(caller) >= OBJECTS_PER_PAGER {
                return u64::MAX;
            }
            // Room for the capability, and for the slot's count of
            // revocations, before there is an object to be left holding:
            // neither can fail once it is made.
            let root = crate::cap::root_of(caller);
            let Some(generation) = crate::cap::generation_for(root, slot) else {
                return u64::MAX;
            };
            if crate::cap::with_cspace(caller, |cs| cs.set(slot, cs.get(slot))) != Some(true) {
                return u64::MAX;
            }
            let Some((_, id)) = crate::memobj::create(caller, arg0, arg1) else {
                return u64::MAX;
            };
            let made = crate::cap::CapSlot {
                cap_type: crate::cap::CapType::MemObject,
                generation,
                root_slot: slot as u16,
                root,
                param0: id,
                param1: crate::cap::OBJECT_READ | crate::cap::OBJECT_WRITE,
            };
            crate::cap::with_cspace(caller, |cs| cs.set(slot, made));
            id
        }
        SYS_OBJECT_MAP => {
            // arg0 = slot, arg1 = address, arg2 = pages, arg3 = first page,
            // arg4 = flags (1 write, 2 shared, 4 exec).
            let caller = scheduler::current_tid();
            let (slot, vaddr, pages, first, flags) =
                (arg0 as usize, arg1 as usize, arg2 as usize, arg3, arg4);
            if slot >= crate::cap::MAX_CAPS
                || flags & !(OBJECT_MAP_WRITE | OBJECT_MAP_SHARED | OBJECT_MAP_EXEC) != 0
                || pages == 0
                || pages > MAP_ANON_MAX
                || !paging::user_range_ok(vaddr, pages)
                || first.checked_add(pages as u64).is_none_or(|end| end > crate::memobj::MAX_PAGE)
            {
                return u64::MAX;
            }
            let Some(cap) = crate::cap::slot(caller, slot) else {
                return u64::MAX;
            };
            if cap.cap_type != crate::cap::CapType::MemObject || !crate::cap::slot_is_valid(&cap) {
                return u64::MAX;
            }
            let write = flags & OBJECT_MAP_WRITE != 0;
            let shared = flags & OBJECT_MAP_SHARED != 0;
            // Reading needs read access; writing through to the object needs
            // write access. A private copy is the caller's own to write.
            let needs = crate::cap::OBJECT_READ
                | if write && shared { crate::cap::OBJECT_WRITE } else { 0 };
            if cap.param1 & needs != needs {
                return u64::MAX;
            }
            let Some(objslot) = crate::memobj::slot_of(cap.param0) else {
                return u64::MAX;
            };
            let cr3 = paging::read_cr3();
            if !unsafe { paging::range_is_free(cr3, vaddr, pages) } {
                return u64::MAX;
            }
            let exec = flags & OBJECT_MAP_EXEC != 0;
            let made = unsafe {
                paging::reserve_object(cr3, vaddr, pages, objslot, first, write, shared, exec)
            };
            if made.is_err() {
                let freed = unsafe { paging::clear_range(cr3, vaddr, pages) };
                scheduler::current_task_uncharge_mem(freed);
                return u64::MAX;
            }
            0
        }
        SYS_OBJECT_CTL => {
            // arg0 = object id, arg1 = op, arg2/arg3 per op. The buffer an
            // op copies through is a page of the caller's.
            let op = arg1;
            if matches!(
                op,
                crate::memobj::CTL_READ_PAGE
                    | crate::memobj::CTL_WRITE_PAGE
                    | crate::memobj::CTL_TAKE_DIRTY
                    | crate::memobj::CTL_TAKE_OUT
            ) && !validate_user_range(arg2, 4096, op != crate::memobj::CTL_WRITE_PAGE)
            {
                return u64::MAX;
            }
            // To be where every program's memory is written out to is to
            // be trusted with all of it.
            if op == crate::memobj::CTL_SWAP && !crate::cap::task_has_swap(scheduler::current_tid()) {
                return u64::MAX;
            }
            crate::memobj::ctl(scheduler::current_tid(), arg0, op, arg2, arg3)
        }
        SYS_PAGE_OUT => {
            // arg0 = address, arg1 = pages. The caller's own memory, which
            // it may always give up: no capability.
            let (vaddr, pages) = (arg0 as usize, arg1 as usize);
            if pages == 0 || pages > MAP_ANON_MAX || !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            crate::reclaim::page_out(vaddr, pages)
        }
        SYS_OBJECT_SYNC => {
            // arg0 = start address, arg1 = pages. Each object mapped shared
            // there is asked, by its pager, to write back; this returns when
            // they have all answered.
            let (vaddr, pages) = (arg0 as usize & !0xFFF, arg1 as usize);
            if pages == 0 || pages > MAP_ANON_MAX || !paging::user_range_ok(vaddr, pages) {
                return u64::MAX;
            }
            let mut slots = [0usize; 16];
            let n = unsafe { paging::shared_objects_in(paging::read_cr3(), vaddr, pages, &mut slots) };
            let mut ok = true;
            for &slot in &slots[..n] {
                let Some((pager, cookie, id)) = crate::memobj::pager_of(slot) else {
                    ok = false;
                    continue;
                };
                let msg = crate::ipc::Message {
                    sender: 0,
                    tag: crate::ipc::TAG_OBJECT_SYNC,
                    data: [cookie, id, 0, 0, 0, 0],
                };
                ok &= matches!(crate::ipc::pager_call(pager, &msg, None), Ok(r) if r.tag == 0);
            }
            if ok { 0 } else { u64::MAX }
        }
        SYS_MEM_INFO => {
            // arg0 = what to say: 0, the frames that are free and the pages
            // charged to the caller; 1, how many frames of memory the
            // machine has; 2, where its memory ends, as the number of the
            // frame after the last — more than a million of them is memory
            // above four gigabytes.
            match arg0 {
                0 => {
                    let free = crate::pmm::free_count() as u64;
                    let charged = scheduler::current_task_mem() as u64;
                    (free.min(u32::MAX as u64) << 32) | charged.min(u32::MAX as u64)
                }
                1 => crate::pmm::total_count() as u64,
                2 => (crate::pmm::top_of_memory() / 4096) as u64,
                // Where memory is written out to: how many pages of room
                // there are, and how many are in use. Nought and nought
                // where there is nowhere.
                3 => {
                    let (room, used) = crate::memobj::swap_room();
                    ((room as u64).min(u32::MAX as u64) << 32) | (used as u64).min(u32::MAX as u64)
                }
                // And how busy it has been: pages written out, and pages
                // read back, since the machine started.
                4 => {
                    let (out, back) = crate::memobj::swap_traffic();
                    (out.min(u32::MAX as u64) << 32) | back.min(u32::MAX as u64)
                }
                // In a kernel built to test its stacks (`stacktest`): call
                // itself until the stack has run out, which is the end of the
                // machine, and a fault that says why. Any other kernel answers
                // that it was not built.
                #[cfg(feature = "stacktest")]
                6 => run_out_of_stack(0),
                _ => u64::MAX,
            }
        }
        SYS_RECV_TIMEOUT => {
            // arg0 = from, arg1 = msg_ptr, arg2 = how long to wait
            let from = arg0 as usize;
            let msg_ptr = arg1 as *mut crate::ipc::Message;
            let timeout = crate::clock::span(arg2);
            let msg_size = core::mem::size_of::<crate::ipc::Message>() as u64;
            if !validate_user_ptr_mut(arg1, msg_size) { return u64::MAX; }
            match crate::ipc::sys_recv_timeout(from, timeout) {
                Ok(msg) => {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe { *msg_ptr = msg };
                    0
                }
                Err(crate::ipc::IpcError::Timeout) => 1,
                // A sleep — a receive from oneself — ended by a signal.
                Err(crate::ipc::IpcError::Interrupted) => 2,
                Err(_) => u64::MAX,
            }
        }
        SYS_TICKS => {
            // The clock's time in the unit a tick is, which on a machine with
            // no finer clock is the count of them.
            crate::clock::now() / crate::clock::TICK_NS
        }
        SYS_BOOT_TIME => crate::clock::boot_seconds(),
        SYS_CLOCK => {
            // arg0 = which: nanoseconds since boot, or with 1 since 1970 —
            // which is 0 on a machine with no clock to have said.
            match arg0 {
                0 => crate::clock::now(),
                CLOCK_WALL => crate::clock::wall(),
                _ => u64::MAX,
            }
        }
        SYS_CLOCK_SET => {
            // arg0 = nanoseconds since 1970, now. The clock is the machine's,
            // so this is for whoever holds the right to set it; and it is
            // written through to the clock that keeps time while the machine
            // is off, or the next boot would undo it.
            if !crate::cap::task_has_clock(scheduler::current_tid()) {
                return u64::MAX;
            }
            // Not before 1970 was a second old, and not after the year 2200:
            // the first is how "no clock" is written, and the second is
            // further than the clock that is written through to can say.
            if !(1_000_000_000..7_258_118_400_000_000_000).contains(&arg0) {
                return u64::MAX;
            }
            crate::clock::set_wall(arg0);
            crate::rtc::write(arg0 / 1_000_000_000);
            0
        }
        SYS_PTIMER => {
            let tid = scheduler::current_tid();
            let now = crate::clock::now();
            // Where to write how a timer stands, two words of nanoseconds:
            // left, and between firings.
            let write = |at: u64, (left, every): (u64, u64)| {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { core::ptr::write_unaligned(at as *mut [u64; 2], [left, every]) };
            };
            match arg0 {
                // arg1 = the clock (0 the date, 1 or 7 the time since boot);
                // arg2 = the signal, 0 for none, with bit 8 to carry the
                // timer's own number rather than arg3; arg3 = what it
                // carries; arg4 = the task of the caller's program to raise
                // it for alone, 0 for the program.
                PTIMER_CREATE => crate::ptimer::create(tid, arg1, arg2 & 0xFF, arg2 & 0x100 != 0, arg3, arg4 as usize)
                    .map_or(u64::MAX, |id| id as u64),
                // arg1 = the timer, with bit 32 for an absolute time; arg2 =
                // when it first fires, a span from now — or with bit 32 of
                // arg1 a time on its clock, in nanoseconds — 0 to disarm it;
                // arg3 = a span between firings, 0 for once; arg4 = where to
                // write how it stood before, or 0.
                PTIMER_SET => {
                    if arg4 != 0 && !validate_user_ptr_mut(arg4, 16) {
                        return u64::MAX;
                    }
                    let absolute = arg1 >> 32 & 1 != 0;
                    let first = if absolute { arg2 } else { crate::clock::span(arg2) };
                    let every = crate::clock::span(arg3);
                    match crate::ptimer::set(tid, (arg1 & 0xFFFF_FFFF) as usize, absolute, first, every, now) {
                        Some((was, at)) => {
                            if arg4 != 0 {
                                write(arg4, was);
                            }
                            if at != 0 {
                                crate::clock::due(at);
                            }
                            0
                        }
                        None => u64::MAX,
                    }
                }
                // arg1 = the timer, arg2 = where to write how it stands, or
                // 0 to ask only whether it is one.
                PTIMER_GET => {
                    if arg2 != 0 && !validate_user_ptr_mut(arg2, 16) {
                        return u64::MAX;
                    }
                    match crate::ptimer::get(tid, arg1 as usize, now) {
                        Some(stands) => {
                            if arg2 != 0 {
                                write(arg2, stands);
                            }
                            0
                        }
                        None => u64::MAX,
                    }
                }
                // arg1 = the timer.
                PTIMER_DELETE => {
                    if crate::ptimer::delete(tid, arg1 as usize) { 0 } else { u64::MAX }
                }
                _ => u64::MAX,
            }
        }
        SYS_MSI_ALLOC => {
            // An interrupt of the caller's own, for a device that sends its
            // interrupts as messages: a number from 16 up, which the caller
            // is registered for as `SYS_IRQ_REGISTER` would have registered
            // it, and the two words to program the device with — where to
            // send, and what.
            //
            // With arg1 = 1, for the PCI device arg0, which the caller
            // holds: and the kernel programs it, where the device has an
            // MSI capability — where its message goes is not the driver's
            // to say. With none — MSI-X alone — the driver writes the two
            // words into the device's table itself.
            //
            // Without a device it takes the capability for any interrupt
            // (0xFF): one for a particular line is for that line. Either way
            // a local APIC, which is what such a message is sent to.
            let tid = scheduler::current_tid();
            let device = (arg1 == 1).then_some(arg0);
            let allowed = match device {
                Some(bdf) => crate::cap::task_has_pci_device(tid, bdf) && crate::pci::find(bdf).is_some(),
                None => crate::cap::task_has_irq(tid, 0xFF),
            };
            if !allowed || !crate::lapic::present() {
                return u64::MAX;
            }
            let to = crate::percpu::apic_id(0) as u64;
            // The address has eight bits for a processor.
            if to > 0xFF {
                return u64::MAX;
            }
            let Some(irq) = crate::irq_dispatch::allocate_message(tid) else {
                return u64::MAX;
            };
            let address = 0xFEE0_0000u64 | (to << 12);
            let data = crate::ioapic::FIRST_VECTOR as u64 + irq as u64;
            if let Some(d) = device.and_then(crate::pci::find) {
                crate::pci::aim(d, address as u32, data as u16);
            }
            ((irq as u64) << 48) | (data << 32) | address
        }
        SYS_PCI_DEVICE => {
            // arg0 = a device to start from, arg1 = where to write the 21
            // words that describe it. The first device at or after arg0
            // that the caller holds.
            let tid = scheduler::current_tid();
            if !validate_user_ptr_mut(arg1, (crate::pci::RECORD * 8) as u64) {
                return u64::MAX;
            }
            let Some(d) = crate::pci::next(arg0, |bdf| crate::cap::task_has_pci_device(tid, bdf as u64)) else {
                return u64::MAX;
            };
            let record = crate::pci::record(d);
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::copy_nonoverlapping(record.as_ptr(), arg1 as *mut u64, crate::pci::RECORD) };
            d.bdf as u64
        }
        SYS_PCI_READ => {
            // arg0 = a device the caller holds, arg1 = offset, arg2 = 1, 2
            // or 4 bytes.
            let tid = scheduler::current_tid();
            if !crate::cap::task_has_pci_device(tid, arg0) || crate::pci::find(arg0).is_none() || arg1 > 0xFFFF {
                return u64::MAX;
            }
            crate::pci::read(arg0 as u16, arg1 as u16, arg2.min(8) as u8).map_or(u64::MAX, |v| v as u64)
        }
        SYS_PCI_WRITE => {
            // arg0 = a device the caller holds, arg1 = offset, arg2 = 1, 2
            // or 4 bytes, arg3 = the value. What the kernel keeps is
            // refused: where the device is, where its message goes, and
            // turning its bus mastering on before its program has claimed
            // it.
            let tid = scheduler::current_tid();
            let Some(d) = crate::pci::find(arg0).filter(|_| crate::cap::task_has_pci_device(tid, arg0)) else {
                return u64::MAX;
            };
            if arg1 > 0xFFFF || !matches!(arg2, 1 | 2 | 4) {
                return u64::MAX;
            }
            let (offset, width, value) = (arg1 as u16, arg2 as u8, arg3 as u32);
            if crate::pci::kept(d, offset, width) {
                return NOT_ALLOWED;
            }
            if crate::pci::turns_master_on(d.bdf, offset, value)
                && !crate::iommu::claimed_by(scheduler::space_of_task(tid), d.bdf)
            {
                return NOT_ALLOWED;
            }
            if crate::pci::write(d.bdf, offset, width, value) { 0 } else { u64::MAX }
        }
        SYS_DISPLAY_MEMORY => {
            // arg0 = a display device (class 0x03) the caller holds and its
            // program has claimed, arg1 = pages, arg2 = an empty slot of the
            // caller's. The screen's memory (`display.rs`), as a `PhysRange`
            // in that slot, which the device reaches from now on: where it
            // begins, or u64::MAX.
            let tid = scheduler::current_tid();
            let space = scheduler::space_of_task(tid);
            let (pages, slot) = (arg1 as usize, arg2 as usize);
            let Some(d) = crate::pci::find(arg0).filter(|_| crate::cap::task_has_pci_device(tid, arg0)) else {
                return u64::MAX;
            };
            if d.class >> 16 != 0x03 || !crate::iommu::claimed_by(space, d.bdf) || slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            // Empty, and with room made for what will be put there.
            let empty = crate::cap::with_cspace(tid, |cs| {
                cs.get(slot).cap_type == crate::cap::CapType::Empty && cs.set(slot, crate::cap::CapSlot::empty())
            });
            if empty != Some(true) {
                return u64::MAX;
            }
            let Some(base) = crate::display::memory(d.bdf, pages) else {
                return u64::MAX;
            };
            let (start, end) = (base as u64, (base + pages * 4096) as u64);
            // The kernel's, never revoked: no count to carry.
            let _ = crate::cap::with_cspace(tid, |cs| {
                cs.set(
                    slot,
                    crate::cap::CapSlot {
                        cap_type: crate::cap::CapType::PhysRange,
                        generation: 0,
                        root_slot: slot as u16,
                        root: crate::cap::KERNEL_ROOT,
                        param0: start,
                        param1: end,
                    },
                )
            });
            crate::iommu::reach(space, base, pages);
            start
        }
        SYS_POWER => {
            // arg0 = 0 to turn the machine off, 1 to start it again. For a
            // holder of `Power`. Neither comes back when it is done, and
            // starting again is always done. Turning off answers with a
            // failure where the firmware's tables do not say how — asked
            // before anything is stopped — and where they did and the
            // machine is still here, by which time the other processors
            // have been stopped: whoever asked is what is left running,
            // and has whatever else it knows to try.
            if !crate::cap::task_has_power(scheduler::current_tid()) {
                return u64::MAX;
            }
            match arg0 {
                POWER_OFF => {
                    crate::power::off();
                    u64::MAX
                }
                POWER_RESTART => crate::power::restart(),
                _ => u64::MAX,
            }
        }
        SYS_CPUS => {
            // No capability: it is a number every program is entitled to
            // divide its work by. The processor the caller is on is true of
            // the instant it was read and of no other: a task is moved
            // wherever it can be preempted, which here is the next line.
            let on = crate::percpu::index() as u64;
            (on << 32) | crate::percpu::count() as u64
        }
        SYS_GETRANDOM => {
            // arg0 = buffer, arg1 = length, arg2 = flags (none yet). No
            // capability: a random number is nobody's secret until it has
            // been handed out. At most a mebibyte a call, a page at a time,
            // so interrupts are never off for long.
            let len = (arg1 as usize).min(1 << 20);
            if !validate_user_ptr_mut(arg0, len as u64) {
                return u64::MAX;
            }
            let mut chunk = [0u8; 4096];
            let mut done = 0;
            while done < len {
                let n = (len - done).min(chunk.len());
                crate::random::fill(&mut chunk[..n]);
                {
                    let _ua = crate::cpu::UserAccess::begin();
                    unsafe {
                        core::ptr::copy_nonoverlapping(chunk.as_ptr(), (arg0 as *mut u8).add(done), n);
                    }
                }
                done += n;
            }
            chunk.fill(0);
            done as u64
        }
        SYS_WAIT => {
            // Block until a child task exits. Returns child TID or u64::MAX.
            scheduler::sys_wait()
        }
        SYS_WAIT_FOR => {
            // arg0 = the child to wait for, or 0 for any; arg1 = flags.
            let mut reports = 0;
            if arg1 & WAIT_STOPPED != 0 {
                reports |= crate::job::HAS_STOPPED;
            }
            if arg1 & WAIT_CONTINUED != 0 {
                reports |= crate::job::HAS_CONTINUED;
            }
            scheduler::sys_wait_for(
                arg0,
                scheduler::Wait {
                    no_wait: arg1 & WAIT_NO_WAIT != 0,
                    by_pid: arg1 & WAIT_BY_PID != 0,
                    group: arg1 & WAIT_GROUP != 0,
                    reports,
                },
            )
        }
        SYS_PID => {
            // arg0 = a task, or 0 for the caller; arg1 = 1 for the task's
            // own number, which is its program's process id when it is the
            // task the program began as.
            let tid = if arg0 == 0 { scheduler::current_tid() } else { arg0 as usize };
            let number = if arg1 == 1 && scheduler::task_is_live(tid) {
                crate::cap::endpoint_of(tid)
            } else {
                scheduler::pid_of(tid)
            };
            match number {
                0 => u64::MAX,
                n => n,
            }
        }
        SYS_SIG_ACTION => {
            // arg0 = signal, arg1 = 0 default / 1 ignore / 2 handled, the
            // program being told / 3 handled, the kernel running the
            // handler — with arg2 what else to hold back while it runs,
            // arg3 how (`signal::NODEFER` and the rest), arg4 where the
            // program is entered — or anything else to ask. Returns what it
            // was.
            if arg1 == 3 {
                crate::signal::handle(scheduler::current_tid(), arg0, arg2, arg3, arg4)
            } else {
                crate::signal::action(scheduler::current_tid(), arg0, arg1)
            }
        }
        SYS_SIG_MASK => {
            // arg0 = how: 0 hold these back too, 1 let these through, 2
            // hold back exactly these, 3 the same and wait for one that is
            // not, 4 say what is waiting that is held back; anything else
            // asks. arg1 = the signals, bit n - 1 for signal n. Returns what
            // was held back before.
            crate::signal::mask(scheduler::current_tid(), arg0, arg1)
        }
        SYS_USAGE => {
            // arg0 = 0 the caller's program, 1 the children it collected, 2
            // the calling task, 3 the program task arg2 is in; arg1 = where
            // to write four words: ns in the program, ns in the kernel for
            // it, times it gave the processor up, times it had it taken.
            crate::usage::usage(scheduler::current_tid(), arg0, arg1, arg2)
        }
        SYS_NICE => {
            // arg0 = a process id, 0 for the caller's; arg1 = how nice to be,
            // -20 to 19, or u64::MAX to ask. Returns 20 + how nice it was.
            crate::usage::nice(scheduler::current_tid(), arg0, arg1)
        }
        SYS_CPU_LIMIT => {
            // arg0 = soft, arg1 = hard, in seconds of processor time
            // (u64::MAX none); arg2 = where to write the two it was, or 0;
            // arg3 = 1 to change nothing.
            crate::usage::cpu_limit(scheduler::current_tid(), arg0, arg1, arg2, arg3 == 1)
        }
        SYS_DEVICE_CLAIM => {
            // arg0 = bus << 8 | device << 3 | function, a device the caller
            // holds; arg1 = 1 to ask how many times the device reached for
            // what it may not.
            crate::iommu::claim(scheduler::current_tid(), arg0, arg1 == 1)
        }
        SYS_SIG_WAIT => {
            // arg0 = the signals to take, arg1 = how long to wait for one, a
            // span (0 not at all, u64::MAX for ever), arg2 = where to say
            // who raised it, or 0 — with arg3 = 1, everything that came with
            // it. Returns the signal, or 0 if none came.
            crate::signal::wait_for(scheduler::current_tid(), arg0, arg1, arg2, arg3 == 1)
        }
        SYS_SIG_STACK => {
            // arg0 = where, arg1 = how long: the stack for handlers that
            // ask to be run on one. No length is no stack; arg0 = u64::MAX
            // changes nothing. arg2 = where to write the one it replaces,
            // where and how long, or 0.
            crate::signal::stack(scheduler::current_tid(), arg0, arg1, arg2)
        }
        SYS_SYSCALL_TRAP => {
            // arg0 = where the caller's program makes its system calls from,
            // arg1 = how many bytes: a call made from anywhere else raises
            // SIGSYS rather than being made. arg1 = 0 for calls from anywhere,
            // as every program begins. The caller's own program only, and
            // the user half only: it is a way for a program to answer its
            // own calls, and changes nothing anybody else can see.
            let (from, len) = (arg0 as usize, arg1 as usize);
            let Some(to) = from.checked_add(len) else { return u64::MAX };
            if len != 0
                && (from < crate::paging::USER_MIN_ADDR as usize
                    || to > crate::paging::USER_ADDR_LIMIT as usize)
            {
                return u64::MAX;
            }
            let (from, to) = if len == 0 { (0, 0) } else { (from, to) };
            if crate::fdtable::set_trap(scheduler::current_tid(), from, to) { 0 } else { u64::MAX }
        }
        SYS_SIG_RETURN => {
            // arg0 = the record a handler was entered with. Does not come
            // back here: the task goes on from where the record says.
            crate::signal::ret(arg0)
        }
        SYS_SIG_RAISE | SYS_SIG_QUEUE => {
            // arg0 = a task of the program to signal — or, with arg2 = 1, the
            // program's process id — and arg1 = the signal, or 0 to ask only
            // whether it could be. Whoever may kill a task may signal it:
            // TaskMgmt for the target, or the same user.
            //
            // SYS_SIG_QUEUE is the same with a value in arg2, and so what
            // arg2 says in arg3: what came with it says it was queued, and
            // a real-time signal waits behind one of its number already
            // waiting. Either answers 0xFFFF_FFFE when one cannot.
            let caller = scheduler::current_tid();
            let (arg2, value) = if nr == SYS_SIG_QUEUE { (arg3, arg2) } else { (arg2, 0) };
            if nr == SYS_SIG_QUEUE && arg2 & RAISE_GROUP != 0 {
                return u64::MAX;
            }
            let caller_uid = scheduler::current_task_uid();
            let may = |tid: usize| {
                crate::cap::task_has_task_mgmt(caller, tid)
                    || scheduler::task_uid_gid(tid).is_ok_and(|(uid, _)| uid == caller_uid)
            };
            if arg2 & RAISE_GROUP != 0 {
                // arg0 = a process group, or 0 for the caller's own. For
                // every program in it that the caller may signal, and the
                // caller's own last: what the signal does to it may be the
                // last thing the caller does.
                if arg1 > crate::signal::NSIG as u64 {
                    return u64::MAX;
                }
                let group = if arg0 == 0 { crate::job::pgid_of(caller) } else { arg0 };
                let mine = scheduler::pid_of(caller);
                let (mut own, mut found, mut any) = (None, false, false);
                let info = crate::signal::Info::from_task(caller, crate::signal::SI_USER, 0);
                let mut from = 0;
                while let Some(tid) = crate::job::next_member(group, &mut from) {
                    found = true;
                    if !may(tid) {
                        continue;
                    }
                    any = true;
                    if arg1 == 0 {
                        continue;
                    }
                    if scheduler::pid_of(tid) == mine {
                        own = Some(tid);
                    } else {
                        let _ = crate::signal::raise_with(tid, arg1 as u8, info);
                    }
                }
                if !found {
                    return u64::MAX;
                }
                if !any {
                    return NOT_ALLOWED;
                }
                if let Some(tid) = own {
                    let _ = crate::signal::raise_with(tid, arg1 as u8, info);
                }
                return 0;
            }
            let by_pid = arg2 & RAISE_BY_PID != 0;
            let tid = if by_pid {
                match scheduler::task_of_pid(arg0) {
                    Some(tid) => tid,
                    None => return u64::MAX,
                }
            } else {
                arg0 as usize
            };
            if !may(tid) {
                return u64::MAX;
            }
            if !scheduler::task_is_live(tid) {
                // By its process id, a program that has ended and not been
                // collected is still there to be named, and there is nothing
                // left of it to tell. A task id says nothing of the kind: it
                // may be anybody's by now.
                return if by_pid { 0 } else { u64::MAX };
            }
            if arg1 == 0 {
                return 0;
            }
            if arg1 > crate::signal::NSIG as u64 {
                return u64::MAX;
            }
            let thread = arg2 & RAISE_THREAD != 0 && !by_pid;
            let code = match (nr == SYS_SIG_QUEUE, thread) {
                (true, _) => crate::signal::SI_QUEUE,
                (false, true) => crate::signal::SI_TKILL,
                (false, false) => crate::signal::SI_USER,
            };
            let info = crate::signal::Info::from_task(caller, code, value);
            let raised = if thread {
                crate::signal::raise_task(tid, arg1 as u8, info)
            } else {
                crate::signal::raise_with(tid, arg1 as u8, info)
            };
            match raised {
                Ok(()) => 0,
                Err(crate::signal::NotRaised::Full) => crate::pipe::WOULD_BLOCK,
                Err(crate::signal::NotRaised::Nobody) => u64::MAX,
            }
        }
        SYS_PGROUP => {
            // arg0 = what is asked; arg1 = a process id, 0 for the caller's
            // own; arg2 = a process group, for the one that sets it.
            let caller = scheduler::current_tid();
            let of = |pid: u64| if pid == 0 { Some(caller) } else { scheduler::task_of_pid(pid) };
            match arg0 {
                PGROUP_GET => of(arg1).map_or(u64::MAX, crate::job::pgid_of),
                PGROUP_SET => match crate::job::set_pgid(caller, arg1, arg2) {
                    Ok(()) => 0,
                    Err(crate::job::Refused::NoSuch) => u64::MAX,
                    Err(crate::job::Refused::NotAllowed) => NOT_ALLOWED,
                },
                SESSION_GET => of(arg1).map_or(u64::MAX, crate::job::sid_of),
                SESSION_NEW => crate::job::set_sid(caller).unwrap_or(NOT_ALLOWED),
                _ => u64::MAX,
            }
        }
        SYS_SIG_ALARM => {
            // arg0 = how long from now until SIGALRM is raised for the
            // caller's program, no time for no alarm; arg1 = how long between
            // repeats after that, none for none; arg2 = 1 to ask how it
            // stands and no more; arg3 = where to write how it stood, in
            // nanoseconds — what was left of it and what it repeated at — or
            // 0. Answers with both in ticks, each rounded up and no more
            // than thirty-two bits of it: sixteen months.
            if arg3 != 0 && (arg3 & 7 != 0 || !validate_user_ptr_mut(arg3, 16)) {
                return u64::MAX;
            }
            let was = crate::signal::alarm(
                scheduler::current_tid(),
                crate::clock::span(arg0),
                crate::clock::span(arg1),
                arg2 & ALARM_ASK != 0,
            );
            match was {
                Some((left, every)) => {
                    if arg3 != 0 {
                        let _ua = crate::cpu::UserAccess::begin();
                        unsafe { *(arg3 as *mut [u64; 2]) = [left, every] };
                    }
                    let ticks = |ns: u64| crate::clock::ticks_of(ns).min(u32::MAX as u64);
                    ticks(left) | ticks(every) << 32
                }
                None => u64::MAX,
            }
        }
        SYS_SIG_TAKE => {
            // arg0 = where in the caller's memory to say that there is
            // something to take, from now on; 0 leaves that as it is.
            if arg0 != 0 && (arg0 & 3 != 0 || !validate_user_ptr_mut(arg0, 4)) {
                return u64::MAX;
            }
            crate::signal::take(scheduler::current_tid(), arg0 as usize)
        }
        SYS_SET_MEM_LIMIT => {
            // arg0 = tid, arg1 = limit in pages (0 = unlimited)
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let limit = arg1 as usize;
            match scheduler::set_mem_limit(tid, limit) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_SHMEM_CREATE => {
            // arg0 = pages
            crate::shmem::create(arg0 as usize)
        }
        SYS_SHMEM_MAP => {
            // arg0 = handle, arg1 = vaddr
            match crate::shmem::map(arg0 as usize, arg1 as usize) {
                u64::MAX => u64::MAX,
                _ => 0,
            }
        }
        SYS_SHMEM_GRANT => {
            // arg0 = handle, arg1 = target tid
            crate::shmem::grant(arg0 as usize, arg1 as usize)
        }
        SYS_SHMEM_UNMAP => {
            // arg0 = handle, arg1 = vaddr
            crate::shmem::unmap(arg0 as usize, arg1 as usize)
        }
        SYS_SHMEM_DESTROY => {
            // arg0 = handle
            crate::shmem::destroy(arg0 as usize)
        }
        SYS_CAP_TRANSFER => {
            // arg0 = dest tid, arg1 = capability bits to transfer
            // Any task can transfer caps it holds — no CAP_TASK_MGMT required.
            let dest = arg0 as usize;
            let caps = arg1 as u32;
            let caller_caps = scheduler::current_task_caps();
            // Sender must hold all bits being transferred
            if caps & !caller_caps != 0 {
                return u64::MAX;
            }
            match scheduler::grant_cap(dest, caps) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_NOTIFY => {
            // arg0 = dest tid, arg1 = badge (bits to OR into notification word)
            let dest = arg0 as usize;
            let badge = arg1;
            if !crate::cap::task_has_endpoint(scheduler::current_tid(), dest) {
                return deny_ipc(scheduler::current_tid(), dest, b"notify");
            }
            match crate::ipc::sys_notify(dest, badge) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_SET_PAGER => {
            // arg0 = tid, arg1 = pager_tid
            if !crate::cap::task_has_task_mgmt(scheduler::current_tid(), 0) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let pager_tid = arg1 as usize;
            match scheduler::set_pager(tid, pager_tid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GET_UID => {
            let uid = scheduler::current_task_uid();
            let gid = scheduler::current_task_gid();
            ((uid as u64) << 32) | (gid as u64)
        }
        SYS_SET_UID => {
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let uid = arg1 as u32;
            match scheduler::set_task_uid(tid, uid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_SET_GID => {
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let tid = arg0 as usize;
            let gid = arg1 as u32;
            match scheduler::set_task_gid(tid, gid) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_GET_TUID => {
            let tid = arg0 as usize;
            match scheduler::task_uid_gid(tid) {
                Ok((uid, gid)) => ((uid as u64) << 32) | (gid as u64),
                Err(()) => u64::MAX,
            }
        }
        SYS_GROUPS => {
            // arg0 = op, arg1 = a task (0 for the caller), arg2 = where the
            // ids are or go, arg3 = how many.
            let caller = scheduler::current_tid();
            let tid = if arg1 == 0 { caller } else { arg1 as usize };
            let count = arg3 as usize;
            match arg0 {
                GROUPS_GET => {
                    let mut groups = [0u32; crate::task::MAX_GROUPS];
                    let Ok(n) = scheduler::task_groups(tid, &mut groups) else {
                        return u64::MAX;
                    };
                    // As many as there is room for; the answer is how many
                    // there are, so a caller with no room at all can ask.
                    let give = n.min(count);
                    if give > 0 {
                        if !validate_user_ptr_mut(arg2, (give * 4) as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        for (i, group) in groups[..give].iter().enumerate() {
                            unsafe { core::ptr::write_unaligned((arg2 as *mut u32).add(i), *group) };
                        }
                    }
                    n as u64
                }
                GROUPS_SET => {
                    if !crate::cap::task_has_set_uid(caller)
                        || count > crate::task::MAX_GROUPS
                        || !(tid == caller || may_prepare(caller, tid))
                    {
                        return u64::MAX;
                    }
                    let mut groups = [0u32; crate::task::MAX_GROUPS];
                    if count > 0 {
                        if !validate_user_ptr(arg2, (count * 4) as u64) {
                            return u64::MAX;
                        }
                        let _ua = crate::cpu::UserAccess::begin();
                        for (i, group) in groups[..count].iter_mut().enumerate() {
                            *group = unsafe { core::ptr::read_unaligned((arg2 as *const u32).add(i)) };
                        }
                    }
                    match scheduler::set_task_groups(tid, &groups[..count]) {
                        Ok(()) => 0,
                        Err(()) => u64::MAX,
                    }
                }
                _ => u64::MAX,
            }
        }
        SYS_IDENTIFY => {
            // arg0 = a task in a call to the caller, arg1 = that task or a
            // child it is preparing, arg2 = uid << 32 | gid (as SYS_GET_UID
            // answers), arg3 = the groups it is in besides, arg4 = how many.
            //
            // Saying who a task is, is something done to it; and as with a
            // descriptor put in its table or a capability in its CSpace, it
            // consents by being in a call to whoever does it. Whose child
            // the target is, is checked here and not by the caller
            // beforehand: a TID is recycled, and "this was its child a moment
            // ago" names whatever has the number now.
            let me = scheduler::current_tid();
            let client = arg0 as usize;
            let target = arg1 as usize;
            let count = arg4 as usize;
            if !crate::cap::task_has_set_uid(me)
                || client == me
                || !crate::ipc::is_calling(client, me)
                || !(target == client || may_prepare(client, target))
                || count > crate::task::MAX_GROUPS
            {
                return u64::MAX;
            }
            let mut groups = [0u32; crate::task::MAX_GROUPS];
            if count > 0 {
                if !validate_user_ptr(arg3, (count * 4) as u64) {
                    return u64::MAX;
                }
                let _ua = crate::cpu::UserAccess::begin();
                for (i, group) in groups[..count].iter_mut().enumerate() {
                    *group = unsafe { core::ptr::read_unaligned((arg3 as *const u32).add(i)) };
                }
            }
            match scheduler::identify(target, (arg2 >> 32) as u32, arg2 as u32, &groups[..count]) {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_TASK_KILL => {
            // arg0 = tid to kill. Requires TaskMgmt cap for target or same UID.
            let tid = arg0 as usize;
            let caller = scheduler::current_tid();
            let caller_uid = scheduler::current_task_uid();
            let has_cap = crate::cap::task_has_task_mgmt(caller, tid);
            let same_uid = scheduler::task_uid_gid(tid)
                .map(|(uid, _)| uid == caller_uid)
                .unwrap_or(false);
            if !has_cap && !same_uid {
                return u64::MAX;
            }
            // A task of the caller's own program is a thread it is ending;
            // anybody else's is a program.
            let own = scheduler::space_of_task(tid) != 0
                && scheduler::space_of_task(tid) == scheduler::space_of_task(caller);
            let ended = if own { scheduler::kill_task(tid) } else { scheduler::kill_program(tid) };
            match ended {
                Ok(()) => 0,
                Err(()) => u64::MAX,
            }
        }
        SYS_TASK_NEXT => {
            // arg0 = a task id. The first task at or past it that has not been
            // taken apart, living or dead, or u64::MAX if none is: what
            // SYS_TASK_INFO says about each number, one call a task.
            scheduler::next_task(arg0 as usize).map_or(u64::MAX, |t| t as u64)
        }
        SYS_TASK_INFO => {
            // arg0 = tid. Returns packed info or u64::MAX if no task.
            // bits [3:0] = state (0=Ready,1=Running,2=Blocked,3=Dead,4=Stopped)
            // bits [31:4] = parent_tid
            // bits [63:32] = uid
            let tid = arg0 as usize;
            match scheduler::task_info(tid) {
                Some((state, uid, _gid, parent)) => {
                    let state_bits = match state {
                        crate::task::TaskState::Dead => 3,
                        // Whatever else it is, it is not running until its
                        // program is continued.
                        _ if crate::job::is_stopped(tid) => 4,
                        crate::task::TaskState::Ready => 0u64,
                        crate::task::TaskState::Running => 1,
                        crate::task::TaskState::Blocked => 2,
                    };
                    state_bits | ((parent as u64) << 4) | ((uid as u64) << 32)
                }
                None => u64::MAX,
            }
        }
        SYS_SIGNAL => {
            // arg0 = tid, arg1 = signal bits. Same permissions as sys_task_kill.
            let tid = arg0 as usize;
            let sig = arg1;
            let caller = scheduler::current_tid();
            let caller_uid = scheduler::current_task_uid();
            let has_cap = crate::cap::task_has_task_mgmt(caller, tid);
            let same_uid = scheduler::task_uid_gid(tid)
                .map(|(uid, _)| uid == caller_uid)
                .unwrap_or(false);
            if !has_cap && !same_uid {
                return u64::MAX;
            }
            match crate::ipc::sys_signal(tid, sig) {
                Ok(()) => 0,
                Err(_) => u64::MAX,
            }
        }
        SYS_CAP_MINT => {
            // arg0 = slot, arg1 = type, arg2 = param0, arg3 = param1
            // Create a root cap in caller's slot (requires existing authority)
            let slot = arg0 as usize;
            // A type is a byte: 0x101 is not 1.
            if arg1 > 0xFF {
                return u64::MAX;
            }
            let cap_type_raw = arg1 as u8;
            let param0 = arg2;
            let param1 = arg3;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let cap_type = match cap_type_raw {
                1 => crate::cap::CapType::IoPort,
                2 => crate::cap::CapType::PhysRange,
                3 => crate::cap::CapType::Irq,
                4 => crate::cap::CapType::TaskMgmt,
                5 => crate::cap::CapType::PhysAlloc,
                6 => crate::cap::CapType::SetUid,
                // 7, a set of task IDs, was withdrawn at 2.0.
                8 => crate::cap::CapType::Endpoint,
                9 => crate::cap::CapType::MemObject,
                10 => crate::cap::CapType::DeviceMemory,
                11 => crate::cap::CapType::Clock,
                12 => crate::cap::CapType::Power,
                13 => crate::cap::CapType::Swap,
                14 => crate::cap::CapType::PciDevice,
                15 => crate::cap::CapType::NetAdmin,
                16 => crate::cap::CapType::RealTime,
                _ => return u64::MAX,
            };
            let tid = scheduler::current_tid();
            let root = crate::cap::root_of(tid);
            // The slot's *current* count of revocations, made if it has none:
            // hardcoding 0 meant that after a single sys_cap_revoke on this
            // slot every capability minted there was born already invalid.
            let Some(generation) = crate::cap::generation_for(root, slot) else {
                return u64::MAX;
            };
            let minted = crate::cap::with_cspace(tid, |cs| {
                let (param0, param1) = if cap_type == crate::cap::CapType::Endpoint {
                    // Asked for by TID, recorded by the endpoint's number, and
                    // minted on ownership rather than from a capability held.
                    crate::cap::endpoint_to_mint(cs, tid, param0 as usize).map(|number| (number, 0))?
                } else if crate::cap::can_mint(cs, cap_type, param0, param1) {
                    // The caller already holds a capability that covers what
                    // it is minting. This used to be skipped for UID 0.
                    (param0, param1)
                } else {
                    return None;
                };
                // Target slot must be empty
                if cs.get(slot).cap_type != crate::cap::CapType::Empty {
                    return None;
                }
                let cap = crate::cap::CapSlot { cap_type, generation, root_slot: slot as u16, root, param0, param1 };
                cs.set(slot, cap).then_some(())
            });
            if minted.flatten().is_none() {
                return u64::MAX;
            }
            0
        }
        SYS_CAP_GRANT => {
            // arg0 = dest_tid, arg1 = src_slot, arg2 = dest_slot or ANY_SLOT
            // Delegate cap to another task (with attenuation tracking).
            // Returns 0, or for ANY_SLOT the slot it is in.
            let dest_tid = arg0 as usize;
            let src_slot = arg1 as usize;
            let any_slot = arg2 == ANY_SLOT;
            let dest_slot = arg2 as usize;
            if src_slot >= crate::cap::MAX_CAPS
                || (!any_slot && dest_slot >= crate::cap::MAX_CAPS)
            {
                return u64::MAX;
            }
            let caller_tid = scheduler::current_tid();

            // Who may put a capability into somebody else's CSpace.
            //
            // A grant can never *raise* the destination's authority — it only
            // ever adds, and what it adds the granter already held. What it can
            // do is fill every slot, and a service that can no longer
            // receive a capability can no longer be handed the display, a file,
            // or an endpoint. Unrestricted, that is a denial of service any
            // task can perform on any other.
            //
            // Two things really do this, and the rule is written from both:
            //
            // - A spawner granting to its child, which holds `TaskMgmt` over
            //   it. `init`, `login`, the shell and the compositor all do this.
            // - A server answering a request, which does not. The framebuffer
            //   device mints a derived `PhysRange` and grants it into a
            //   claimant it never spawned — while that claimant is blocked in
            //   `sys_call` to it.
            //
            // - A server *returning* something. The framebuffer device takes the
            //   display back from whoever has it and hands it to whoever had it
            //   before, which is a task that is not calling anybody: it is
            //   sitting in its own loop waiting to be told. The console gets
            //   its display back this way, and a rule without this arm boots to
            //   a black screen the moment a compositor exits.
            //
            // So: authority over the destination, or its consent. Consent is
            // either a call in progress — being blocked in one *is* the asking —
            // or a standing `Endpoint` naming the granter, which is the
            // destination having already said it is willing to talk to this
            // task. Neither can be manufactured by a stranger: a service holds
            // endpoints to the things it calls, not to everything that calls
            // it, so nobody can push a capability into the nameserver.
            // - A spawner filling in a child it has built and not yet
            //   started, which is the same window `may_prepare` describes: the
            //   destination cannot object because it does not yet exist to
            //   anyone else.
            if !crate::cap::task_has_task_mgmt(caller_tid, dest_tid)
                && !crate::ipc::is_calling(dest_tid, caller_tid)
                && !crate::cap::task_has_endpoint(dest_tid, caller_tid)
                && !may_prepare(caller_tid, dest_tid)
            {
                return u64::MAX;
            }
            let Some(src_cap) = crate::cap::slot(caller_tid, src_slot) else {
                return u64::MAX;
            };
            if src_cap.cap_type as u8 == crate::cap::CapType::Empty as u8 {
                return u64::MAX;
            }
            // A revoked cap must not be re-delegatable.
            if !crate::cap::slot_is_valid(&src_cap) {
                return u64::MAX;
            }
            let Some(derived) = crate::cap::derive(caller_tid, src_slot, &src_cap) else {
                return u64::MAX;
            };
            let landed = crate::cap::with_cspace(dest_tid, |cs| {
                let slot = if any_slot {
                    crate::cap::receive_slot(cs, &src_cap)?
                } else if cs.get(dest_slot).cap_type == crate::cap::CapType::Empty {
                    dest_slot
                } else {
                    return None;
                };
                // An endpoint the destination already holds is not copied
                // again; the slot it is in is the answer.
                if cs.get(slot).cap_type == crate::cap::CapType::Empty && !cs.set(slot, derived) {
                    return None;
                }
                Some(slot)
            });
            match landed.flatten() {
                Some(slot) if any_slot => slot as u64,
                Some(_) => 0,
                None => u64::MAX,
            }
        }
        SYS_CAP_TAKE => {
            // arg0 = the caller whose offer to take, arg1 = slot or ANY_SLOT.
            // Returns the slot it is in.
            //
            // A capability arrives in a server's CSpace only if the server
            // takes it: an offer is the caller's, and it sits on the call until
            // the call ends. Nothing can fill a server's slots uninvited.
            let taker = scheduler::current_tid();
            let client = arg0 as usize;
            let any_slot = arg1 == ANY_SLOT;
            let want = arg1 as usize;
            if !any_slot && want >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let Some(from) = crate::ipc::offered_to(client, taker) else {
                return u64::MAX;
            };
            let Some(offered) = crate::cap::slot(client, from) else {
                return u64::MAX;
            };
            if !crate::cap::slot_is_valid(&offered) {
                return u64::MAX;
            }
            let Some(derived) = crate::cap::derive(client, from, &offered) else {
                return u64::MAX;
            };
            let landed = crate::cap::with_cspace(taker, |cs| {
                let slot = if any_slot {
                    crate::cap::receive_slot(cs, &offered)?
                } else if cs.get(want).cap_type == crate::cap::CapType::Empty {
                    want
                } else {
                    return None;
                };
                if cs.get(slot).cap_type == crate::cap::CapType::Empty && !cs.set(slot, derived) {
                    return None;
                }
                Some(slot)
            });
            let Some(slot) = landed.flatten() else {
                return u64::MAX;
            };
            crate::ipc::withdraw_offer(client);
            slot as u64
        }
        SYS_CAP_REVOKE => {
            // arg0 = slot
            // Bump generation, invalidate all derived caps
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let tid = scheduler::current_tid();
            crate::cap::revoke(tid, slot);
            0
        }
        SYS_CAP_INSPECT => {
            // arg0 = slot
            // Return packed info: type in bits [7:0], param0 in upper bits
            // For full inspection, use two calls or a buffer.
            // Simple: return type | (param0 << 8) truncated to u64
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            let Some(cap) = crate::cap::slot(scheduler::current_tid(), slot) else {
                return u64::MAX;
            };
            // Pack: [7:0]=type, [23:8]=param0 low 16, [39:24]=param1 low 16
            cap.cap_type as u64 | ((cap.param0 & 0xFFFF) << 8) | ((cap.param1 & 0xFFFF) << 24)
        }
        SYS_CAP_READ => {
            // arg0 = tid, arg1 = slot, arg2 = out: type, param0, param1, valid
            //
            // Anybody may read their own; reading another task's takes the
            // authority to manage it, which already covers far more than
            // knowing what it may do.
            let caller = scheduler::current_tid();
            let tid = arg0 as usize;
            let slot = arg1 as usize;
            if slot >= crate::cap::MAX_CAPS || !validate_user_ptr_mut(arg2, 32) {
                return u64::MAX;
            }
            if tid != caller && !crate::cap::task_has_task_mgmt(caller, tid) {
                return u64::MAX;
            }
            let (Some(cap), Some(room)) = (crate::cap::slot(tid, slot), crate::cap::room_of(tid)) else {
                return u64::MAX;
            };
            let out = [
                cap.cap_type as u64,
                cap.param0,
                cap.param1,
                crate::cap::slot_is_valid(&cap) as u64,
            ];
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), arg2 as *mut u64, 4) };
            // How many slots the space has room for, past which every one is
            // empty: what a walk of all of them goes to. It answered 0.
            room as u64
        }
        SYS_CAP_DELETE => {
            // arg0 = slot
            // Delete cap from own CSpace
            let slot = arg0 as usize;
            if slot >= crate::cap::MAX_CAPS {
                return u64::MAX;
            }
            // A slot past the room the space has is empty already.
            let emptied = crate::cap::with_cspace(scheduler::current_tid(), |cs| {
                if let Some(cap) = cs.get_mut(slot) {
                    *cap = crate::cap::CapSlot::empty();
                }
            });
            if emptied.is_none() {
                return u64::MAX;
            }
            0
        }
        SYS_SET_USER_CAPS => {
            // arg0 = uid, arg1 = capability bitmask
            // Requires CAP_SET_UID (root gets it via UID bypass)
            if !crate::cap::task_has_set_uid(scheduler::current_tid()) {
                return u64::MAX;
            }
            let uid = arg0 as u32;
            let caps = arg1 as u32;
            crate::cap::set_user_caps(uid, caps);
            0
        }
        SYS_GET_USER_CAPS => {
            // arg0 = uid
            let uid = arg0 as u32;
            crate::cap::user_caps(uid) as u64
        }
        _ => u64::MAX,
    }
}

// Syscall entry in AT&T syntax.
//
// On `syscall` instruction: RCX = user RIP, R11 = user RFLAGS.
// RSP is unchanged (still user RSP). Interrupts are cleared by SFMASK.
//
// `swapgs` finds this processor's own state (`percpu.rs`): %gs:0 is a word
// to keep the caller's RSP in, %gs:8 the top of the running task's kernel
// stack, and %gs:16 a word for the caller's R9 — a sixth argument no call
// of this kernel's takes, which the shuffle below writes over, and which a
// call a program's trap turns into a signal has to give back.
//
// After saving user context, we shuffle registers to match the C ABI for
// syscall_dispatch(nr, arg0, arg1, arg2, arg3, arg4), then sysret back.
//
// User convention: RAX=nr, RDI=arg0, RSI=arg1, RDX=arg2, R10=arg3, R8=arg4
// C ABI:          RDI=nr, RSI=arg0, RDX=arg1, RCX=arg2, R8=arg3,  R9=arg4
core::arch::global_asm!(
    ".global syscall_entry",
    "syscall_entry:",
    "    swapgs",
    "    movq %r9, %gs:16",            // keep user R9
    "    movq %rsp, %gs:0",            // save user RSP
    "    movq %gs:8, %rsp",            // load kernel RSP

    // Save user context on kernel stack
    "    pushq %gs:0",                 // user RSP
    "    pushq %r11",                  // user RFLAGS
    "    pushq %rcx",                  // user RIP

    // Save registers we need to preserve across the call
    "    pushq %rbx",
    "    pushq %rbp",
    "    pushq %r12",
    "    pushq %r13",
    "    pushq %r14",
    "    pushq %r15",

    // Save syscall args (we need them after setting up C ABI)
    "    pushq %rdi",                  // arg0
    "    pushq %rsi",                  // arg1

    // Interrupts stay off: `syscall_dispatch` turns them on once it holds
    // the kernel lock.

    // Set up 6-arg C ABI: syscall_dispatch(nr, arg0, arg1, arg2, arg3, arg4)
    // User regs: rax=nr, rdi=arg0, rsi=arg1, rdx=arg2, r10=arg3, r8=arg4
    // Shuffle order matters — move destinations that overlap sources last
    "    movq %r8, %r9",               // arg4 → r9 (6th C arg) — before r8 overwrite
    "    movq %r10, %r8",              // arg3 → r8 (5th C arg)
    "    movq %rdx, %rcx",             // arg2 → rcx (4th C arg)
    "    movq %rsi, %rdx",             // arg1 → rdx (3rd C arg)
    "    movq %rdi, %rsi",             // arg0 → rsi (2nd C arg)
    "    movq %rax, %rdi",             // nr → rdi (1st C arg)
    "    call syscall_dispatch",

    // Return value is in %rax.
    // Scrub the caller-saved scratch registers the ABI lets us clobber: they
    // still hold kernel values here and sysret would hand them to ring 3.
    // (rcx/r11 are overwritten below with the user's saved RIP/RFLAGS,
    // rsi/rdi are restored from the user's own saved args.)
    "    xorl %edx, %edx",
    "    xorl %r8d, %r8d",
    "    xorl %r9d, %r9d",
    "    xorl %r10d, %r10d",

    // Restore saved arg registers (we pushed rdi, rsi)
    "    popq %rsi",
    "    popq %rdi",

    // Restore callee-saved registers
    "    popq %r15",
    "    popq %r14",
    "    popq %r13",
    "    popq %r12",
    "    popq %rbp",
    "    popq %rbx",

    // Restore user context
    "    cli",                          // disable interrupts before sysret
    "    popq %rcx",                   // user RIP
    "    popq %r11",                   // user RFLAGS
    "    popq %rsp",                   // user RSP
    "    swapgs",
    "    sysretq",
    options(att_syntax)
);

/// Go back to user mode with a whole register frame, as a forked child does.
///
/// [`enter_usermode`] starts a program: one entry point, one stack, one
/// argument. This *resumes* one — every register the system call stub would
/// have restored, and RAX set to zero, because the child's only difference
/// from its parent is what `fork` answered.
///
/// # Safety
/// `frame` must be a `UserFrame` in memory this address space has mapped, and
/// its RIP and RSP must be a user address.
pub unsafe fn enter_usermode_frame(frame: *const crate::task::UserFrame) -> ! { unsafe {
    // Out of the kernel, as in `enter_usermode`.
    core::arch::asm!("cli", options(nostack, nomem));
    crate::usage::leaving(scheduler::current_tid());
    crate::klock::release();
    core::arch::asm!(
        // The iretq frame, built from the saved one.
        "pushq $0x2B",                 // SS
        "pushq 80(%rcx)",              // RSP
        "pushq 72(%rcx)",              // RFLAGS
        "pushq $0x33",                 // CS
        "pushq 64(%rcx)",              // RIP
        // Everything the stub would have popped. RCX is the frame until the
        // last of them, and iretq does not care what is in it.
        "movq 0(%rcx), %rsi",
        "movq 8(%rcx), %rdi",
        "movq 16(%rcx), %r15",
        "movq 24(%rcx), %r14",
        "movq 32(%rcx), %r13",
        "movq 40(%rcx), %r12",
        "movq 48(%rcx), %rbp",
        "movq 56(%rcx), %rbx",
        "xorl %eax, %eax",             // fork returns 0 in the child
        "swapgs",
        "iretq",
        in("rcx") frame,
        options(att_syntax, noreturn)
    );
}}

/// Go back to user mode with every register given: what `SYS_SIG_RETURN`
/// does, for a task that a handler interrupted anywhere at all — in the
/// middle of arithmetic, where RCX and R11 are its own and `sysret` would
/// take them.
///
/// # Safety
/// `regs` are a program's: RIP and RSP in the user half, RFLAGS what a
/// program may have.
pub fn enter_usermode_regs(regs: &crate::signal::Regs) -> ! {
    // The call that led here checked the record; it is over.
    scheduler::unpin();
    unsafe {
        // Out of the kernel, as in `enter_usermode`.
        core::arch::asm!("cli", options(nostack, nomem));
        crate::usage::leaving(scheduler::current_tid());
        crate::klock::release();
        core::arch::asm!(
            "pushq $0x2B",                 // SS
            "pushq 136(%rax)",             // RSP
            "pushq 128(%rax)",             // RFLAGS
            "pushq $0x33",                 // CS
            "pushq 120(%rax)",             // RIP
            "movq 8(%rax), %rbx",
            "movq 16(%rax), %rcx",
            "movq 24(%rax), %rdx",
            "movq 32(%rax), %rsi",
            "movq 40(%rax), %rdi",
            "movq 48(%rax), %rbp",
            "movq 56(%rax), %r8",
            "movq 64(%rax), %r9",
            "movq 72(%rax), %r10",
            "movq 80(%rax), %r11",
            "movq 88(%rax), %r12",
            "movq 96(%rax), %r13",
            "movq 104(%rax), %r14",
            "movq 112(%rax), %r15",
            "movq 0(%rax), %rax",
            "swapgs",
            "iretq",
            in("rax") regs.as_ptr(),
            options(att_syntax, noreturn)
        );
    }
}

/// Read a timer: the number of times it has fired, as a `u64`, waiting for
/// the first if it has not fired yet. That is the shape Linux gives it, and
/// what a toolkit's event loop reads to find out how many blinks it missed.
/// Read a counter, waiting for it to be something.
///
/// Eight bytes, because that is what the value is; a shorter buffer is an
/// error rather than a truncation, which is what Linux answers too.
fn event_read(ev: usize, ptr: *mut u8, max_len: usize) -> u64 {
    if max_len < 8 {
        return u64::MAX;
    }
    loop {
        if let Some(n) = crate::eventfd::take(ev) {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(ptr as *mut u64, n) };
            drop(_ua);
            return 8;
        }
        // A signal the kernel runs a handler for ends the wait, or stops
        // it beginning — `wait` looks too, in the same step as it parks.
        if crate::signal::ends_wait(scheduler::current_tid()) {
            return crate::signal::INTERRUPTED;
        }
        if !crate::eventfd::wait(ev)
            && !crate::eventfd::readable(ev)
            && !crate::signal::ends_wait(scheduler::current_tid())
        {
            // Not waited on and not readable: it has gone.
            return u64::MAX;
        }
    }
}

/// Add to a counter. `block` decides what happens when it is full, which is
/// only ever the case at 2^64 - 1 and so is close to unreachable.
fn event_write(ev: usize, ptr: *const u8, len: usize, block: bool) -> u64 {
    if len < 8 {
        return u64::MAX;
    }
    let n = {
        let _ua = crate::cpu::UserAccess::begin();
        unsafe { core::ptr::read_unaligned(ptr as *const u64) }
    };
    if n == u64::MAX || n == 0 {
        // u64::MAX is reserved so that it is always an error rather than a
        // wrap, and a zero add would be a wake-up that woke nobody.
        return u64::MAX;
    }
    loop {
        if crate::eventfd::add(ev, n) {
            return 8;
        }
        if !block {
            return crate::pipe::WOULD_BLOCK;
        }
        if crate::signal::ends_wait(scheduler::current_tid()) {
            return crate::signal::INTERRUPTED;
        }
        if !crate::eventfd::wait(ev) && !crate::signal::ends_wait(scheduler::current_tid()) {
            return u64::MAX;
        }
    }
}

fn timer_read(timer: usize, ptr: *mut u8, max_len: usize) -> u64 {
    if max_len < 8 {
        return u64::MAX;
    }
    loop {
        if let Some(n) = crate::timerfd::take(timer) {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::write_unaligned(ptr as *mut u64, n) };
            drop(_ua);
            return 8;
        }
        if crate::signal::ends_wait(scheduler::current_tid()) {
            return crate::signal::INTERRUPTED;
        }
        if !crate::timerfd::wait(timer) {
            // Either it fired while we were looking, or there was no room to
            // be recorded — the loop takes the first and this returns for the
            // second rather than sleeping unwoken. Or a signal came, which
            // the loop answers.
            if crate::timerfd::pending(timer) == 0 && !crate::signal::ends_wait(scheduler::current_tid()) {
                return u64::MAX;
            }
        }
    }
}

/// Read from a terminal, waiting for something to arrive.
///
/// Through a kernel buffer rather than into the caller's pages directly: the
/// copy happens with the SMAP window open, and that window must not be held
/// across the wait — which is a reschedule, and RFLAGS.AC travels with the
/// task that owns it.
/// A read of a terminal by a program its session has put behind: not a read
/// at all. What is typed is for whoever is in front, and a job that reads
/// from the background is stopped until it is brought forward (SIGTTIN).
///
/// `None` if the caller is in front, or the terminal is not its session's
/// to be behind — then the read is a read. Otherwise the answer to give
/// instead of reading: the read failed, for a program that ignores the
/// signal or has nobody to continue it; or it was interrupted, for one that
/// has been stopped and started again, or has a handler to run. A program
/// that asks again is looked at again.
fn read_from_behind(pty: usize) -> Option<u64> {
    let me = scheduler::current_tid();
    let (session, front) = crate::pty::job(pty)?;
    let group = crate::job::pgid_of(me);
    if session == 0 || crate::job::sid_of(me) != session || group == front {
        return None;
    }
    let ttin = crate::signal::SIGTTIN;
    // Ignored or held back, the signal cannot stop it, and the read fails.
    let held = crate::signal::mask_of(me) & (1 << (ttin - 1)) != 0;
    if crate::signal::action(me, ttin as u64, u64::MAX) == 1 || held || crate::job::orphaned(group) {
        return Some(u64::MAX);
    }
    crate::job::raise_for_group(group, ttin);
    Some(crate::signal::INTERRUPTED)
}

/// A change to a terminal by a program its session has put behind — its
/// settings, its size — or a write, where the terminal asks for that
/// (`TOSTOP`): not made. The program is stopped (SIGTTOU) and asked again
/// when it is continued, as a read from behind is — unless it ignores the
/// signal or holds it back, which is how a shell says that it may, and then
/// it goes ahead. One nobody could continue fails. `None` if it goes ahead.
fn change_from_behind(pty: usize) -> Option<u64> {
    let me = scheduler::current_tid();
    let (session, front) = crate::pty::job(pty)?;
    let group = crate::job::pgid_of(me);
    if session == 0 || crate::job::sid_of(me) != session || group == front {
        return None;
    }
    let ttou = crate::signal::SIGTTOU;
    let held = crate::signal::mask_of(me) & (1 << (ttou - 1)) != 0;
    if crate::signal::action(me, ttou as u64, u64::MAX) == 1 || held {
        return None;
    }
    if crate::job::orphaned(group) {
        return Some(u64::MAX);
    }
    crate::job::raise_for_group(group, ttou);
    Some(crate::signal::INTERRUPTED)
}

/// The same for a write to the slave, which only a terminal that asks for
/// it stops.
fn write_from_behind(pty: usize) -> Option<u64> {
    if crate::pty::stops_writers(pty) { change_from_behind(pty) } else { None }
}

fn pty_read(pty: usize, end: u8, ptr: *mut u8, max_len: usize) -> u64 {
    let mut buf = [0u8; 256];
    let want = max_len.min(buf.len());
    if want == 0 {
        return 0;
    }
    loop {
        // Every time round: a job that was in front when it began to wait
        // may have been stopped and put behind since — and the session that
        // made this terminal the reader's to read may have ended.
        if end == 1 {
            if !crate::pty::slave_is_for(pty, scheduler::current_tid()) {
                return u64::MAX;
            }
            if let Some(answer) = read_from_behind(pty) {
                return answer;
            }
        }
        match crate::pty::read(pty, end, &mut buf[..want]) {
            Ok(0) => return 0, // the other end has gone: end of file
            Ok(n) => {
                let _ua = crate::cpu::UserAccess::begin();
                unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), ptr, n) };
                drop(_ua);
                return n as u64;
            }
            Err(()) => match crate::pty::wait_readable(pty, end) {
                crate::pty::Waited::Look => {}
                // A signal with a handler: the program would rather hear
                // about that than go on waiting for a key.
                crate::pty::Waited::Interrupted => return crate::signal::INTERRUPTED,
                // No room to be woken; better said than slept.
                crate::pty::Waited::NoRoom => return u64::MAX,
            },
        }
    }
}

/// What is typed at a terminal, written to its master. A character that
/// raises a signal has been taken out of it by the line discipline, which
/// only remembers that it was pressed: who it is for is decided here.
fn pty_type(pty: usize, ptr: *const u8, len: usize) -> usize {
    let n = {
        let _ua = crate::cpu::UserAccess::begin();
        let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
        crate::pty::write(pty, 0, slice)
    };
    if let Some(signo) = crate::pty::take_signal(pty) {
        crate::signal::from_terminal(pty, signo);
    }
    n
}

/// Write to a terminal from the program in it, waiting for room.
///
/// All of it, as a write to a terminal is: a short count here is a program
/// that has to loop, and a count of nothing is one that takes the terminal
/// for a full disk. Through a kernel buffer for the reason `pty_read` gives —
/// the wait is a reschedule, and the SMAP window must not be open across one.
fn pty_write(pty: usize, ptr: *const u8, len: usize) -> u64 {
    let mut buf = [0u8; 256];
    let mut done = 0;
    while done < len {
        let chunk = (len - done).min(buf.len());
        {
            let _ua = crate::cpu::UserAccess::begin();
            unsafe { core::ptr::copy_nonoverlapping(ptr.add(done), buf.as_mut_ptr(), chunk) };
        }
        let mut sent = 0;
        while sent < chunk {
            sent += crate::pty::write(pty, 1, &buf[sent..chunk]);
            if sent < chunk {
                match crate::pty::wait_writable(pty, buf[sent]) {
                    crate::pty::Waited::Look => {}
                    // Nobody is left to read it, or a signal came first.
                    // What went, went.
                    waited => {
                        let total = done + sent;
                        return if total > 0 {
                            total as u64
                        } else if waited == crate::pty::Waited::Interrupted {
                            crate::signal::INTERRUPTED
                        } else {
                            u64::MAX
                        };
                    }
                }
            }
        }
        done += chunk;
    }
    done as u64
}

/// Enter user mode via iretq.
///
/// # Safety
/// `rip` must point to valid user code, `rsp` to a valid user stack.
pub unsafe fn enter_usermode(rip: u64, rsp: u64, arg: u64) -> ! { unsafe {
    // Out of the kernel: the lock is given up here, since this does not
    // return through anything that would. Interrupts off first, and off
    // until the `iretq` — between the two this processor is in the kernel
    // without the lock, and after the `swapgs` without its own GS.
    core::arch::asm!("cli", options(nostack, nomem));
    crate::usage::leaving(scheduler::current_tid());
    crate::klock::release();
    core::arch::asm!(
        "pushq {user_ss}",             // SS
        "pushq {user_rsp}",            // RSP
        // RFLAGS built from scratch: bit 1 (reserved, always set) + IF.
        // Deriving it from the kernel's current RFLAGS handed user mode
        // whatever DF/AC/IOPL state the kernel happened to be in.
        "pushq $0x202",                // RFLAGS
        "pushq {user_cs}",             // CS
        "pushq {user_rip}",            // RIP
        "swapgs",                       // set up GS for next syscall
        "iretq",
        user_ss = in(reg) 0x2Bu64,     // 0x28 | 3
        user_rsp = in(reg) rsp,
        user_cs = in(reg) 0x33u64,      // 0x30 | 3
        user_rip = in(reg) rip,
        // The entry point's first argument. A thread needs its closure, and
        // iretq leaves the general registers alone, so RDI simply survives.
        in("rdi") arg,
        options(att_syntax, nostack, noreturn)
    );
}}
