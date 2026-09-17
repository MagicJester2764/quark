# Phase 14 — What Phase 8 left out: the rest of the protocol, and a window manager

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans
> to implement this plan task by task, inline, on `main`. Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** A window can be moved, resized, maximised and closed with the
pointer; a scroll wheel reaches a client as `wl_pointer` version 5 events; a
client learns which output it is on; and the middle button pastes.

**Architecture:** The compositor already has a window stack, a pointer and a
seat. This adds an *interaction* layer over them — a grab, which is what a
compositor does between a button press on its own chrome and the release that
ends it — and the protocol that goes with it: `xdg_toplevel.move`, `.resize`,
`.set_maximized`, `.close` and the configure round trip that makes a resize
agreed rather than imposed. Below that, the PS/2 mouse learns the IMPS/2
handshake so that a wheel exists at all, and the event travels through the
input server to the compositor unchanged in shape.

**Tech stack:** Rust (`no_std`) for `user/wm`, `user/input`, `user/keyboard`;
C for the clients in `../explosion/toolchain`. Wayland's wire format as
`quark_rt::wl::wire`; the interfaces are written out in `user/wm/src/protocol.rs`.

**Spec:** `~/src/osdev/ROADMAP.md`, "Phase 14 — What Phase 8 left out". Its
acceptance sentence is: *done when a window can be moved and resized with the
pointer, and a scroll wheel reaches a client.*

## Global constraints

- Inline execution on `main`, a commit per task, pushed. No branches.
- Every task is verified by booting the image
  (`../explosion/tools/boot-test.sh`) and reading the screen; user-space
  output does not reach serial.
- `wm` keeps the invariants in `quark/CLAUDE.md`: a client's request is read
  inside the request, an unanswerable request is a `wl_display.error` naming
  the object and the reason, a slot is not freed while an object still names
  it, and no server blocks on one client.
- Nothing here patches an upstream client. If `weston-simple-shm` or
  `wlcairo` needs changing to work, that is a compositor bug.
- A protocol version is advertised only when every event of it is sent:
  `wl_seat` goes to 5 in the same task that sends `frame` and the axis
  events, and not before.

---

### Task 1: The wheel exists

**Files:**
- Modify: `quark/user/keyboard/src/main.rs` (IMPS/2 handshake, four-byte
  packets, a wheel field on a mouse event)
- Modify: `quark/user/input/src/main.rs` (carry the wheel through
  `TAG_INPUT_MOUSE`)
- Modify: `quark/user/mousetest/src/main.rs` (count wheel clicks and say so)

**Interfaces:**
- Produces: the keyboard driver's `TAG_MOUSE_EVENT` gains `data[3] = wheel`,
  a signed count of detents since the last packet, positive towards the user —
  the sign the hardware reports, and the one `wl_pointer.axis` wants, so
  nothing between the two flips it. `TAG_INPUT_MOUSE` carries it in `data[3]`
  unchanged, because the input server passes the driver's reply on whole.

- [x] **Step 1: The failing check.** `mousetest` prints `wheel: N` in its
  summary. `../explosion/tools/drive-qemu.py` gains a `wheel <up|down> <n>`
  op, which is `n` pairs of QMP `input-send-event` with
  `{"type": "btn", "data": {"down": true, "button": "wheel-up"}}` and the
  same with `down: false` — a wheel is a button in QEMU's input model, not an
  axis. Boot, run `mousetest`, send five clicks. Expected: `wheel: 0`,
  because a three-byte packet has no wheel byte and the driver never asked
  for a fourth.

- [x] **Step 2: The handshake.** In `enable_mouse`, after
  `MOUSE_SET_DEFAULTS`, send the magic knock — sample rate 200, 100, 80 —
  then `MOUSE_GET_DEVICE_ID` (0xF2). An id of 3 means four-byte packets with
  a Z byte; anything else leaves the decoder at three. Keep the result in a
  static the decoder reads, because the packet length changes with it.

- [x] **Step 3: The decoder.** `MouseDecoder` takes a fourth byte when the id
  is 3 or more, and the two devices spell it differently: IMPS/2 (3) puts a
  signed byte there, IMEX (4) keeps the wheel in the low four bits and the
  fourth and fifth buttons above it, which this system ignores rather than
  reading as a wheel spun eight detents at once. The length comes from the id
  and not a guess: four bytes taken from a mouse sending three eats the next
  packet's first byte and loses synchronisation for good. A packet whose first
  byte has bit 3 clear is out of step and is dropped, as it is now.

- [x] **Step 4: Verify.** Boot, run `mousetest`, drive the wheel, and read
  `wheel: N` with N the number of clicks sent. `dtest` still passes, and
  typing at the prompt still works — the same controller carries both.

- [ ] **Step 5: Commit.** quark: "A wheel on the same controller".

---

### Task 2: `wl_pointer` version 5

**Files:**
- Modify: `quark/user/wm/src/protocol.rs` (`SEAT` to version 5; `POINTER_FRAME`,
  `POINTER_AXIS_SOURCE`, `POINTER_AXIS_STOP`, `POINTER_AXIS_DISCRETE`; the
  axis and axis-source enumerations)
- Modify: `quark/user/wm/src/client.rs` (`pointer_axis`, `pointer_frame`, and
  a `frame` after every pointer event group)
- Modify: `quark/user/wm/src/seat.rs` (an axis reaches the focused client)
- Modify: `quark/user/wm/src/main.rs` (`pump_mouse` reads the wheel)
- Create: `explosion/toolchain/wlscroll.c`; Modify:
  `explosion/toolchain/build-weston-client.sh`

**Interfaces:**
- Produces: `wlscroll` — a client that takes a pointer at version 5, prints
  one line per `axis`/`frame` group (`axis vertical 10.0 discrete 1`), and
  exits after twenty of them or ten seconds.
- Produces: every pointer event group ends with `wl_pointer.frame`, which is
  what version 5 means: enter, motion, button and axis are all parts of one
  group and a client applies them together.

- [ ] **Step 1: The failing check.** Build `wlscroll`; boot;
  `wm wlscroll`; send wheel events. Expected: nothing is printed, because the
  seat is advertised at 4 and no axis event is sent.

- [ ] **Step 2: The events.** `pointer_axis(id, axis, value)` sends
  `wl_pointer.axis(time, axis, value)` with `value` in fixed point — one
  detent is 10.0, as Weston sends — then `axis_discrete(axis, 1)` for a
  client at version 5, then `frame`. A wheel that stops sends `axis_stop`.
  `axis_source` says `wheel` (0).

- [ ] **Step 3: Version 5 everywhere.** `SEAT` becomes version 5, and
  `request_count` and the version checks stay as they are: a client that binds
  at 4 gets no `frame`, and `insert_at` already records what it bound at.
  Every place that sends a pointer event sends `frame` after it, guarded on
  the version.

- [ ] **Step 4: Verify.** `wm wlscroll`, wheel up and down: one line per
  detent, with the sign right. `wm weston-simple-shm wlcairo` still behaves,
  and `wlprobe` still reports the seat it gets. `wlfuzz` still leaves the
  compositor standing (twenty seeds).

- [ ] **Step 5: Commit.** quark: "A scroll wheel reaches a client";
  explosion: "wlscroll".

---

### Task 3: A window can be moved

**Files:**
- Create: `quark/user/wm/src/grab.rs` (what the pointer is doing between a
  press and its release)
- Modify: `quark/user/wm/src/main.rs` (`dispatch_pointer` asks the grab
  first; the title bar is a handle), `quark/user/wm/src/client.rs`
  (`xdg_toplevel.move`)

**Interfaces:**
- Produces: `grab::start_move(window, x, y)`, `grab::motion(x, y) -> bool`
  (true when the grab consumed the movement), `grab::release()`; a grab is
  ended by the button going up, by the window going away, or by the client
  disconnecting.
- Produces: `xdg_toplevel.move(seat, serial)` starts the same grab, which is
  how a client-side title bar asks for one.

- [ ] **Step 1: The failing check.** A key script that starts
  `wm weston-simple-shm`, presses the left button on the title bar, moves the
  pointer 200 pixels right and 100 down, releases, and takes a screenshot.
  Expected today: the window has not moved, and the click only raised it.

- [ ] **Step 2: The grab.** A press inside a window's title bar (the frame's
  top `TITLE_H` pixels, outside the close button) starts a move grab
  recording the window and the pointer's offset inside it. Motion sets the
  window's `x` and `y` — clamped so that at least the title bar stays on
  screen — and repaints the union of the old and new rectangles. The release
  ends it. While a grab is on, pointer events do not reach any client: the
  pointer belongs to the compositor.

- [ ] **Step 3: The request.** `xdg_toplevel.move(seat, serial)` starts the
  same grab for the window the surface is showing in, with the offset taken
  from the pointer's current position. The serial is not checked against a
  press, because this compositor does not keep a history of serials; that is
  written down as a gap rather than pretended.

- [ ] **Step 4: Verify.** The Step 1 script: the window is where it was
  dragged to, its contents intact, the backdrop repainted behind it. A drag
  that leaves the screen edge keeps the title bar reachable. `wlcairo` keeps
  animating while it is dragged.

- [ ] **Step 5: Commit.** quark: "A window can be moved".

---

### Task 4: A window can be resized

**Files:**
- Modify: `quark/user/wm/src/grab.rs` (a resize grab and its edges),
  `quark/user/wm/src/main.rs` (the frame's edges are handles),
  `quark/user/wm/src/client.rs` (`xdg_toplevel.resize`, a configure with a
  size and the `resizing` state), `quark/user/wm/src/shell.rs` (a configure
  the compositor asks for rather than one it sends once)

**Interfaces:**
- Produces: a press within `BORDER + 2` pixels of an edge or corner starts a
  resize grab; motion sends `xdg_toplevel.configure(width, height, states)`
  with `resizing` set, and `xdg_surface.configure(serial)` after it, at most
  one per tick. The client acknowledges and attaches a buffer of that size;
  the window follows the buffer, as it does now.
- Produces: `xdg_toplevel.resize(seat, serial, edges)` starts the same grab.
- Produces: a client that never acknowledges is not waited for: the window
  keeps the size of the buffer it last attached.

- [ ] **Step 1: The failing check.** A key script that drags the bottom-right
  corner of `wlcairo`'s window 150 pixels out and screenshots. Expected
  today: the window does not change size, and the drag moves the pointer over
  the backdrop.

- [ ] **Step 2: The grab.** `grab::start_resize(window, edges, x, y)` keeps
  the window's rectangle at the start and computes a new one per motion,
  clamped to a minimum of 64 by 48 and to the screen. The compositor does not
  resize the window itself: it asks, and the client's next commit is what
  changes it.

- [ ] **Step 3: The configure.** A configure carries the state array —
  `resizing` (3) while the grab is on, `activated` (4) when the surface has
  focus — which means `arg_array` with four-byte entries rather than the
  empty array sent today. Sizes are the *content* size, without the frame.

- [ ] **Step 4: Verify.** The Step 1 script: `wlcairo` redraws at the new
  size with its scene scaled to it, and `weston-simple-shm` too. Releasing
  sends a configure without `resizing`. A client that ignores the configure
  (`wlprobe`) keeps its old size and nothing breaks.

- [ ] **Step 5: Commit.** quark: "A window can be resized".

---

### Task 5: Maximise, and a close button

**Files:**
- Modify: `quark/user/wm/src/client.rs` (`set_maximized`, `unset_maximized`,
  `xdg_toplevel.close`), `quark/user/wm/src/main.rs` (a close box in the
  title bar, and the maximised geometry), `quark/user/wm/src/surface.rs`
  (remember the size to go back to)

**Interfaces:**
- Produces: `set_maximized` configures the window to the whole screen less
  the frame, with `maximized` (1) in the states; `unset_maximized` configures
  it back to the size it had. A double click on the title bar does the same.
- Produces: a close box at the right of the title bar sends
  `xdg_toplevel.close`, which is a request to the client rather than an order:
  a client that ignores it stays. `wm` prints nothing; the client decides.

- [ ] **Step 1: The failing check.** Click where the close box will be on
  `wlcairo`'s title bar; double click its title bar. Expected today: both
  raise the window and nothing else.

- [ ] **Step 2: The close box.** Drawn in `draw_window` as an `×` in a box at
  the frame's top right, hit-tested before the move grab so that a press
  there is a close and not a drag.

- [ ] **Step 3: Maximise.** The states array, the saved geometry, and the
  double-click timing (two presses within 50 ticks in the same title bar).

- [ ] **Step 4: Verify.** `wlcairo` closes when its box is clicked (it exits,
  and the session ends when its last program does); `weston-simple-shm`
  maximises and comes back to its old size. A client that has no close
  handler stays open.

- [ ] **Step 5: Commit.** quark: "Maximise, and a close button".

---

### Task 6: A surface knows which output it is on

**Files:**
- Modify: `quark/user/wm/src/client.rs` (`wl_surface.enter` and `.leave`),
  `quark/user/wm/src/surface.rs` (remember whether a surface has been told)

**Interfaces:**
- Produces: a surface that becomes visible is sent `wl_surface.enter` with
  the client's `wl_output` object, if it has bound one, and `leave` when it
  stops being visible. A client that binds the output later is told then.

- [ ] **Step 1: The failing check.** `wlprobe` prints the outputs its surface
  is on. Expected today: none, and the compositor never sends the event —
  `SURFACE_ENTER` is a constant nothing uses.

- [ ] **Step 2: Send it.** On the commit that gives a surface its window, and
  on `bind` of `wl_output` for a client that already has one.

- [ ] **Step 3: Verify.** `wm wlprobe`: it names the output. `wlfuzz` still
  leaves the compositor standing.

- [ ] **Step 4: Commit.** quark: "A surface knows which output it is on".

---

### Task 7: The middle button pastes

**Files:**
- Modify: `quark/user/wm/src/protocol.rs` (the primary-selection interfaces),
  `quark/user/wm/src/client.rs` (device, source and offer),
  `quark/user/wm/src/clipboard.rs` (a second selection beside the first)
- Modify: `explosion/toolchain/wlclip.c` (a `--primary` mode)

**Interfaces:**
- Produces: `zwp_primary_selection_device_manager_v1` at version 1, with
  `zwp_primary_selection_device_v1`, `_source_v1` and `_offer_v1`. The
  selection is set by a client and announced to whoever has keyboard focus,
  exactly as the clipboard is; the compositor never sees the bytes.

- [ ] **Step 1: The failing check.** `wlclip --primary copy` and
  `wlclip --primary paste` in one session. Expected today: the manager is not
  advertised and the client says so.

- [ ] **Step 2: The interfaces.** A second `Mimes` and owner in
  `clipboard.rs`, and the three objects in `client.rs`, written as the
  clipboard's are. The offer is named by the compositor, as the clipboard's
  is.

- [ ] **Step 3: Verify.** `wm "wlclip --primary copy" "wlclip --primary
  paste"` moves the text; the ordinary clipboard still works alongside it;
  `wlfuzz` still leaves the compositor standing.

- [ ] **Step 4: Commit.** quark: "The middle button pastes"; explosion:
  "wlclip: the primary selection".

---

### Task 8: Write it down

- [ ] `quark/CLAUDE.md`: the grab (the pointer belongs to the compositor
  between a press on its chrome and the release), the configure round trip
  (a size is agreed, not imposed), and which versions each interface is
  advertised at and why.
- [ ] `~/src/osdev/ROADMAP.md`: Phase 14 done, what it took, what is left
  (drag and drop, touch, key repeat as a compositor policy).
- [ ] Tick this plan; commit and push quark and explosion.

---

## Acceptance

A window can be moved by its title bar and resized by its corner, with
`wlcairo` and `weston-simple-shm` both redrawing at the new size; a wheel
click reaches `wlscroll` as a `wl_pointer.axis` with a `frame` after it; a
close box asks a client to go and it goes; `wlprobe` names its output; the
middle button pastes. `wlfuzz` still leaves the compositor standing for twenty
seeds, `runtests /etc/hostile.tests` is still clean, and `dtest`, the C
library's suite and the font suites still pass on ext2 and ext4.
