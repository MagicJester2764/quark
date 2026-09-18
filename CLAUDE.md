# Working on Quark

Quark is an x86-64 microkernel. It is one of three repos that build together
and must be checked out as siblings:

```
repos/
  quark/       this repo — kernel, drivers, user space
  bang/        UEFI bootloader, and nothing else
  explosion/   the distro: stages the other two and assembles the image
  rust/        fork of rust-lang/rust carrying the x86_64-unknown-quark std PAL
```

`explosion` refers to `../quark` and `../bang`, and the fork's
`library/Cargo.toml` patches `quark-rt` through the relative path
`../../quark/user/quark-rt`. Anything other than a flat sibling layout breaks
the build. The dependency runs one way — the distro reaches down to the kernel
and the bootloader, never the reverse.

## Toolchain

Pinned to `nightly-2026-03-01` in `rust-toolchain.toml`. `../bang` pins the
same nightly, but for its own reasons rather than to match this tree: it does
not depend on the fork at all, and pins because newer toolchains rewrite the
uefi crate's UCS-2 loops into a `wcslen` libcall it has to supply. Keeping the
two equal only saves rustup a second download.

**The pin must equal the commit the fork is based on.** `../rust`'s `library/`
is a checkout of upstream at one commit and only compiles with the rustc built
from it; a newer compiler rejects its own `core` (`impl const Trait for Type`
becomes "expected a trait, found type", features get removed). This is
checkable rather than guessable:

```bash
rustc --version                 # ... (38c0de8dc 2026-02-28)
git -C ../rust log -1 --format=%H $(git -C ../rust merge-base HEAD upstream/main 2>/dev/null || echo HEAD~2)
```

The short hash in `rustc --version` must match the fork's base commit. It does:
`38c0de8dc` for both. If you rebase the fork, move both pins in the same commit.

Note the off-by-one — the date in a rustup channel is the *publish* date, so
`nightly-2026-03-01` is the build dated 02-28. A floating `nightly` channel is
what to avoid: it drifts forward and silently leaves the fork behind.

Fresh machine:

```bash
rustup toolchain install nightly-2026-03-01
rustup component add rust-src llvm-tools-preview --toolchain nightly-2026-03-01
rustup target add x86_64-unknown-none x86_64-unknown-uefi --toolchain nightly-2026-03-01
git -C ../rust submodule update --init --depth 1 library/backtrace
```

That submodule is the easy one to miss. Without it `std` fails with
`couldn't read .../backtrace/src/lib.rs`, which stops the build before
`../explosion` can stage a kernel — so the image keeps whatever it had and you
debug the wrong binary.

The fork is needed only for the hosted `hello`. Without it on disk, `make`
skips that one program and says so; the kernel and every other program still
build, because a kernel should not need a patched rustc checkout to compile.

## Build and run

```bash
make            # kernel, drivers and every user program
make install DESTDIR=<dir>   # stage the artifacts for a distro to consume

cd ../explosion
make run        # stage both trees, assemble the image, boot it in QEMU
```

Quark builds a kernel and the programs that run on it. It does not know what an
image looks like, and nothing here reaches into a sibling repo — `make install`
lays artifacts out and ExplOSion collects them.

The hosted `hello` compiles `std` from source and is the memory-hungriest step;
an unexplained build death is usually the OOM killer. Note that
`cargo -Z build-std` does not track `quark-rt`, which reaches `hello` only
through the fork's `library/Cargo.toml` patch, so the Makefile hashes the
quark-rt sources and cleans the hosted build when they change. Without that,
editing quark-rt leaves a stale `hello` linked against the previous copy —
which is how it ended up calling pre-Phase-0 syscall numbers and faulting.

ExplOSion's QEMU targets pass `-cpu max` deliberately. Default CPU models expose
neither SMEP nor SMAP, so the kernel's supervisor-mode protections are silently
inactive without it — a boot test on the default CPU proves nothing about them.
The kernel prints which it enabled to serial at boot.

Only serial reaches stdout; `console::puts` goes to the framebuffer. To see
user-space output headlessly, screendump over QMP rather than assuming the
system hung.

## Invariants that must not regress

These were established deliberately. Breaking one silently re-opens a hole.

- **User mappings live at or above `paging::USER_MIN_ADDR` (PML4[1]).**
  `create_address_space` deep-copies only PML4[0]'s PDPT and *shares* the page
  directories beneath it, so a mapping below that writes into tables every
  address space shares and promotes them to USER everywhere. It is also what
  makes SMAP safe: no USER bit exists anywhere in the kernel's identity map.
  Validate with `paging::user_range_ok`.
- **`paging::OWNED` (PTE bit 9) decides what may be freed.** Only frames the
  address space owns go back to the allocator. Device MMIO, shared memory and
  frames another task still holds are mapped *without* it. An owned frame is
  mapped in exactly one place — that is what makes freeing it on unmap safe —
  so a new mapping path either leaves the bit off or moves the page, as
  `sys_addrspace_give` does, rather than copying the mapping.
- **A spawner gives a program its pages; it does not lend them.** The loader
  builds the image in its own `sys_mmap` memory and moves it across, so the
  child owns its code and stack and frees them when it goes. Lending frames
  with `sys_addrspace_map` kept them the spawner's: a shell leaked every program
  it ran, and a spawner that exited first freed them under its children.
- **A dead task holds all its memory until it is reaped.** `sys_wait` reaps the
  child it returns; the idle loop reaps the rest. Anything that collects a
  child some other way must reap it too, or a parent running programs back to
  back — which never lets the machine idle — runs out of memory.
- **Every task has its own floating-point state**, saved and restored on every
  switch (`fpu.rs`). The kernel is soft-float and never touches the registers,
  so this is the whole of it. FXSAVE is enough only while CR4.OSXSAVE is clear:
  enabling AVX without moving to XSAVE hands one task another's YMM registers.
- **The argument page carries the program's own header table** — its end
  belongs to it however long the command line is (`spawn::PHDRS_AT`, mirrored
  in `user/libc/include/quark/layout.h`). musl finds a static program's
  thread-local template through it, and without it every thread-local lands
  outside its block. So every spawner calls `set_args`, even with no
  arguments; a program reading an argument page that was never mapped faults.
- **Page-table entries that refer to a memory object carry its slot** in bits
  52–62, present or not, and every path that clears or replaces an entry —
  `clear_range`, `unmap_page`, `map_page`, `free_pt_leaves` — hands the
  reference back (`memobj::unmap_ref`). Miss one and the object is never
  released. Protection keys would give bits 59–62 a meaning, so CR4.PKE stays
  clear.
- **A reserved page is a non-present entry with `paging::MARKER` set**, in a
  page table or, for 2 MiB at once, a page directory. It is not empty: the
  walks that free tables and the checks that an address is free test for an
  all-zero entry, not a clear `PRESENT` bit, or they throw reservations away
  and free tables still in use. The kernel backs a reserved page before it
  touches it (`validate_user_range`, the futex path), and a page fault on one
  is served, so the first touch from anywhere gives it its frame.
- **A fault in ring 3 ends the task, never the machine.** The task exits with
  the negated Linux signal number (`idt.rs`), and only a fault taken in ring 0
  halts. musl's `abort()` is a privileged `hlt`, so before this, one failed
  assert stopped everything.
- **C objects must put constructors in `.init_array`.** The cross compiler is
  configured `--enable-initfini-array`, and the user link script places the
  arrays and refuses `.ctors` outright, so an object carrying them has
  constructors that silently never run. `crtbegin.o` and `crtend.o` are on the
  link line, but for the other thing they do: they bracket `.eh_frame` and
  register it, which is how a C++ exception finds its handler. Their own
  constructor is in `.init_array` like everybody else's.
- **Capabilities are the authority.** There is no UID 0 bypass; `uid == 0` no
  longer short-circuits `cap::task_has_*`. A service that cannot do something
  is missing a capability, not a privilege level.
- **A descriptor that says non-blocking must not park the task.** `O_NONBLOCK`
  was accepted and ignored for a phase: the flag set a bit only `sendmsg` read,
  and `read`/`write` always used the calls that park. Every main loop drains
  its wake-up "until it is empty", and empty is a read that answers `EAGAIN`;
  a read that waits there is a program that stops rather than one that fails —
  glib's does it holding its context lock, so nothing else in the program ever
  runs again. `SYS_FD_READ_NB` and `SYS_FD_WRITE_NB` are the calls that answer
  instead, and the same goes for a futex wait with a deadline: dropping the
  timeout makes `g_cond_wait_until` wait for ever.

- **`UserAccess` guards must not span a block or yield.** RFLAGS.AC travels
  with the task's saved flags, so a guard held across a reschedule leaves the
  SMAP window open in whatever runs next. Copy into a kernel buffer first — see
  `fd_write_ipc`.
- **Validate user pointers with `validate_user_ptr{,_mut}`, not a range check.**
  The kernel runs on the caller's CR3; an in-range but unmapped address faults
  *inside* the kernel, sometimes with a lock held and interrupts off.
- **Mapping authority is ownership first, `PhysRange` second.** `sys_map_phys`
  and the deprecated `sys_addrspace_map` accept frames the caller owns
  (`pmm::owns_range`), so a task that allocated a frame may map it holding no
  capability at all. That is what almost every mapper does. A `PhysRange` grant
  is for memory the allocator never owned — the framebuffer, device MMIO, a
  boot module — and covers exactly that. The legacy `CAP_MAP_PHYS` bit confers
  nothing: it used to expand into a range over all of memory, which a bit
  passed with `SYS_GRANT_CAP` or `SYS_CAP_TRANSFER` could hand anybody.
- **A server copies what a client lent; it never maps a client's page.** Data
  travels with the call (`SYS_CALL_LEND`, then `SYS_LENT_READ` and
  `SYS_LENT_WRITE`), so no server needs authority over physical memory to serve
  anyone. A protocol that names a physical address makes its server a deputy
  that will read or write any page in the machine — that is why the disk, VFS
  and NET servers once held all of it. A shared-memory handle is no better
  when the request names it: handles are global numbers.
- **No task holds a `PhysRange` wider than one device.** init is started with
  the framebuffer and its boot modules, `fb` holds the framebuffer and lends it
  on, and no driver holds any. `dtest physical` walks every CSpace and fails
  otherwise.
- **A program declares what it needs; a spawner grants from that.** Capabilities
  come from a `quark_rt::manifest!` block compiled into the image, found by
  scanning for its magic, not from a table of names in `init`. A spawner mints
  each request from a capability it already holds, so it can never hand out more
  than it has — the shell holds no `PhysRange` and therefore cannot give one
  away. The framebuffer is the one exception: its address comes from the
  bootloader at runtime, so `init` grants it directly.
- **IPC needs an Endpoint capability, and an Endpoint names a task, not a
  TID.** `sys_send`/`sys_call`/`sys_notify` look for an `Endpoint` recording
  the destination's endpoint number, which the kernel assigns when a task slot
  is filled and never gives out again. TIDs are recycled; numbers are not, so a
  capability to a dead task names nothing and nothing has to be swept at reap
  time. Only the task itself, its creator or a holder may mint one. Everybody
  else is handed one: every program gets the nameserver's from its spawner, and
  a lookup grants the one for the name. IPC the kernel performs through an
  installed fd bypasses this on purpose: the fd is the authorisation, and only
  a CAP_TASK_MGMT holder can install one.
- **A server calls a client back only with a capability the client offered.**
  `sys_call_offer` puts one on a call and `sys_cap_take` accepts it; nothing
  else can fill a server's CSpace, and a claim or registration made without
  one is refused.
- **A program is its address space, not its task.** `SYS_TASK_SPACE` names the
  program a task belongs to with an id the kernel never reuses, and
  `SYS_SPACE_WATCH` says when its last task has gone. Anything a server keeps
  for a program — an open file, a working directory, a lock — is kept by that,
  so every thread of a program shares it and a recycled TID inherits nothing.
- **A call from the kernel to a pager carries `PAGER_BIT` in its sender**, and
  nothing else can: the bit is set by `call_as` and by no syscall. A pager
  answers `TAG_PAGE_IN` and `TAG_OBJECT_SYNC` only for a sender that has it,
  and replies to the sender as it came — the reply strips the bit and reaches
  the faulting task.
- **No server blocks on one client.** A request that cannot be answered now is
  kept and answered later, rather than waited for: `input` holds a reader
  until there is a line, the framebuffer device gives a claimant a second to
  answer a handover and then goes on without it, and a compositor's writes to
  a client are non-blocking. A server that waits on one client has stopped
  serving every other, and a fuzzer finds that in seconds.
- **A driver answers only the server that claimed it.** The disk driver
  serves the VFS and the keyboard driver serves `input`, each from the first
  claim until that claimant dies, and refuses everybody else (error 5). A
  program that could reach the disk driver could write any sector, and one
  that could reach the keyboard's would read whatever anybody typed.
- **Only the kernel reports a death.** Any program that can call a server can
  send `TAG_TASK_DIED`; what it cannot do is send as sender 0. A server
  believes a notice through `quark_rt::ipc::death_notice` (or
  `space_death_notice`) and answers anything else with that tag as the
  unknown request it is. The nameserver used to forget a service, and `fb`
  give up the console's display, because a program said so.

A **pseudo-terminal** is a kernel descriptor, for the same reason a pipe is: a
terminal emulator waits on its master with `poll`, and readiness the kernel
cannot see is readiness `poll` cannot report. `/dev/ptmx` makes a pair and
answers with the master, `/dev/pts/N` opens its slave, and both paths are
caught in the C layer ahead of the VFS. The line discipline is the part
programs depend on and no more — echo, canonical input, and the newline
translations — and the rest of a `termios` is stored and handed back unchanged.
Between the master being opened and the slave being opened the master's read
waits rather than reporting an end of file: the program that will hold the
slave has not been started yet. Afterwards, the last slave closing *is* the end
of file, which is how a terminal learns its shell has exited.

A **timer is a descriptor too** (`SYS_TIMER_CREATE`), because a program's event
loop already waits on descriptors and a cursor that blinks needs the same wait
to end at a time rather than at an event.

`init` spawns `FB`, `CONSOLE`, `INPUT` and `VFS` in passes of their own. If a
program misbehaves for lack of a capability, check that its pass actually calls
`grant_caps_from_manifest` — there is no shared path that does it for them, and
INPUT's pass once granted nothing at all, which the UID bypass hid.

## What a process is

A program here starts one of two ways, and both are now ordinary.

A **spawner** builds one: it makes an address space, reads an ELF into its own
memory, moves the pages across, wires the descriptors, hands over the
capabilities and starts a task in it. That is `quark_rt::spawn`, and it needs
authority over nobody — a task the caller created and has not started is its
own to fill, because nothing else can name it, it holds nothing and it cannot
run. `TaskMgmt` buys the unbounded form; without it a program may have as many
children at once as it may have threads.

Or a program **forks** and **execs**, which is what a C program does and what
every Unix program assumes:

- **`fork` copies eagerly.** The child is a task in a copy of the caller's
  address space that returns 0 from the same system call — which works because
  the syscall stub's eleven pushes always land at `kernel_stack_top - 88`, so a
  task inside a call has its whole register frame at a known place. A page the
  parent owns becomes a page of the child's own; a page it does not own —
  shared memory, a device, a file's page — is shared, because `OWNED` is what
  decides who may free a frame. Copy-on-write would save all of the copying and
  none of the correctness, and it needs reference counts frames here have not
  got.
- **`exec` keeps the task and changes the program.** The C layer loads the ELF
  into an address space it made and `SYS_EXEC_SPACE` swaps the task into it:
  same id, same descriptors, same capabilities, same parent, new address space
  — and therefore a new program as far as every server is concerned. The
  thread pointer is cleared with it, or the new program's first thread-local
  reads through an address the old one had.
- **A kernel budget is per program, not per TID.** A pipe outlives its
  creator — its ends are descriptors other tasks hold — so counting the
  per-task cap by TID gave a fresh task the budget of whatever had its number
  before, and a program that had spent its eight left the next task to take
  that number unable to make any. Space ids are never reused; TIDs are. The
  same trap is written down under Known gaps for servers, and it is worth
  looking for anywhere in the kernel that remembers a number.
- **Descriptors are released when a task dies, not when it is reaped.** Its
  memory waits for a parent to collect it, which is where the exit status
  lives; a descriptor is something another task can be *waiting on*, and the
  oldest idiom in Unix — a child writes down a pipe and exits, a parent reads
  to the end and then waits — had each half waiting for the other.
- **A descriptor sent over a stream outlives the sender's end.** It is in the
  stream rather than in the sender, and the peer can still take it.

## The screen

`user/fb` is the framebuffer device: it owns the hardware the way `/dev/fb0`
does, knows the mode, and decides who draws. It has no opinion about windows.

Everything else is a client of it. `user/qtty` is the text console: it claims
the display at boot and draws fullscreen — that is what the machine boots into,
a plain TTY. `user/wm` is a compositor you *run*: `wm <program>` takes the
display, starts that program, composites its windows, and gives the display back
when it exits.

`wm` speaks **Wayland**, not a protocol shaped like it. It hands each program a
socketpair end as descriptor 3 and `WAYLAND_SOCKET=3`, which is what
`wl_display_connect` looks at first — so upstream libwayland runs unpatched.
`user/wm/src` is one module per part of that: `client` (a connection and its
buffered bytes), `objects` (one id table per client, which is what makes "a
client cannot name another client's objects" true by construction), `surface`,
`shell`, `shm`, `seat`, `grab` (what the pointer is doing between a press on
the compositor's own furniture and the release that ends it), `clipboard`,
`cursor`, `keymap`, `protocol`, `draw`.
`user/wmdemo` and the older six-tag window protocol still work alongside it.

The pointer has a wheel, and it reaches a client as `wl_pointer.axis` with the
group the version-5 events describe: `axis_source` says it is a wheel,
`axis_discrete` gives the click count, the axis carries ten units per detent as
Weston sends, and a `frame` ends the group. It starts at the i8042: a PS/2
mouse says nothing about a wheel until it is asked, and what asks is the knock
in `user/keyboard` — sample rate 200, then 100, then 80 — after which the
device calls itself 3 and sends four bytes instead of three. The packet length
comes from that answer and not from hope; reading a fourth byte from a mouse
sending three loses the stream for good.

The keyboard goes with it, and so does the pointer. `user/input` has the same
claim protocol: while a program holds it, raw key and pointer events go to that
program and line readers wait. The compositor claims input when it claims the
display and hands each event to the focused window — whoever owns the screen
owns the keyboard, the way switching virtual terminals has always worked.
With nobody holding it, `input` cooks keys as they are typed: the driver
notifies it of each one, and a finished line waits for a reader. It never
waits on the keyboard itself, so a reader waiting for a line holds up nobody
else's request.

Both come from one driver. A PS/2 mouse is not a second device: it is the same
i8042 answering on the same data port 0x60, with IRQ 12 instead of 1 and bit 5
of the status port saying which device a byte came from. `user/keyboard` holds
both lines and routes on that bit — never on which interrupt fired, because a
byte for one device can be waiting when the other's interrupt arrives. Two
drivers sharing port 0x60 would take each other's bytes, and the symptom of
losing that race is a keyboard that types rubbish or stops.

Some things to know before changing any of it:

- **The display is a stack, and so is the keyboard.** A claim goes on top and
  displaces the one below, which gets it back (`TAG_FB_GAINED`) when
  everything above has let go; a claimant further down that releases or dies
  just leaves the line. So `wm "wm <program>"` unwinds to the outer session
  and then the console. A compositor that loses the display unmaps it and
  waits, keeping its keyboard claim below the new one, and repaints all of
  the screen when the display comes back. Eight deep, then claims are
  refused.
- **The display is lent, not shared.** `init` grants the framebuffer
  `PhysRange` to `fb` and nowhere else; `fb` mints a derived capability per
  claimant and revokes it to take the display back. Revocation governs the
  right to *map*, not mappings that already exist, so the outgoing owner is
  told and answers before the new one is let in — and must empty its
  capability slot, since granting into an occupied one fails.
- **Guard every framebuffer write on still owning the display**, not just the
  flush. The console gated its flush and not `hide_cursor` or `scroll`, and
  carried on writing into memory it had just unmapped.
- **Composite into a back buffer.** Painting onto the visible surface means the
  cleared screen is briefly the one on the monitor, once per frame; a cursor
  blink is enough to make that a visible flash. `sys_mmap`/`sys_munmap` take at
  most 256 pages, so a screenful takes a loop.
- **Repaint the region that changed, not the screen.** A commit says which
  window changed; repainting all of it costs a megapixel of backdrop and a
  four-megabyte copy, which a client committing a dozen times a second turns
  into a compositor with no time left to read the keyboard. `wm` clips every
  drawing primitive to a region and copies only that region out.
- **A client's request is read inside the request.** Every argument comes
  through a cursor bounded by the size in the message's own header, and
  anything that cannot be honoured — an opcode the interface does not have, an
  object that is not there or is not what the request needs, a string that
  does not end in a NUL, a `bind` above the version advertised — is a
  `wl_display.error` naming the object and the reason before the connection
  ends. Reading straight from the buffer took the next message's bytes, or the
  last read's, as arguments a client had not sent.
- **A slot is not freed while an object still names it.** `xdg_toplevel.destroy`
  takes the role away and leaves the surface, because the client's
  `wl_surface` still names it; destroying the surface takes away every object
  of that client's that named it. A surface slot freed under a live name is a
  slot the next client's surface takes — with the first client still able to
  attach to it. Buffers count the surfaces showing them rather than carrying a
  flag, for the same reason, and a pool is unmapped by what was mapped rather
  than by what the client called a pool.
- **What the compositor has, each client has a share of.** Pools, buffers and
  surfaces are shared tables, so one client may hold a quarter of each: a
  client asking for them in a loop is a client, not a compositor.
- **Between a press on the compositor's own furniture and the release that
  ends it, the pointer is the compositor's.** That is a grab (`wm/src/grab.rs`),
  and while one is on no client hears a motion or a button — the movement is
  not about them. A press on the title bar moves the window, one within `GRIP`
  of an edge or corner resizes it, one on the close box asks the client to go,
  and two on the bar within half a second fill the screen. A grab ends when the
  button comes up, when the window goes, or when its client disconnects; the
  last two are one thing, and `destroy_window` says so before it frees the slot,
  or the grab would go on moving a window somebody else has since been given.
  `xdg_toplevel.move` and `.resize` start the same grabs for a client that
  draws its own decorations, and are refused unless a button is actually down —
  a grab with nothing held ends at the next release or never, which is a client
  taking the pointer away from whoever is using the machine.
- **A size is agreed, not imposed.** The compositor never resizes a window
  itself: it sends `xdg_toplevel.configure` with a size and the states, then
  `xdg_surface.configure` with a serial, and the window follows whatever buffer
  the client attaches. A client that ignores the pair keeps the size it had and
  nothing waits for it. The two halves go together — one without the other
  leaves a client waiting for a serial that never comes — and a surface accepts
  any serial from the oldest unanswered one up to the newest sent, because a
  resize sends one per tick and answering one supersedes the older ones. A
  compositor that insisted on the newest killed a client for being a frame
  behind.
- **A version is advertised only when every event of it is sent.** `wl_seat` is
  5 because `wl_pointer.frame` and the axis events go out; `wl_compositor` is 4
  — the buffer transform, the buffer scale and `damage_buffer` are read and
  checked, and `wl_surface.enter`/`leave` are the only events up to it —
  because weston's toytoolkit binds it at 3 with no negotiation and a
  compositor offering less is one every weston client dies against; `wl_output`
  is 2 for `scale` and `done`; `xdg_wm_base`, `wl_shm`, the decoration manager,
  `wl_data_device_manager` and the primary selection are 1. The
  clipboard stops at 1 deliberately: 2 and 3 are drag and drop. An object made
  from another inherits its version, which is how a client that bound
  `wl_seat` at 4 gets a `wl_pointer` with no `frame`.
- **Say why a client was killed.** A protocol error is fatal to a connection
  and most programs die of one in silence: libwayland hands the reason to the
  program, and the program exits. `weston-terminal` exited three times without
  a word before the compositor started printing what it had refused.
- **Events are pulled, not pushed.** A server calls a client only when the
  client asked it to and handed over the right to — `fb` and the keyboard are
  offered an `Endpoint` with the request that needs one. Otherwise it answers:
  a call blocks until the client replies, so a slow client would stall the
  server, and one that is itself calling the server would deadlock with it. A
  reply needs no capability, so every other hop here is the client asking.

## Toolkits

Above the font stack there is now a whole GNOME-shaped one, built for Quark and
running on it: **glib** (with GObject, GIO, a main loop and a thread pool),
**harfbuzz**, **fribidi**, **pango**, **graphene**, **gdk-pixbuf**, and
**GTK 4**. `wm hello-world` draws GTK's own `examples/hello/hello-world.c`,
unmodified, in a window — and prints "Hello World" when the button is clicked.

The rules that got it there, and that a further port should follow:

- **Nothing patches an upstream library.** Everything each one needed was added
  to Quark: `eventfd`, a futex wait that honours its timeout, an `O_NONBLOCK`
  that means it, a `poll` with no descriptors that waits, a spawner that can
  read a program bigger than four megabytes, and a compiler that admits this is
  a Unix. Teaching a package's `config.sub` the word `quark` is not a patch to
  the package; it is a patch to autoconf's idea of what operating systems
  exist.
- **Static, and non-PIC.** There is no dynamic loader here, so
  `-Ddefault_library=static -Db_staticpic=false` is on every meson build and
  the compiler wrapper drops `-fPIC` whatever a build system says. A module
  that would be `dlopen`ed has to be built in instead — gdk-pixbuf's loaders
  are, which is also why no loader cache is needed.
- **glib is built twice.** Three of its tools are C programs rather than Python
  — `glib-compile-resources`, `glib-compile-schemas` and `gio-querymodules` —
  and GTK's build runs two of them to turn XML into C. The copies in the target
  prefix are Quark binaries and cannot run on the build machine, so a native
  glib lives in `$QUARK_HOSTDEPS` beside the host expat, and every build script
  puts it on PATH first.
- **There is no OpenGL.** GTK links libepoxy whatever it draws with; epoxy was
  built to look for a GL implementation at run time and correctly finds none,
  so GSK falls back to its cairo renderer. A Mesa software rasteriser is a
  project of its own and is not this one.
- **GTK 4 has no static build.** `gtk/meson.build` says `shared_library` with
  no choice about it. What it also has is the `static_library` the shared one
  wraps, so `build-gtk.sh` builds those and `build-gtk-client.sh` links a
  program against them — the same shape as the weston toytoolkit port.
- **A toolkit program is twenty-five megabytes**, and the spawner reads the
  whole image into its own memory before giving the pages away, so it is in
  memory twice while it starts. QEMU gets a gigabyte and the root filesystem is
  128 MiB.

## Scheduling

Four bands, best first: drivers, servers, ordinary programs, idle. A task runs
only when nothing better is waiting and takes turns within its own band. A
program asks for a band in its `manifest!` block alongside its capabilities,
and a spawner applies it under the same narrowing rule — it can never grant a
better band than it is in, so only `init` can put a driver in the driver band.

Three things follow from that, and breaking any of them is quiet:

- **Waiting means blocking.** A task in a better band that spins on
  `sys_yield` is immediately runnable again, so nothing below it ever runs.
  This is fatal rather than merely wasteful now: `nameserver::lookup_retry`
  yielded a hundred times between tries and starved the VFS out of ever
  registering. Use `sleep_ticks`, or `sys_recv_timeout` if there is also
  something to hear.
- **A synchronous call hands over the CPU.** The caller has blocked and has
  nothing to contribute until the reply, so the callee is switched to directly
  and runs on what is left of the caller's slice rather than a fresh one. A
  server does not earn a quantum every time it is called.
- **A hand-over keeps interrupts off from waking the callee to switching to
  it.** `make_ready` leaves the callee runnable but in no queue, since it is
  about to run, so `donate_to` takes the flags `call_inner` saved instead of
  saving its own. With a gap between the two, a tick preempted the caller,
  already blocked, and nothing ever ran either task again: fontconfig hung
  about once a minute scanning fonts. `dtest calls` makes three million calls
  in three seconds and caught it on its first run.
- **A task runs at the band of whoever is waiting on it**, for as long as that
  is true. Without it a server called by something urgent is preempted by
  anything in between. It is also what lets the direct switch stay safe: the
  callee already carries the caller's band when the scheduler decides whether
  handing straight over would run something ahead of its betters.

## Files

`user/vfs` serves ext2, ext4 and FAT32; `docs/vfs.md` is its protocol. What a
C program sees goes through `user/linux-abi`, which turns descriptors and
Linux's calls into that protocol. `tools/check-rootfs.sh` in ../explosion runs
`e2fsck` on the image a boot test just used, and it is the check for any
change here: it has found what reading the code did not.

- **A handle names an inode, never a copy of one.** The inode is read when the
  handle is used, so two handles on one file agree about its size and blocks,
  and one cannot write through a block map the other just shortened.
- **A handle is its program's, and closes when the program does.** The server
  names a program by its address space (`SYS_TASK_SPACE`) and watches each one
  it gives a handle to, a working directory or a lock. A file whose last name
  went while a handle or a working directory held it is freed when that goes,
  not before.
- **A file removed while in use is on the disk's orphan list** (`s_last_orphan`,
  each inode's `i_dtime` naming the next), in the same transaction that took
  its last name, and comes off it before it is freed. The server frees what a
  stopped machine left there before it answers anything. Deletion times are
  kept above the inode count so that no freed inode reads as a link in that
  list. `tools/crash-test.sh` stops a machine with one on the list.
- **A symbolic link's text is not a block map.** A target under 60 bytes lives
  in `i_block`; freeing, truncating or mapping such an inode as if it held
  block numbers frees whatever blocks the text spells.
- **`/dev` is the server's, whatever the disk holds.** The lookup answers for
  the root's `dev` directory itself, so no path — through links, or relative —
  reaches the disk's copy, and nothing is made there.
- **Paths are lent, never cut.** A path up to 4095 bytes travels in a buffer
  lent with the call, and a longer one is refused. The old requests carried
  paths in the message and truncated them, which opens a different file.
- **A directory's times change with its entries**, which is how fontconfig
  knows its cache is stale.
- **A descriptor names an open file.** In the Linux layer, `dup` gives a file
  a second descriptor that shares its position, and the VFS handle closes with
  the last one. The server never sees the copies.
- **A journaled write never lets a prefetch cache the old copy.** While a
  transaction holds a sector, a read ahead skips it; caching what is on disk
  under it lost a rename on ext4.
- **A write allocates every block in its range, holes included.** A
  truncate that lengthens a file leaves holes, and ext4 keeps the extent root
  in logical order so that a block written into one is where a read looks.

## Known gaps

- Servers still know their clients by TID (`Message.sender`). The kernel will
  not deliver a call the caller had no capability for, but a server that keeps
  a client's TID past one call — a lease, a registration, a foreground task —
  must watch it with `sys_task_watch` and forget it on death. Otherwise it
  treats whatever takes the TID next as the same client.
- Nothing is ever paged out: anonymous memory is given its frames when first
  touched (`SYS_MAP_ANON`, which the C library's `mmap` uses) and keeps them.
  A machine that runs out ends whichever task touched the page it could not
  give (SIGBUS), not the biggest. Reading an untouched page gives it a frame
  of its own, where Linux maps one shared page of zeroes. Rust programs' heaps
  still come from `SYS_MMAP`, backed at once.
- Focus is a single stack with little policy: Tab cycles, a new window takes it,
  and a click raises the one under the pointer. Keyboard focus and pointer focus
  are tracked separately, as Wayland requires, but there is no follow-mouse and
  no focus stealing prevention.
- A **forked child inherits the numbers but not the files**. The kernel's
  descriptors — pipes, streams, ptys, shared memory, timers — are the task's
  and are copied to a child and kept across an exec. The C layer's *VFS* files
  live in the program's own memory and are named by its address space, so a
  forked child holds numbers the server will not answer for, and an exec starts
  with none; a working directory is the same. `weston-terminal` needs neither,
  and the next piece of this hole is making the VFS understand that one program
  is a copy of another.
- `fork` copies every page the caller owns, eagerly, and a threaded program
  cannot `exec`: POSIX has it end every other thread, and ending them means
  unwinding what they hold in a server, so it is refused rather than half done.
- There are **no signals**. `setsid` answers with the caller's own id,
  `TIOCSCTTY` is accepted, and a terminal's Ctrl-C reaches the program in it as
  a byte rather than as a signal — there are no process groups for one to go
  to. `sigaction` is accepted and remembered by nobody.
- A pty's window size is stored and nothing is told when it changes: Linux
  sends `SIGWINCH`, and there are no signals. A program that draws itself to
  the terminal's size reads it once.
- The compositor keeps no history of serials, so `xdg_toplevel.move` and
  `.resize` cannot check that the serial they are given was a recent press.
  What they check instead is that a button is down. Drag and drop, touch and
  key repeat as a compositor policy are all still missing, and so is any way to
  put a window somewhere other than on the one screen: fullscreen and minimise
  are read and ignored.
- **`wl_shm_pool.resize` is refused.** A pool may only grow, and growing means
  new memory, which means a descriptor the request does not carry; a client
  that drew past the old end would fault the compositor. A client that needs a
  bigger pool makes a new one and lets the old go after the commit that
  replaces it — which is safe because a buffer destroyed while it is being
  shown becomes a zombie and its pool stays mapped until nothing shows it.
  Toolkits do call `resize`, so this is a real gap rather than a preference.
- The clock is read once, from the CMOS clock at boot, as UTC. Nothing sets it,
  and there is no time zone.
- `O_CREAT` through a symbolic link whose target does not exist says EEXIST,
  where Linux makes the target, and `linkat` cannot name its source by
  descriptor (`AT_EMPTY_PATH`). FAT32 has no links, and no directory handles
  to start a relative path from.
- A FAT32 root cannot remove, rename or shorten anything, and ext4 refuses to
  shorten a file whose extent tree has grown past the inode.
- A mapped file's pages stay cached until nothing maps the file any more;
  nothing evicts them under pressure, and the kernel's cache holds 8192 pages
  across 256 objects, of which the VFS pages 30 at once. A private writable
  mapping copies a page when it is first touched, read or write. A file
  descriptor cannot be `dup2`ed onto one of the kernel's numbers (a program's
  stdout), nor the other way round.
- `mprotect` says yes and does nothing: a mapping is made with the protection
  it will keep, so a program that maps read-only and then asks for write gets
  a mapping that still faults on the write. Shortening a file does not take
  away pages of it a program has already mapped past the new end; what it does
  is stop new ones being filled from beyond it.
- `std::fs` is not implemented for this target: a hosted Rust program reads
  and writes through descriptors it is given, not through `File::open`. C
  programs have the whole of the C library's file interface.
- `flock` and `fcntl` locks are one kind here, so the two can keep each other
  out where Linux keeps them apart. Locks live in the server's memory, 256 at
  once.
- A thread starts with a copy of what its creator holds — capabilities and
  descriptors, poll sets and sockets excepted — not a share of it: what either
  is given or closes afterwards, the other does not see. A pipe end that was
  open when a thread started stays open until that thread closes it or exits.
- A C program has 16 open files; the VFS has 512 handles for everybody, and
  128 for any one program.
- **No OpenGL, no D-Bus, no `dlopen`.** GTK starts without any of them and says
  so: `g_module_symbol` complains about a NULL module twice, the session bus
  cannot be reached, and GSK draws through cairo. Each is a real absence rather
  than a stub, and each is a thing a bigger application may ask for and not get.
- **Quark has no dma-buf**, and the Linux uapi headers copied wholesale into
  the sysroot said it did until `linux/dma-buf.h` was taken out of them. Every
  other header there describes something a program can ask for and be told no;
  that one is asked at *build* time, and a yes makes a toolkit compile a path
  that cannot work.
- The shell cannot set a variable for one command — there is no
  `VAR=value program`, only the four in `BASE_ENV`. Nothing has needed it yet,
  because GTK falls back to the cairo renderer by itself, but the next program
  that wants an environment variable will need the shell to grow one.
- The rust fork is one commit on `upstream/main`. Rebasing it means re-checking
  the PAL against std's internals, which move: the allocator PAL shape, the
  futex module location, `RawOsError`'s home and `BorrowedCursor`'s parameters
  have all changed under it before.
