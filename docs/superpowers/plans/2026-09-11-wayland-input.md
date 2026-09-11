# Wayland input implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to
> implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A Wayland client somebody else wrote can be typed into, and then
clicked on.

**Architecture:** The compositor already claims the keyboard with the display
and routes keys to the focused window; what is missing is the last hop, because
a Wayland client does not read the window event queue that routing fills. 8b.1
adds `wl_seat` and `wl_keyboard` and makes that hop. 8b.2 pays two debts the
first sub-phase leaves: the `sys_cap_grant` denial of service, whose fixing rule
can now be written from two real cases; and a real XKB keymap, so that a client
learns the layout instead of guessing it. 8c.1 adds the pointer, which needs a
mouse, which needs the i8042 demultiplex. 8c.2 clears the loose ends this phase
has been accumulating: the clipboard, decoration negotiation, and the names.

**Tech stack:** Rust `no_std` (`user/wm`, `user/keyboard`, `user/input`), the
Quark kernel, upstream libwayland, weston clients.

**Spec:** `docs/wayland.md` — the input rules there (two focuses, the implicit
grab, serials) are what this implements.

## Global constraints

- **`docs/abi.md` is the contract.** Any syscall change goes in it and bumps
  `ABI_VERSION_MINOR`; `tools/check-abi.sh` fails the build on drift. Current
  version is 1.9.
- **`user/wm` is `no_std` with no allocator.** Fixed arrays; refuse when full
  rather than growing.
- **A client is never trusted.** Every id, size and index it sends is checked
  before it indexes anything.
- **Verification is boot-in-QEMU.** `$SP/cycle.sh <keys> <ppm>` builds the
  kernel, relinks the weston clients (they link `liblinux-abi.a` by path, so a
  kernel-side fix does not reach them otherwise) and boots. User-space
  `println!` goes to the framebuffer, so the screendump is the output.
- **Advertised version means implemented.** `wl_seat` is advertised at 4, which
  is `wl_keyboard.repeat_info` and `wl_seat.name` but stops short of v5's
  `wl_pointer.frame`.

---

### Task 1 (8b.1): A seat, and a keyboard on it

**Files:**
- Create: `user/wm/src/seat.rs`
- Modify: `user/wm/src/protocol.rs`, `user/wm/src/client.rs`,
  `user/wm/src/objects.rs`, `user/wm/src/surface.rs`, `user/wm/src/main.rs`
- Modify: `explosion/toolchain/wlprobe.c`

**Interfaces:**
- Consumes: Tasks 3–5 of the Wayland MVP plan — the object table, the client
  write path, and `surface::window_of`.
- Produces: `seat::{focus_changed, key, KEYBOARD_VERSION}`;
  `surface::by_window(window) -> Option<usize>`;
  `client::Client::{keyboard_enter, keyboard_leave, keyboard_key}`.

`wl_keyboard.key` carries a Linux evdev keycode, and for the non-extended PS/2
set-1 scancodes the two are the same number — `KEY_ESC` is 1 and set-1 escape is
0x01, all the way to `KEY_F12` at 88 and 0x58. So there is no translation table,
which is worth a comment because it looks like a missing one.

- [ ] **Step 1: Write the failing test**

Extend `wlprobe` to bind the seat, take a keyboard, and report what arrives:

```c
static void kb_keymap(void *d, struct wl_keyboard *k, uint32_t format,
                      int32_t fd, uint32_t size) {
    (void)d; (void)k;
    printf("keymap: format %u size %u fd %d\n", format, size, fd);
    if (fd >= 0) close(fd);
}
static void kb_enter(void *d, struct wl_keyboard *k, uint32_t serial,
                     struct wl_surface *s, struct wl_array *keys) {
    (void)d; (void)k; (void)s; (void)keys;
    printf("enter: serial %u\n", serial);
}
static void kb_leave(void *d, struct wl_keyboard *k, uint32_t serial,
                     struct wl_surface *s) {
    (void)d; (void)k; (void)s;
    printf("leave: serial %u\n", serial);
}
static void kb_key(void *d, struct wl_keyboard *k, uint32_t serial,
                   uint32_t time, uint32_t key, uint32_t state) {
    (void)d; (void)k; (void)time;
    printf("key: serial %u code %u %s\n", serial, key,
           state ? "down" : "up");
}
static void kb_modifiers(void *d, struct wl_keyboard *k, uint32_t serial,
                         uint32_t dep, uint32_t lat, uint32_t lck,
                         uint32_t group) {
    (void)d; (void)k; (void)serial; (void)lat; (void)group;
    printf("mods: depressed %u locked %u\n", dep, lck);
}
static void kb_repeat(void *d, struct wl_keyboard *k, int32_t rate,
                      int32_t delay) {
    (void)d; (void)k;
    printf("repeat: %d/s after %dms\n", rate, delay);
}
static const struct wl_keyboard_listener kb_listener = {
    kb_keymap, kb_enter, kb_leave, kb_key, kb_modifiers, kb_repeat,
};
```

bound from the registry with

```c
    } else if (strcmp(iface, "wl_seat") == 0) {
        seat = wl_registry_bind(r, name, &wl_seat_interface, 4);
    }
```

and, after the first commit is configured,

```c
    keyboard = wl_seat_get_keyboard(seat);
    wl_keyboard_add_listener(keyboard, &kb_listener, NULL);
```

- [ ] **Step 2: Run it to verify it fails**

`$SP/cycle.sh $SP/t8b.keys /tmp/claude-1000/t8b.ppm` with a key script that
logs in, runs `wm wlprobe`, types `abc`, waits, presses Escape and screendumps.

Expected: `globals: 4` and no `wl_seat` among them; `seat: NULL`.

- [ ] **Step 3: Write the implementation**

`protocol.rs` gains the fifth global and the opcodes:

```rust
pub const SEAT: Interface = Interface { name: b"wl_seat", version: 4 };
pub const GLOBALS: [&Interface; 5] = [&COMPOSITOR, &SHM, &OUTPUT, &XDG_WM_BASE, &SEAT];

// wl_seat requests and events.
pub const SEAT_GET_POINTER: u16 = 0;
pub const SEAT_GET_KEYBOARD: u16 = 1;
pub const SEAT_GET_TOUCH: u16 = 2;
pub const SEAT_RELEASE: u16 = 3;
pub const SEAT_CAPABILITIES: u16 = 0;
pub const SEAT_NAME: u16 = 1;
/// `wl_seat.capability` bits.
pub const SEAT_CAP_POINTER: u32 = 1;
pub const SEAT_CAP_KEYBOARD: u32 = 2;

// wl_keyboard requests and events.
pub const KEYBOARD_RELEASE: u16 = 0;
pub const KEYBOARD_KEYMAP: u16 = 0;
pub const KEYBOARD_ENTER: u16 = 1;
pub const KEYBOARD_LEAVE: u16 = 2;
pub const KEYBOARD_KEY: u16 = 3;
pub const KEYBOARD_MODIFIERS: u16 = 4;
pub const KEYBOARD_REPEAT_INFO: u16 = 5;
/// `wl_keyboard.keymap_format`: the client uses a layout of its own.
pub const KEYMAP_FORMAT_NO_KEYMAP: u32 = 0;
pub const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// `wl_keyboard.key_state`.
pub const KEY_RELEASED: u32 = 0;
pub const KEY_PRESSED: u32 = 1;
```

`objects.rs` gains `Seat` and `Keyboard` kinds.

`surface.rs` gains the reverse lookup, because routing knows a window and the
protocol needs a surface:

```rust
/// The surface showing in a window, if any.
pub fn by_window(window: usize) -> Option<usize> {
    (0..MAX_SURFACES).find(|&i| unsafe { SURFACES[i].used && SURFACES[i].window == window })
}
```

`seat.rs` holds the focus and the modifier translation:

```rust
//! The seat: one keyboard, and later one pointer.
//!
//! Keyboard focus is separate from pointer focus in Wayland and this is only
//! the first of the two. What the compositor calls focus is a *window*; what
//! the protocol needs is a surface and the client that owns it, so the hop
//! from one to the other happens here rather than in the routing.

/// Which surface has keyboard focus, or `surface::NONE`.
static mut FOCUS: usize = surface::NONE;

/// The driver's modifier byte as an XKB modifier mask.
///
/// The indices are XKB's conventional ones -- Shift 0, Lock 1, Control 2,
/// Mod1 2**3 for Alt -- which is what every keymap derived from the standard
/// set uses, and therefore what a client will read them as.
pub fn xkb_mods(driver: u8) -> u32 {
    let mut m = 0;
    if driver & MOD_SHIFT != 0 { m |= 1 << 0; }
    if driver & MOD_CAPSLOCK != 0 { m |= 1 << 1; }
    if driver & MOD_CTRL != 0 { m |= 1 << 2; }
    if driver & MOD_ALT != 0 { m |= 1 << 3; }
    m
}
```

`client.rs` gains the three event senders and the two requests. `get_keyboard`
sends `keymap` first — with `KEYMAP_FORMAT_NO_KEYMAP` and a one-page empty
memory descriptor, because the event carries a descriptor whatever the format
says and a real keymap is Task 3 — then `repeat_info`, then `enter` if this
client's surface already has focus.

The descriptor rides on its own flush, which matters: the kernel queues a
descriptor ahead of the bytes of the write it was attached to, and libwayland
pops descriptors in message order. Flushing everything pending, queueing the
keymap alone and flushing that with the descriptor attached is what keeps the
two in step.

`main.rs` routes: `dispatch_key` also calls `seat::key(...)`, and
`cycle_focus`/`raise`/`destroy_window` call `seat::focus_changed(...)`.

- [ ] **Step 4: Run it to verify it passes**

Same invocation. Expected on screen:

```
globals: 5 ... seat:OK
keymap: format 0 size 0 fd 5
repeat: 25/s after 400ms
enter: serial N
key: serial N code 30 down
key: serial N code 30 up
```

`code 30` is `KEY_A`. Typing `abc` must produce 30, 48, 46 and their releases,
and Escape must produce none of them — the compositor's own bindings are never
passed on.

- [ ] **Step 5: Commit**

```bash
git add -A && git commit   # message written at the time
```

---

### Task 2 (8b.2): A grant the destination asked for

**Files:**
- Modify: `src/syscall.rs`, `src/ipc.rs`, `docs/abi.md`, `user/dtest/src/main.rs`

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `ipc::is_calling(target, dest) -> bool`.

`sys_cap_grant` checks that the caller holds what it is granting and that the
destination slot is empty, and nothing else — so any task may fill any other
task's sixteen CSpace slots and stop it ever being handed a display, a file or
an endpoint again. The roadmap deferred this until the rule could be written
from two real cases rather than one. Both exist:

- **A spawner granting to its child.** `init`, `login`, `shell`, `wm`,
  `manifest.rs`. The granter holds `TaskMgmt` over the destination.
- **A server answering a request.** `user/fb` mints a derived `PhysRange` and
  grants it into a claimant it never spawned — while that claimant is blocked
  in `sys_call` to it.

So the rule is: **`TaskMgmt` over the destination, or the destination is
currently blocked calling the granter.** The second needs no new system call and
no new state, because being blocked in `sys_call` *is* the invitation: a task
that calls a server has asked it for an answer, and a capability is one.

- [ ] **Step 1: Write the failing test**

In `user/dtest`, a case that does not need a second task, because the child in
`dtest` is spawned and would pass under the first half of the rule:

```rust
// A grant to somebody who neither asked nor is ours to manage.
// The nameserver is TID 2, is not this task's child, and is not calling us.
check(
    "cannot push a capability into an unrelated task",
    syscall::sys_cap_mint(SCRATCH, syscall::CAP_TYPE_ENDPOINT, 0, 0).is_ok()
        && syscall::sys_cap_grant(2, SCRATCH, 15).is_err(),
);
```

- [ ] **Step 2: Run it to verify it fails**

`$SP/boot.sh $SP/dt.keys /tmp/claude-1000/dt.ppm`. Expected: that one case
reported failed, everything else passing.

- [ ] **Step 3: Write the implementation**

In `src/ipc.rs`:

```rust
/// Is `tid` blocked in a `sys_call` to `dest`?
///
/// Which is to say: has it asked `dest` for something? A capability granted in
/// answer is not an imposition, and this is how the kernel can tell the two
/// apart without a system call whose only purpose is to say "I am expecting
/// one" -- which a task blocked in a call could not make anyway.
pub fn is_calling(tid: usize, dest: usize) -> bool {
    unsafe {
        match scheduler::get_task_mut(tid) {
            Some(t) => matches!(
                t.state_detail,
                TaskState::CallBlocked { to } | TaskState::CallSendBlocked { to } if to == dest
            ),
            None => false,
        }
    }
}
```

adapted to whatever the task structure actually records — read it rather than
assuming the shape above.

In the `SYS_CAP_GRANT` arm, before anything else:

```rust
let caller = scheduler::current_tid();
// A grant adds authority to somebody else's CSpace. It can never *raise*
// theirs -- a grant only ever adds -- but a CSpace has sixteen slots, and
// filling a service's slots is a denial of service: one that can no longer
// receive a capability can no longer be handed the display.
if !crate::cap::task_has_task_mgmt(caller, dest_tid)
    && !crate::ipc::is_calling(dest_tid, caller)
{
    return u64::MAX;
}
```

- [ ] **Step 4: Run it to verify it passes**

`dtest` green, and — because this rule is load-bearing in both directions —
`wm weston-simple-shm` must still draw. The framebuffer lease is the second case
and a regression here takes the display with it.

- [ ] **Step 5: Commit**

---

### Task 3 (8b.2): A keymap the client can read

**Files:**
- Create: `user/wm/src/keymap.rs`
- Modify: `user/wm/src/client.rs`, `explosion/toolchain/wlprobe.c`

**Interfaces:**
- Consumes: Task 1's `get_keyboard`.
- Produces: `keymap::XKB_V1: &[u8]`.

A `NO_KEYMAP` seat means every client guesses the layout. `XKB_V1` means the
compositor says it, once, in the format every client already parses.

The keymap text is generated once from the host and checked in, with the command
that produced it in the comment — hand-writing XKB is how you get a keymap that
xkbcommon rejects at run time, a long way from here.

- [ ] **Step 1: Write the failing test**

`wlprobe` reads the descriptor instead of closing it, and prints what it got:

```c
static void kb_keymap(void *d, struct wl_keyboard *k, uint32_t format,
                      int32_t fd, uint32_t size) {
    (void)d; (void)k;
    printf("keymap: format %u size %u\n", format, size);
    if (fd < 0) return;
    char *map = mmap(NULL, size, PROT_READ, MAP_PRIVATE, fd, 0);
    if (map == MAP_FAILED) {
        printf("keymap: mmap FAILED\n");
    } else {
        /* The first line, which says it is a keymap at all. */
        int n = 0;
        while (n < (int)size && map[n] != '\n') n++;
        printf("keymap: %.*s\n", n, map);
        munmap(map, size);
    }
    close(fd);
}
```

- [ ] **Step 2: Run it to verify it fails**

Expected: `keymap: format 0 size 0`, and no first line, because there is
nothing in the descriptor.

- [ ] **Step 3: Write the implementation**

Generate on the host:

```bash
xkbcli compile-keymap --layout us > /tmp/us.xkb   # or setxkbmap + xkbcomp
```

and embed it, then send it: a memory descriptor sized to the text, mapped,
written, sent with `KEYMAP_FORMAT_XKB_V1` and the byte count including the
terminating NUL, which is what xkbcommon expects.

The descriptor is made once and duplicated per client with `sys_fd_dup_self`,
because a client may close it and the next client still needs one.

- [ ] **Step 4: Run it to verify it passes**

Expected: `keymap: format 1 size N` and a first line of `xkb_keymap {`.

- [ ] **Step 5: Commit**

---

### Task 4 (8c.1): One controller, two devices

**Files:**
- Modify: `user/keyboard/src/main.rs`, `user/input/src/main.rs`,
  `user/wm/src/main.rs`, `user/wm/src/seat.rs`, `user/wm/src/client.rs`,
  `user/wm/src/protocol.rs`, `user/wm/src/objects.rs`
- Modify: `explosion/toolchain/wlprobe.c`

**Interfaces:**
- Consumes: Task 1's seat.
- Produces: `seat::{motion, button}`; `wl_pointer` at version 4.

The risk this sub-phase was split off for. A PS/2 mouse is not a second device
with a second port — it is the *same* i8042 controller, answering on the same
data port 0x60, with IRQ 12 instead of 1 and bit 5 of the status port saying
which device a byte came from. Two drivers holding port 0x60 race, and the
symptom of losing that race is a keyboard that types garbage or stops.

So there is one driver. `user/keyboard` takes IRQ 12 as well, enables the
auxiliary device through the controller, and routes each byte by status bit 5.
It keeps its name and its nameserver registration, because everything that talks
to it talks about keys; what changes is that it also answers a pointer poll.

- [ ] **Step 1: Write the failing test**

`wlprobe` takes a pointer from the seat and prints enter, motion and button.
QEMU's `-display none` still delivers mouse input over QMP `input-send-event`,
so the key script can move and click without a window to look at.

- [ ] **Step 2: Run it to verify it fails**

Expected: `capabilities` reports keyboard only, so `wl_seat_get_pointer` returns
an object that never hears anything.

- [ ] **Step 3: Write the implementation**

Controller first, protocol second, and verify the controller alone before adding
any protocol on top of it — a mouse that wedges the keyboard must not look like
a `wl_pointer` bug.

1. Enable the auxiliary port (`0xA8` to 0x64), turn on its interrupt in the
   controller configuration byte, and put the mouse in streaming mode.
2. On IRQ 12, read 0x60 and feed a three-byte packet decoder; on IRQ 1, the
   existing scancode path. Check status bit 5 in both, and route on it rather
   than on which interrupt arrived, because a byte can be waiting for the other
   device when either fires.
3. `user/input` gains a pointer poll beside the key poll; the compositor pumps
   both.
4. The compositor moves a cursor, repaints the two regions the move touched,
   and decides pointer focus by which window is under it — a second focus, kept
   apart from the keyboard's, as `docs/wayland.md` requires.
5. `wl_pointer` at version 4: enter, leave, motion, button, axis, and the
   `release` request. Version 5's `frame` is deliberately out.

- [ ] **Step 4: Run it to verify it passes**

Move and click over `weston-simple-shm`; expected: enter with a surface,
motion with coordinates in surface-local fixed point, button with a serial, and
the keyboard still typing throughout.

- [ ] **Step 5: Commit**

---

---

### Task 5 (8c.2): The loose ends

**Files:**
- Create: `user/wm/src/clipboard.rs`
- Modify: `user/wm/src/{protocol,objects,client,seat}.rs`
- Rename: `user/console` to `user/qtty`, `user/shell` to `user/qsh`
- Modify: `user/init/src/main.rs`, `Makefile`, `explosion/Makefile`,
  `explosion/toolchain/*`, `quark/CLAUDE.md`, the memory index

**Interfaces:**
- Consumes: Tasks 1 and 4 — the clipboard is per seat and follows keyboard
  focus, and decoration is per toplevel.
- Produces: `clipboard::{offer, take}`.

Three things that have each been deferred once and are cheap now.

**The clipboard.** `wl_data_device_manager`, `wl_data_device`, `wl_data_source`
and `wl_data_offer` at version 3. The shape is the reason it waited for
descriptor passing: a client does not hand the compositor the *data*, it hands
over a source and a list of MIME types, and a receiver passes a pipe descriptor
back through the compositor for the source to write into. The compositor never
sees the bytes, which is the point — copying a gigabyte between two clients
costs the compositor a descriptor and nothing else.

Selection follows keyboard focus: `wl_data_device.selection` goes to whichever
client has it, and a client that never has focus can never read the clipboard.
That is the rule that makes a clipboard on a compositor safe to have.

**`xdg-decoration`.** `zxdg_decoration_manager_v1` at version 1. The compositor
already draws a title bar, so the negotiation has exactly one answer —
`configure` with `server_side` — and the value is that a toolkit stops drawing
its own on top of it. There was no second party to negotiate with until now.

**The names.** `user/console` becomes `qtty` and `user/shell` becomes `qsh`.
`qterm` is reserved and not built: it is Quark's own Wayland terminal and there
is nothing to write it against until `weston-terminal` proves the protocol. The
rename touches the boot image (`init` loads by name), ExplOSion's staging, and
both `CLAUDE.md` files; a rename that leaves one of those behind produces a
machine that boots to no console at all.

- [ ] **Step 1: Write the failing test**

Two clients, because a clipboard with one participant proves nothing: extend
`wlprobe` with a `--copy <text>` mode that takes the selection and a `--paste`
mode that reads it, then run `wm wlprobe --copy hello` and, in the same session,
`wlprobe --paste`.

For decoration, `wlprobe` binds the manager, asks for a decoration on its
toplevel and prints the mode it is configured with.

- [ ] **Step 2: Run it to verify it fails**

Expected: no `wl_data_device_manager` and no `zxdg_decoration_manager_v1` among
the globals; `--paste` prints nothing.

- [ ] **Step 3: Write the implementation**

The renames first and on their own, because a boot failure from a rename must
not be mistaken for a clipboard bug. Then decoration, which is one event. Then
the clipboard.

- [ ] **Step 4: Run it to verify it passes**

Expected: `paste: hello`, `decoration: server_side`, and the machine still boots
to a console with a shell on it.

- [ ] **Step 5: Commit**

---

## Deliberately not in this plan

Touch, key repeat generated by the compositor rather than described to the
client, primary selection, and drag and drop — which shares `wl_data_device`
with the clipboard but needs pointer grabs and a drag icon surface, and is a
larger thing than the selection half. Pointer *constraints* and relative motion
are what a game would want and no client here is one.

## Acceptance

A client somebody else wrote receives keystrokes and pointer events from Quark,
with keyboard focus and pointer focus tracked separately; two clients pass text
through a clipboard the compositor never reads; no task can fill another's
CSpace; and the machine boots to `qtty` running `qsh`.
