# Wayland on Quark

**Status: design.** Nothing here is implemented. This is the plan for Phase 8
of `../../ROADMAP.md`, and it is written down before the code because the parts
that are easy to get wrong are the ones nobody notices until a client from
somewhere else refuses to run.

Quark implements **Wayland**: the actual protocol, the actual wire format, with
the goal that a client built for Linux and never modified will run here.

## Why the real thing

A protocol shaped like Wayland but carried over Quark's own IPC was designed in
full first, and rejected. It would have been smaller and it would have worked.
What it could not buy at any price is a client somebody else wrote — and
without that, every program that ever draws on this system has to be written
for this system.

Writing that draft was still worth it, because it exposed what the objection to
real Wayland actually was. Wayland assumes four things:

1. a bidirectional, ordered byte stream between two processes;
2. descriptor passing, so a message can carry a handle to an object;
3. memory addressable as a descriptor, which both sides map;
4. `poll`, so one task can wait on several descriptors.

Quark has none of them. And the draft's whole transport — a shared-memory ring,
a futex wake, and an endpoint the compositor granted sideways into the input
server so that a server could push — was machinery invented to work around
exactly that absence. It was not a design; it was a series of detours around
holes in the operating system.

**So the four are Phase 10, and they are worth building whether or not anything
is ever drawn on this machine.** A socketpair, descriptor passing, memory as an
object, and a way to wait on more than one thing at once are what every
Unix-shaped system has, and Quark has already bent two subsystems out of shape
for want of them.

## What Wayland needs, and where it comes from

| Wayland requires | Quark today | Comes from |
|---|---|---|
| `AF_UNIX` `SOCK_STREAM` connection | pipes: unidirectional, 4 KiB, 8 per task | **Phase 10** — socketpair |
| `SCM_RIGHTS` descriptor passing | nothing; `SYS_FD_DUP` pushes, needs `TaskMgmt` | **Phase 10** — passing, attached to the stream |
| `memfd`/`shm_open` + `mmap` for `wl_shm` | `shmem` handles, not descriptors; `mmap` is anonymous | **Phase 10** — memory as a descriptor |
| `poll()` on the connection | nothing at all | **Phase 10** |
| `pthread_mutex`, `pthread_cond` in `wl_display` | futex only | **Phase 9** |
| libffi, for dispatching into listeners | — | port; still a hard dependency at libwayland 1.26.90 |
| libwayland-client | — | port, patched — see below |

The compositor is **our own code**. libwayland-server is not used, which is what
keeps `epoll` off this list: it belongs to that library's event loop, not to the
client's.

## What the compositor is

Six things have to happen for a window to be on a screen:

1. Own the screen — mode, pixel format, and the memory that is scanned out.
2. Give each application somewhere to draw.
3. Decide where each buffer goes, and in what stacking order.
4. Composite the visible parts into the screen.
5. Route input to whichever client it belongs to.
6. Furniture and policy: title bars, focus, dragging, closing.

**Job 1 is not Wayland's and stays where it is.** `user/fb` owns the framebuffer
the way `/dev/fb0` does and lends the display by capability; clients never see
it, and the compositor is one of its claimants. That is a cleaner separation
than Linux has, where mode-setting is kernel code.

X11 split jobs 3 and 6 into a window-manager process separate from the server
doing 1, 4 and 5, and the two disagreed in the gap between them — a window that
flickers at the wrong size is that disagreement. Wayland collapsed them, so
**"compositor" and "window manager" name one program**. `user/wm` is that
program, and it already being both is the right shape rather than a shortcut.

## Protocols implemented

Versions are what the compositor advertises; a client binds no higher than the
minimum of what it supports and what it is offered. Start low and raise a
version only when a target client needs it — the exact minimum each of the
three milestone clients demands is a thing to determine by running them, not by
reading.

**MVP — everything `weston-simple-shm` touches:**

`wl_display`, `wl_registry`, `wl_callback`, `wl_compositor`, `wl_surface`,
`wl_shm`, `wl_shm_pool`, `wl_buffer`, `wl_output`, and from `xdg-shell`:
`xdg_wm_base`, `xdg_surface`, `xdg_toplevel`.

No seat is needed to put a picture on the screen, which is what makes this the
smallest honest claim that a real Wayland client runs here.

**Near — what `weston-terminal` adds:**

`wl_seat`, `wl_keyboard`, `wl_pointer`, `wl_region`, and the clipboard:
`wl_data_device_manager`, `wl_data_device`, `wl_data_source`, `wl_data_offer`.
Plus `xdg_popup` for its menus, and `xdg-decoration` so the compositor keeps
drawing the title bars rather than every client growing its own.

**Deliberately not implemented, and each for a reason:**

- `wl_touch` — no touch hardware, and nothing to test against.
- `wl_subcompositor`, `wl_subsurface` — a client-side optimisation for
  compositing video or GL under a widget tree; nothing here needs it.
- `wl_shell`, `wl_shell_surface` — deprecated in favour of `xdg-shell`.
- output transforms, fractional scaling, `wl_drm`, `linux-dmabuf` — one output,
  one scale, no rotation, no GPU.

## Rules the compositor must honour

These are not Quark's rules; they are the parts of Wayland that a compositor
gets wrong quietly, where the client is correct and the picture is not. They
survive from the earlier draft unchanged, because they were never about the
transport.

**A surface has no meaning until it is given a role.** A bare `wl_surface` is
never displayed. `xdg_surface.get_toplevel` makes it an application window;
`wl_pointer.set_cursor` makes another one a cursor; `xdg_popup` makes a third a
menu. One surface type, one buffer path, one commit path, and no special case
for any of them. Giving a surface a second role is a protocol error.

**Surface state is double-buffered and applied atomically.** `attach`, `damage`,
`frame`, `set_opaque_region` and the rest do nothing when they arrive — they
accumulate *pending* state, and `commit` applies all of it at once. This is what
makes a half-drawn or half-resized frame unrepresentable rather than merely
unlikely, and a compositor that applies state as it arrives will look correct
until the first resize.

**A buffer is lent, and must be released.** `wl_buffer.release` says the
compositor has finished reading. A client with two buffers draws into one while
the compositor reads the other; a client with one waits for the release before
redrawing. Getting this wrong means the compositor reads memory the client is
writing, and the tearing is real even when it is invisible.

That release also carries a Quark-specific weight: a fullscreen 1280×800 buffer
is 1000 pages, and two of them will not fit in one shared region under today's
limits. Single-buffered clients are therefore a case that must actually work,
not a degenerate one.

**Frame callbacks are the throttle.** A client that draws only when its
`wl_surface.frame` callback fires never draws faster than the screen updates.
Its absence is why an animating client here used to keep the compositor too busy
to read the keyboard.

**Keyboard focus and pointer focus are separate.** Keyboard focus is one
surface, changed by policy. Pointer focus is whatever is under the cursor,
regardless. Moving across an unfocused window still sends it `enter` and
`motion` — that is how hover works without stealing focus. Every keyboard focus
change is `leave` on the old surface *then* `enter` on the new, in that order,
so no client can believe it holds focus twice.

**The implicit grab.** While any button is held, pointer events keep going to
the surface where the press happened, even after the cursor leaves it. Without
it, dragging breaks the moment the pointer moves off the window and a button
un-presses if the pointer slides off before release.

**Serials.** Every input event carries a serial from one monotonic counter, and
requests that must follow real input take one back — `set_cursor`,
`xdg_toplevel.move`, `set_selection`. The compositor rejects a serial that names
no recent event it sent. That is what stops a client grabbing the pointer or
starting a drag nobody asked for.

**Decorations are server-side.** `xdg_toplevel.move` does not ask the client to
move itself: the compositor takes an interactive grab, follows the pointer, and
repositions the window while the client does nothing. The title bar, its drag
region and its close button belong to the compositor, negotiated through
`xdg-decoration`. Wayland's default is the opposite; this inverts it because a
client here may be a hundred and forty lines and should not have to reimplement
a title bar to have one.

**Compositor policy, and nowhere else.** Escape ends the session, Tab cycles
focus, click focuses and raises. None of it reaches a client. This is job 6, and
it lives in one place.

## The Quark port of libwayland

Patched, and kept in `explosion/toolchain/patches/` beside the musl and gcc
patches. Two things need changing and the rest should compile.

**Finding the compositor.** `wl_display_connect` reads `$WAYLAND_DISPLAY` and
opens a socket at `$XDG_RUNTIME_DIR/wayland-0`. Quark has no environment and no
filesystem sockets. The Quark path is a nameserver lookup for `wm` and an IPC
handshake in which the compositor creates a socketpair and installs one end in
the caller's descriptor table; `wl_display_connect` returns that descriptor and
everything above it is unmodified. `wl_display_connect_to_fd` already exists and
does the rest, so the patch is small and confined to one function.

**`wl_shm` memory.** A client creates the pool with `memfd_create` or
`shm_open` and mmaps it. Phase 10's memory-as-a-descriptor is exactly that
shape, so this may need no patch at all beyond musl answering `memfd_create` —
which belongs in the translation layer rather than in libwayland.

Everything else — the connection buffer, the closure marshalling, the proxy and
listener machinery, the object id allocator — is portable C over `sendmsg`,
`recvmsg`, `poll`, `mmap` and pthread, and should build once those exist.

## Budgets

Three limits will be hit, and two of them by the MVP:

- **Descriptor tables are eight entries.** A client holds stdin, stdout, stderr,
  its compositor connection, and a memory object per pool — and a terminal adds
  a clipboard pipe. Eight is not enough; raising it belongs in Phase 10.
- **`MAX_SHMEM` is 32 regions system-wide**, which is half a region per task.
  Linux's SysV limit is 4096 and its POSIX shared memory has no count limit at
  all; macOS's 32 is a legacy knob nothing modern uses. 256 costs about 14 KB.
- **`MAX_PAGES_PER_REGION` is 1024 and a region is one contiguous run.** A
  fullscreen 1280×800 buffer is exactly 1000 pages. Raising the ceiling to 4096
  covers 1920×1080 double-buffered, but the ceiling is only a promise the
  allocator can keep if a region becomes a *list* of contiguous runs.

## Known holes this design touches

**`sys_cap_grant` checks the source and not the destination.** It verifies the
caller holds the capability being granted and that the target's slot is empty,
and nothing else — so any task can fill any other task's sixteen CSpace slots.
That cannot raise anyone's authority, but a service that can no longer be handed
a capability can no longer be handed the display. Recorded against Phase 2 in
the roadmap. Phase 10 reduces how much this matters, since a descriptor passed
across a stream is a grant the receiver asked for.

**The compositor cannot be given the band it needs.** `sys_task_priority` lets
a caller make a task only equal to or worse than itself, and a compositor
started from the shell inherits `PRIO_NORMAL` — the same band as the clients
that block on it. The fix is a capability to grant a band without being in it,
which `init` passes down to the shell: authority held and passed on but never
exercised, which is how the rest of this system works. It widens what a shell
may do to what it starts, which is the trade `startx` made by being setuid.

## Milestones

| | Client | What it proves | What it needs beyond the compositor |
|---|---|---|---|
| **MVP** | `weston-simple-shm` | a real Wayland client, unmodified, draws on Quark | libffi, libwayland-client, xdg-shell |
| **Near** | `weston-terminal` | text, input, clipboard, menus | cairo, pixman, freetype, fontconfig |
| **Mid to long** | GTK and Qt applications | a desktop is possible | glib, gio, pango, harfbuzz, and further |

## Open questions

- **Which protocol versions the milestone clients actually demand.** Answerable
  by running them and reading the error, and not before.
- **Whether `poll` is a syscall or a descriptor.** A syscall taking an array is
  the obvious shape; an epoll-like object scales better and is more to build.
  Phase 10's decision, not this one's.
- **Whether the compositor keeps a Quark-native path at all.** The text console
  is a framebuffer client that does not need Wayland, and making it speak the
  protocol to draw a TTY may be worse than leaving it as a direct claimant of
  `user/fb`. Decide once the compositor exists.
