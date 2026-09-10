# Quark display protocol

**Version 1.0.** The interface between a display server and the programs that
draw on it.

This document is the contract. A client should be writable against this
document alone, without reading the compositor's source — the same standard
`docs/abi.md` sets for the kernel.

## What this is, and what it is not

This is **Wayland's object model, carried over Quark's IPC**. The structure is
Wayland's: a registry of versioned globals, surfaces that mean nothing until
given a role, atomically committed double-buffered state, buffers lent to the
compositor and released back, a seat that owns the input devices. Anyone who
knows Wayland will recognise every object here.

It is **not Wayland's wire protocol**, and a Wayland client cannot connect to
it. That format assumes a byte-stream socket, file-descriptor passing, and a
server that may write to a client whenever it likes. Quark has fixed 48-byte
synchronous messages and shared memory; the prefix is `qw_` rather than `wl_`
so that nothing about the name promises otherwise.

What was worth copying is the model, because it is the part that survived
contact with real desktops. The wire format is a consequence of Wayland running
on Unix sockets, and reproducing it here would mean building a socket, an
fd-passing mechanism and a push channel that Quark does not otherwise need — to
gain compatibility with clients that also want EGL, epoll and dbus.

### The six jobs

Every system that puts a window on a screen does the same six things, and it is
worth naming them because this protocol is only about some of them:

1. Own the screen — mode, pixel format, and the memory the display scans out.
2. Give each application somewhere to draw.
3. Decide where each buffer goes, and in what stacking order.
4. Composite the visible parts into the screen.
5. Route input to whichever client it belongs to.
6. Furniture and policy: title bars, focus, dragging, closing.

Job 1 is **not in this protocol**. `user/fb` is the framebuffer device — it
owns the hardware the way `/dev/fb0` does and lends the display by capability —
and clients never see it. Everything else is here.

X11 split jobs 3 and 6 into a separate window-manager process from the server
doing 1, 4 and 5, and the two disagreed in the gap between them; a window that
flickers at the wrong size is that disagreement. Wayland collapsed them, so
"compositor" and "window manager" name one program. `user/wm` is that program.

## Transport

Two mechanisms, and the asymmetry is deliberate: **requests need an
authenticated sender, events need throughput.**

### Requests: `sys_call`

A request is a synchronous call to the compositor:

```
tag      = opcode
data[0]  = object id
data[1..5] = arguments
```

The compositor looks up `data[0]` in the object table *of that sender*, which
yields the interface, and dispatches the opcode within it. That is Wayland's
`(object, opcode)` pair in registers rather than in a byte stream.

Object tables are per client and the sender is the kernel's — `sys_recv`
overwrites the message's `sender` field with the calling task ID — so a client
**cannot name another client's objects**. That property falls out of the
transport rather than being enforced by a check.

Every request has a reply: `QW_OK` (0) or an error code. Wayland has no replies
because a socket has none, and defers errors to a `wl_display.error` event;
here an error arrives at the call site, which is better and is a deliberate
divergence.

Five words is 40 bytes of arguments, which covers every request in this
version. **Variable-length data in a request travels in the client's own pool**
as an offset and a length — titles, MIME types, clipboard contents — so there is
one rule rather than an inline case and a chunking case. A client with no pool
cannot set a title, which is not a limitation worth removing: it has no buffer
either, so it has no window to name.

Variable-length data in an *event* goes inline in the ring instead, spanning
continuation records. The compositor must not write into a client's pool: that
memory belongs to the client's own allocator, and the compositor has no way to
know what is free.

### Events: a ring in shared memory, woken by futex

At connect the compositor creates a two-page shared region, grants it to the
client, and reports the handle. The client maps it. Layout:

```
offset  0   head      u32   written by the compositor; also the futex word
offset  4   tail      u32   written by the client
offset  8   capacity  u32   number of record slots; always a power of two
offset 12   overflow  u32   set by the compositor, cleared by the client
offset 16   (reserved, zero)
offset 32   records[capacity]
```

The region is **two pages** and `capacity` is **128**, so the records occupy
4096 bytes from offset 32 and the rest is reserved. Two pages leaves room for
255 records, which is not a power of two, and a mask is worth more than the
slack.

A record is 32 bytes:

```
offset  0   object    u32
offset  4   opcode    u16   0xFFFF means "continuation of the previous record"
offset  6   len       u16   total payload length in bytes, across continuations
offset  8   payload   24 bytes
```

`head` and `tail` are free-running counters, not indices; a slot is
`counter & (capacity - 1)`. They wrap at 2^32, and every comparison between
them is wrapping arithmetic — `head.wrapping_sub(tail)` is the fill level, and
a plain `>` between the two is wrong.

An event longer than 24 bytes occupies further records with opcode `0xFFFF`, so
there is no arbitrary ceiling on an event's size — which is what stops the next
MIME type or window title from needing a protocol change.

**Ordering.** The compositor writes a record's bytes, issues a release fence,
*then* stores `head`, then calls `sys_futex_wake(&head, n)`. The client loads
`head` with acquire ordering before reading any record. On x86 the fences are
compiler barriers, since stores are not reordered with stores; on a weaker
architecture they are real instructions, and the rule is written here so that
porting finds it rather than debugging does.

**Waiting.** The client compares `head` to its own `tail`; if they are equal it
calls `sys_futex_wait(&head, head)`. The wake that lands between the load and
the wait is not a lost wake-up: `futex_wait` returns immediately when the word
no longer holds the expected value. A client with nothing to do therefore
consumes no CPU and makes no system calls. The kernel keys its wait queue on
the *physical* address, so this works between address spaces with no special
support.

**`tail` is untrusted.** The client owns it, and a client may write anything
there. Before computing free space the compositor must check
`head.wrapping_sub(tail) <= capacity`; if that fails, the ring is treated as
overflowed and reset. One wrapping subtraction covers both a `tail` ahead of
`head` and one impossibly far behind it, which a pair of ordinary comparisons
does not. Without the check, a client writing nonsense into its own ring header
becomes an out-of-bounds write *inside the compositor*.

**Coalescing.** Before appending a `qw_pointer.motion` event the compositor
replaces an unread `motion` already at the head of the ring for the same
pointer, rather than adding a second one. A client only ever wants the latest
position, and without this a pointer dragged across the screen can overflow a
128-slot ring on its own. No other event is ever coalesced.

**Overflow.** If the ring is full the compositor sets `overflow` and drops the
event. It never blocks: a client that has stopped draining must not be able to
stall the display. The client, on seeing `overflow`, clears it and issues
`qw_display.sync`, and the compositor then re-sends the state that must be
*correct* rather than merely current — keyboard modifiers, keyboard and pointer
focus, and each toplevel's configure. That is what stops a dropped key-release
leaving a modifier stuck down forever.

### The same ring, three times

`user/i8042` to `user/input`, `user/input` to the compositor, and the
compositor to each client all use this structure. The input server wakes the
compositor with `sys_notify`, which is non-blocking and wakes a task blocked in
`sys_recv(TID_ANY)`; the compositor therefore waits in one place for both
client requests and input, and polls nothing.

For that to be possible the compositor mints an `Endpoint` naming itself and
grants it into the input server's CSpace. Minting an endpoint that names only
yourself is always permitted (`docs/abi.md`), and `sys_cap_grant` requires only
that the destination slot is empty. See **Known holes** below.

### Connect

`sys_call(compositor, QW_CONNECT)` with no arguments, where **`QW_CONNECT` is
tag `0xFFFF_FFFF`**. It is the one message that is not `(object, opcode)`,
because it is what creates the object table it would otherwise be dispatched
through; the tag is outside the opcode range so it can never collide with one.
The reply carries the event-ring shared-memory handle in `data[0]` and its
capacity in `data[1]`. A second `QW_CONNECT` from a client that already has a
table is `QW_ERR_INVALID_OPCODE`, not a reset.
After mapping it, object id **1** is the client's `qw_display`, and everything
else is reached through the registry.

## Objects

An object has an id, an interface, and a version. Ids are 32-bit and allocated
by the **client**, from 1 upwards — which is why a request that creates an
object carries the id it is about to use instead of waiting to be told one.

Ids at or above `0xFF00_0000` are allocated by the **compositor**. Only
`qw_data_device.data_offer` uses that range in this version, but the split
exists from the start because retrofitting it would renumber every object.

Id 0 is null, and is a valid argument wherever an object argument is optional.

Destroying an object is a request on it. The compositor answers with a
`qw_display.delete_id` event when the id is safe to reuse, because a client may
have issued requests naming that id which are still in flight.

### Versioning, and why the registry exists

The compositor advertises what it has as `qw_registry.global(name, interface,
version)` events. A client **binds** the globals it understands and ignores the
rest, choosing a version no higher than both what it supports and what was
advertised.

That is the whole extensibility story. A client written against version 1 of an
interface keeps working against a compositor that has grown version 2, and a
compositor without a feature simply never advertises it — the client learns
this by *not being told*, rather than by making a request and getting an error.
A protocol of bare message tags has neither property: every new feature is a
new tag, and no client can ask whether it exists.

`name` is an opaque per-compositor identifier for one instance of a global, not
an interface id; `global_remove` names the same value.

## Interfaces

Interface ids appear in `global` and `bind`. Opcodes are per interface and
independent between requests and events, both starting at 0.

| Id | Interface | Version |
|---|---|---|
| 1 | `qw_display` | 1 |
| 2 | `qw_registry` | 1 |
| 3 | `qw_compositor` | 1 |
| 4 | `qw_surface` | 1 |
| 5 | `qw_shm` | 1 |
| 6 | `qw_shm_pool` | 1 |
| 7 | `qw_buffer` | 1 |
| 8 | `qw_seat` | 1 |
| 9 | `qw_keyboard` | 1 |
| 10 | `qw_pointer` | 1 |
| 11 | `qw_output` | 1 |
| 12 | `qw_shell` | 1 |
| 13 | `qw_toplevel` | 1 |
| 14 | `qw_callback` | 1 |
| 15 | `qw_data_device_manager` | 1 |
| 16 | `qw_data_device` | 1 |
| 17 | `qw_data_source` | 1 |
| 18 | `qw_data_offer` | 1 |

Globals — the interfaces a client may `bind` — are `qw_compositor`, `qw_shm`,
`qw_seat`, `qw_output`, `qw_shell` and `qw_data_device_manager`. The rest are
created through requests on those.

### `qw_display` (object 1, always present)

| Op | Request | Arguments |
|---|---|---|
| 0 | `get_registry` | `new_id` |
| 1 | `sync` | `new_id` (a `qw_callback`) |

| Op | Event | Payload |
|---|---|---|
| 0 | `error` | `object` u32, `code` u32, message (bytes) |
| 1 | `delete_id` | `id` u32 |

`sync` is a round-trip barrier: the callback's `done` event arrives after every
event queued before the request. It is how a client knows the initial burst of
`global` events is finished, and how it resynchronises after an overflow.

`error` reports a **fatal** protocol violation, after which the compositor
disconnects the client. Errors in a single request come back as that request's
reply instead.

### `qw_registry`

| Op | Request | Arguments |
|---|---|---|
| 0 | `bind` | `name` u32, `interface` u32, `version` u32, `new_id` u32 |

| Op | Event | Payload |
|---|---|---|
| 0 | `global` | `name` u32, `interface` u32, `version` u32 |
| 1 | `global_remove` | `name` u32 |

### `qw_compositor`

| Op | Request | Arguments |
|---|---|---|
| 0 | `create_surface` | `new_id` |

### `qw_surface`

| Op | Request | Arguments |
|---|---|---|
| 0 | `destroy` | — |
| 1 | `attach` | `buffer` (object or 0), `x` i32, `y` i32 |
| 2 | `damage` | `x` i32, `y` i32, `w` i32, `h` i32 |
| 3 | `frame` | `new_id` (a `qw_callback`) |
| 4 | `commit` | — |

| Op | Event | Payload |
|---|---|---|
| 0 | `enter` | `output` u32 |
| 1 | `leave` | `output` u32 |

**`attach`, `damage` and `frame` do nothing on their own.** They accumulate
*pending* state. `commit` applies all of it at once, atomically. This is the
single most important rule in the protocol: it makes a half-drawn or
half-resized frame not merely unlikely but unrepresentable.

`attach(0, …)` detaches, and the surface becomes invisible until a buffer is
attached and committed again.

Damage is in surface coordinates and accumulates across `damage` requests until
a `commit` consumes it. A commit with no damage still applies other pending
state; it just repaints nothing.

A surface with no role is never displayed.

### `qw_shm`, `qw_shm_pool`, `qw_buffer`

| Interface | Op | Request | Arguments |
|---|---|---|---|
| `qw_shm` | 0 | `create_pool` | `new_id`, `size` u32 (bytes) |
| `qw_shm_pool` | 0 | `create_buffer` | `new_id`, `offset` u32, `width` u32, `height` u32, `stride` u32, `format` u32 |
| `qw_shm_pool` | 1 | `destroy` | — |
| `qw_buffer` | 0 | `destroy` | — |

| Interface | Op | Event | Payload |
|---|---|---|---|
| `qw_shm` | 0 | `format` | `format` u32 |
| `qw_buffer` | 0 | `release` | — |

`create_pool`'s reply carries the shared-memory handle in `data[0]`; the client
maps it with `sys_shmem_map` at an address of its own choosing, because only
the client knows what else its address space holds.

`release` says the compositor has finished reading a buffer. **It is what makes
both single and double buffering correct**, and it is the arrow this system has
been missing: with two buffers a client draws into B while the compositor reads
A and swaps when told; with one buffer it simply waits for `release` before
redrawing. Double buffering is therefore the client's choice, not the
protocol's requirement — which matters, because a fullscreen 1280×800 buffer is
1000 pages and two of them do not fit in one region.

Formats are enumerated, and the channel layout is part of the name rather than
carried separately:

| Value | Format |
|---|---|
| 0 | `QW_FORMAT_XRGB8888` |
| 1 | `QW_FORMAT_XBGR8888` |

The compositor advertises every format it can composite without conversion —
in practice the one its output is in — with a `qw_shm.format` event per format.
A buffer in any other format is a protocol error.

### `qw_seat`, `qw_keyboard`, `qw_pointer`

A seat is a group of input devices belonging to one user, and it is lent as a
group: a program cannot take the keyboard and leave the pointer.

| Interface | Op | Request | Arguments |
|---|---|---|---|
| `qw_seat` | 0 | `get_keyboard` | `new_id` |
| `qw_seat` | 1 | `get_pointer` | `new_id` |
| `qw_keyboard` | 0 | `release` | — |
| `qw_pointer` | 0 | `set_cursor` | `serial` u32, `surface` (object or 0), `hotspot_x` i32, `hotspot_y` i32 |
| `qw_pointer` | 1 | `release` | — |

| Interface | Op | Event | Payload |
|---|---|---|---|
| `qw_seat` | 0 | `capabilities` | `caps` u32 — bit 0 pointer, bit 1 keyboard |
| `qw_keyboard` | 0 | `enter` | `serial` u32, `surface` u32, `nkeys` u32, `keys` u8[] |
| `qw_keyboard` | 1 | `leave` | `serial` u32, `surface` u32 |
| `qw_keyboard` | 2 | `key` | `serial` u32, `time` u32, `scancode` u32, `ascii` u32, `state` u32 |
| `qw_keyboard` | 3 | `modifiers` | `serial` u32, `mods` u32 |
| `qw_pointer` | 0 | `enter` | `serial` u32, `surface` u32, `sx` i32, `sy` i32 |
| `qw_pointer` | 1 | `leave` | `serial` u32, `surface` u32 |
| `qw_pointer` | 2 | `motion` | `time` u32, `sx` i32, `sy` i32 |
| `qw_pointer` | 3 | `button` | `serial` u32, `time` u32, `button` u32, `state` u32 |
| `qw_pointer` | 4 | `axis` | `time` u32, `axis` u32, `value` i32 |

`state` is 0 for released and 1 for pressed. Buttons are 0 left, 1 right, 2
middle. Axis 0 is vertical scroll, 1 horizontal, and `value` is in notches —
positive is down and right. Pointer coordinates are **surface-local**.

There is no keymap object. Quark's keyboard driver already translates
scancodes, so `key` carries both the raw scancode and the resulting ASCII byte,
and a client may use either.

### `qw_output`

| Op | Request | Arguments |
|---|---|---|
| 0 | `release` | — |

| Op | Event | Payload |
|---|---|---|
| 0 | `mode` | `width` u32, `height` u32, `refresh` u32 (mHz, 0 if unknown) |
| 1 | `done` | — |

`done` ends an atomic burst of output properties, so a client never acts on
half a mode change.

### `qw_shell`, `qw_toplevel`

The role that turns a surface into an application window.

| Interface | Op | Request | Arguments |
|---|---|---|---|
| `qw_shell` | 0 | `get_toplevel` | `new_id`, `surface` u32 |
| `qw_toplevel` | 0 | `destroy` | — |
| `qw_toplevel` | 1 | `set_title` | `pool` u32, `offset` u32, `len` u32 |
| `qw_toplevel` | 2 | `move` | `seat` u32, `serial` u32 |

| Interface | Op | Event | Payload |
|---|---|---|---|
| `qw_toplevel` | 0 | `configure` | `width` u32, `height` u32, `states` u32 |
| `qw_toplevel` | 1 | `close` | — |

`move` does not tell the client to move itself. The compositor takes an
interactive move grab, follows the pointer until the button is released, and
repositions the window; the client does nothing. The same shape will serve
resize when it is added.

`states` is a bitmask; bit 0 is **activated**, meaning this toplevel holds
keyboard focus, and it is the only bit defined in this version. A client that
draws its own focus indication reads it here rather than inferring focus from
keyboard events.

`close` is a request to shut down, not an instruction — the client decides, and
may ignore it.

**Decorations are server-side.** The compositor draws the title bar, and the
drag region and close button are therefore its own. Wayland's default is the
opposite, with server-side decoration as an extension; this inverts it
deliberately, because a client here may be a hundred and forty lines and should
not have to reimplement a title bar to have one.

### `qw_callback`

| Op | Event | Payload |
|---|---|---|
| 0 | `done` | `data` u32 |

Created by `qw_surface.frame` and `qw_display.sync`, and destroyed by the
compositor after `done` — which is why `done` is followed by a
`qw_display.delete_id` for it. A frame callback's `data` is the time in ticks;
a sync callback's is the serial current when the sync was answered.

**A frame callback is the throttle.** A client that draws only when told never
draws faster than the screen updates, and never spins guessing a rate. Its
absence is why an animating client used to keep the compositor too busy to read
the keyboard.

### Clipboard: `qw_data_device_manager` and friends

| Interface | Op | Request | Arguments |
|---|---|---|---|
| `qw_data_device_manager` | 0 | `create_data_source` | `new_id` |
| `qw_data_device_manager` | 1 | `get_data_device` | `new_id`, `seat` u32 |
| `qw_data_source` | 0 | `offer` | `pool` u32, `offset` u32, `len` u32 (a MIME type) |
| `qw_data_source` | 1 | `destroy` | — |
| `qw_data_device` | 0 | `set_selection` | `source` u32, `serial` u32 |
| `qw_data_device` | 1 | `release` | — |
| `qw_data_offer` | 0 | `receive` | `pool` u32, `offset` u32, `len` u32 (MIME type), `fd` u32 |
| `qw_data_offer` | 1 | `destroy` | — |

| Interface | Op | Event | Payload |
|---|---|---|---|
| `qw_data_source` | 0 | `send` | `fd` u32, MIME type (bytes) |
| `qw_data_source` | 1 | `cancelled` | — |
| `qw_data_device` | 0 | `data_offer` | `id` u32 (compositor-allocated) |
| `qw_data_device` | 1 | `selection` | `offer` u32 (or 0) |
| `qw_data_offer` | 0 | `offer` | MIME type (bytes) |

Wayland passes a pipe file descriptor over its socket for the data transfer.
Quark does not pass descriptors, and does not need to: the compositor creates a
pipe and installs an end directly into each client's descriptor table with
`sys_fd_dup`, for which it holds `TaskMgmt`. The receiving client names the
descriptor slot it wants in `receive`; the offering client is told which slot
its write end landed in by `send`. A task's descriptor table has eight entries
and 0, 1 and 2 are spoken for, so a client has five to choose from and gets
`QW_ERR_INVALID_ARG` for one that is occupied.

This is the only place in the protocol where the compositor allocates object
ids, because `data_offer` names an object the client did not ask for.

## Input semantics

### Two focuses

**Keyboard focus** is one surface. It changes on click and on the compositor's
own Tab binding. Every change is `leave` on the old surface and *then* `enter`
on the new, in that order, so a client can never believe it holds focus twice.

**Pointer focus** is whatever surface is under the cursor, and is independent
of keyboard focus. Moving across a window that does not have keyboard focus
still sends it `enter` and `motion`; that is how hover works without stealing
focus.

### The implicit grab

While any button is held, pointer events continue to go to the surface where
the press happened, even after the cursor leaves it. Without this, dragging
anything breaks the moment the pointer moves off the window, and a button
un-presses if the pointer slides off before release.

### Serials

Every input event carries a serial from a single monotonically increasing
counter. Requests that should only follow real input take one back —
`set_cursor` and `toplevel.move` — and the compositor rejects a serial that
does not name a recent event it actually sent. That is what prevents a client
spontaneously grabbing the pointer or starting a drag nobody asked for.

### The cursor is a surface

`set_cursor` takes a `qw_surface` with the cursor role, plus a hotspot. A
cursor is a small buffer at a position, so it needs no new object type, no new
buffer type and no new commit path — which is the role machinery paying for
itself. A client that sets no cursor gets a default arrow drawn by the
compositor, and the cursor is composited last, above everything.

### What the compositor keeps

Escape ends the session; Tab cycles keyboard focus. Neither reaches a client.
Click-to-focus and click-to-raise are compositor policy. This is job 6, and it
lives in exactly one place.

## Errors

| Code | Name | Meaning |
|---|---|---|
| 0 | `QW_OK` | success |
| 1 | `QW_ERR_INVALID_OBJECT` | no such object in this client's table |
| 2 | `QW_ERR_INVALID_OPCODE` | not a request of that object's interface |
| 3 | `QW_ERR_INVALID_ARG` | an argument is out of range or malformed |
| 4 | `QW_ERR_ID_IN_USE` | the `new_id` is already allocated |
| 5 | `QW_ERR_ID_RANGE` | a client used a compositor-allocated id |
| 6 | `QW_ERR_NO_MEMORY` | the compositor could not allocate |
| 7 | `QW_ERR_ROLE` | the surface already has a different role |
| 8 | `QW_ERR_FORMAT` | unsupported pixel format |
| 9 | `QW_ERR_BAD_SERIAL` | the serial names no recent input event |
| 10 | `QW_ERR_LIMIT` | a per-client limit was reached |

Codes 1, 2, 4 and 5 indicate a broken client rather than a failed operation,
and the compositor may follow the reply with a `qw_display.error` and
disconnect.

## Budgets

The kernel's shared-memory table is finite and this protocol must live inside
it. Per client: **one event ring, and pools as needed**. A pool carves out many
buffers, which is what pools are for, so a typical client costs two regions.

With `MAX_SHMEM` raised to 256 and `MAX_PAGES_PER_REGION` to 4096, a fullscreen
1920×1080 buffer fits in one region and a client double-buffering at that size
fits in one pool. Regions are lists of contiguous runs rather than one run, so
a large region does not require a large unfragmented span.

Per-client limits, enforced with `QW_ERR_LIMIT`: 64 objects, 8 surfaces, 4
pools, 16 buffers.

## Known holes

**`sys_cap_grant` checks the source and not the destination.** The compositor
grants an endpoint naming itself into the input server's CSpace so that input
can push events rather than be polled, and that is a legitimate thing for two
cooperating servers to do — but the same call lets any task fill any other
task's sixteen CSpace slots, which is a denial of service. Recorded against
Phase 2 in `ROADMAP.md`; the rule wanted is a quota or an accept step, not a
prohibition, and it should be written once there are two real cases to write it
from.

**The compositor needs a scheduling band it cannot be given.**
`sys_task_priority` lets a caller only make a task equal to or worse than
itself, and a compositor started from the shell inherits `PRIO_NORMAL` — the
same band as the clients that block on it, costing up to a quantum of input
latency behind a busy client. This version adds `CapType::Priority`, a
capability to grant a band without being in it, which `init` passes down to the
shell. It widens what a shell may do to what it starts, and that is the same
trade `startx` made by being setuid.

## Divergences from Wayland, listed

Kept deliberately, and each for a reason:

- **Requests have replies**, so errors arrive at the call site. A socket has no
  replies; Quark's IPC does.
- **Events are pulled from a ring rather than pushed down a socket**, because a
  server cannot originate IPC to a client it spawned. The futex makes the ring
  a real push in practice.
- **Buffers are shared-memory handles, not file descriptors**, and the
  clipboard's pipe is installed rather than passed.
- **Decorations are server-side by default**, rather than client-side with a
  server-side extension.
- **No keymap object**; `key` carries scancode and ASCII.
- **No `wl_subsurface`, no viewport, no output transforms, no fractional
  scaling.** One output, one scale, no rotation.
- **No resize.** `configure` exists and carries a size, but the compositor
  never changes it in this version. The machinery for a client to reallocate
  buffers on configure is present; the policy is not.
