#![no_std]
#![no_main]
#![allow(static_mut_refs)]

//! A compositor.
//!
//! Run it and it takes the display; quit and it gives the display back, and
//! the text console picks up where it left off. It is a *client* of the
//! framebuffer device the same way anything else is — it does not own the
//! screen, it borrows it — which is what makes it something a user installs
//! rather than something the system is built around.
//!
//! Its own clients ask it for a window, get a slab of shared memory back, draw
//! into that, and say when they have finished. It composites the results.
//!
//! ```text
//!     client                     wm                    screen
//!     ------                     --                    ------
//!     CREATE(w,h,title) ------>  make shmem
//!                       <------  handle + id
//!     map it, draw
//!     COMMIT(id) ------------->  composite ---------->  framebuffer
//! ```
//!
//! Keys come the other way. The compositor claims the keyboard from the input
//! server when it claims the display — whoever owns the screen owns the
//! keyboard, which is how switching between a graphical session and a text
//! console has always worked — and hands each key to the focused window's
//! queue. A client reads its own queue, because a compositor cannot send to a
//! program it spawned: originating IPC needs an Endpoint capability naming the
//! destination, and a task ID that did not exist at spawn time is not
//! something this can mint. Answering a caller needs no capability, so the
//! client asks.
//!
//! Shared memory rather than messages because a window is a megabyte and a
//! message is forty bytes: the point of a compositor is that a client's
//! drawing does not travel anywhere. The client writes pixels, the server
//! reads them, and the only thing that moves is a word saying "look now".
//!
//! Compositing happens into a buffer of its own, and only the finished frame
//! reaches the screen. That is not an optimisation — it is what stops the
//! flicker. Painting the backdrop and then the windows *onto the framebuffer*
//! means the cleared screen is briefly the visible one, once per frame; at a
//! cursor blink's two frames a second that reads as a steady flash rather than
//! as motion.
//!
//! Windows are composited in stacking order on every commit, whole. That is
//! more work than tracking damage, and at these sizes it is a memcpy per
//! window per update — worth revisiting when a client updates faster than it
//! can be redrawn, and not before. Damage tracking would make the blink cheap;
//! the back buffer is what makes it invisible.

use quark_rt::ipc::{Message, TAG_TASK_DIED, TID_ANY};
use quark_rt::spawn::{self, Scratch};
use quark_rt::wm as proto;
use quark_rt::{args, nameserver, println, syscall, vfs};

// The back buffer is ordinary memory, so this needs to allocate pages. The
// right to map the framebuffer is not asked for here: it is lent by the
// framebuffer device when the display is claimed, and taken away again when it
// is released.
// The back buffer is ordinary memory, and running a session means creating a
// task for it and giving that task pages. The right to map the framebuffer is
// not asked for here: it is lent by the framebuffer device when the display is
// claimed, and taken away again when it is released.
// 64 pages, not "unlimited": a capability may only be narrowed, and the shell
// that launches this holds 64. Asking for more than the spawner has is not
// refused loudly — the mint simply fails and the capability is absent, which
// then looks like an unrelated failure much later.
mod client;
mod draw;
mod objects;
mod protocol;
mod seat;
mod shell;
mod shm;
mod surface;

use draw::{draw_text, fill_rect, pack_colour, present, Rect, Screen, CLIP, GLYPH_H, SCREEN};

quark_rt::manifest!([
    quark_rt::manifest::CapReq::phys_alloc(64),
    quark_rt::manifest::CapReq::task_mgmt(0),
]);

/// Scratch addresses for staging a session program's pages into its new
/// address space. Each spawner needs its own; these are the compositor's.
static SPAWN_SCRATCH: Scratch =
    Scratch { elf: 0x8A_0000_0000, stack: 0x8B_0000_0000, args: 0x8C_0000_0000 };
/// Where a session program's image is read before it is loaded.
const FILE_BUF: usize = 0x8D_0000_0000;

const PAGE_SIZE: usize = 4096;

/// Talking to the framebuffer device.
const TAG_FB_CLAIM: u64 = 2;
const TAG_FB_RELEASE: u64 = 3;
/// Somebody else wants the display. Stop drawing and answer.
///
/// Well clear of this compositor's own protocol numbers: both arrive at the
/// same `sys_recv`, and 4 was already `TAG_WM_FOCUS`.
const TAG_FB_LOST: u64 = 0x100;

/// Talking to the input server, which arbitrates the keyboard the same way the
/// framebuffer device arbitrates the screen.
const TAG_INPUT_CLAIM: u64 = 0x200;
const TAG_INPUT_RELEASE: u64 = 0x201;
const TAG_INPUT_POLL: u64 = 0x202;
const TAG_INPUT_KEY: u64 = 0x203;

// The window protocol itself lives in `quark_rt::wm`, with the client half
// that speaks it. Two copies of a wire format drift; this one is the server.
use proto::{
    TAG_COMMIT as TAG_WM_COMMIT, TAG_CREATE as TAG_WM_CREATE, TAG_DESTROY as TAG_WM_DESTROY,
    TAG_ERROR, TAG_FOCUS as TAG_WM_FOCUS, TAG_MOVE as TAG_WM_MOVE, TAG_OK,
    TAG_POLL_EVENT as TAG_WM_POLL_EVENT, TAG_SCREEN as TAG_WM_SCREEN,
};


/// Height of a window's title bar, in pixels.
const TITLE_H: usize = GLYPH_H + 6;
/// Width of the frame drawn around a window.
const BORDER: usize = 2;

const MAX_WINDOWS: usize = 8;
const MAX_TITLE: usize = 32;
/// Keys held for a window that has not asked for them yet.
///
/// Deep enough for a burst of typing between two of a client's frames — every
/// key is two of these, since a release is an event too, and a client redrawing
/// itself can be a frame behind. When it fills, the oldest goes: a client that
/// has stopped reading should not be able to make the newest keystroke the one
/// that is lost.
const EVENT_QUEUE: usize = 64;

/// Where the framebuffer is mapped.
const FB_VADDR: usize = 0x81_0000_0000;
/// Where the frame is assembled before any of it is shown.
const BACK_VADDR: usize = 0x85_0000_0000;
/// The slot the framebuffer device grants the display into.
const FB_LEASE_SLOT: usize = 2;
/// Where window buffers are mapped, one region each.
const WIN_BASE: usize = 0x82_0000_0000;
/// Room per window: 1280x800x4 is 4 MiB, so eight of them is the ceiling on
/// what a client may ask for.
const WIN_STRIDE: usize = 0x40_0000;



#[derive(Clone, Copy)]
struct Window {
    used: bool,
    owner: usize,
    shmem: usize,
    /// Where the client's pixels are mapped in *our* address space.
    buf: usize,
    /// Whether `shmem` is a region this compositor made and must give back.
    ///
    /// A Wayland surface's pixels live in a pool the *client* made and this
    /// compositor merely mapped, and a window over one of those must not
    /// destroy it when it closes — the client may still be drawing there, into
    /// the buffer it is about to attach to a different surface.
    owns_buf: bool,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    stride: usize,
    title: [u8; MAX_TITLE],
    title_len: usize,
    /// Keys waiting to be collected, packed by [`pack_event`].
    events: [u32; EVENT_QUEUE],
    ev_head: usize,
    ev_len: usize,
}

const NO_WINDOW: Window = Window {
    used: false,
    owner: 0,
    shmem: 0,
    buf: 0,
    owns_buf: false,
    w: 0,
    h: 0,
    x: 0,
    y: 0,
    stride: 0,
    title: [0; MAX_TITLE],
    title_len: 0,
    events: [0; EVENT_QUEUE],
    ev_head: 0,
    ev_len: 0,
};

static mut WINDOWS: [Window; MAX_WINDOWS] = [NO_WINDOW; MAX_WINDOWS];
/// Bottom to top. A window's place here is its place on the screen.
static mut STACK: [usize; MAX_WINDOWS] = [usize::MAX; MAX_WINDOWS];
static mut STACK_LEN: usize = 0;
/// Which window input would go to, and which gets the lit title bar.
static mut FOCUS: usize = usize::MAX;
/// The framebuffer device that lent us the display.
static mut FB_TID: usize = 0;
/// The input server that lent us the keyboard. Zero if it would not.
static mut INPUT_TID: usize = 0;

/// How many programs one session may be. More than one so that focus is a
/// thing that can be observed: with a single window there is nowhere for a key
/// to go wrong.
const MAX_SESSION: usize = 4;
/// The programs this session is for. When the last of them stops, so does this.
static mut SESSION: [usize; MAX_SESSION] = [0; MAX_SESSION];

/// The clients connected to this compositor.
///
/// One per session program: `wm` makes a socketpair, keeps this end and hands
/// the other to the child as descriptor 3, then tells it where to look with
/// `WAYLAND_SOCKET`. That is the whole reason libwayland needs no patch — its
/// `wl_display_connect` checks that variable before it looks for a socket in a
/// filesystem this system does not have.
static mut CLIENTS: [client::Client; client::MAX_CLIENTS] =
    [client::NO_CLIENT; client::MAX_CLIENTS];
static mut SESSION_LEN: usize = 0;








/// Total size of a window on screen, frame included.
fn framed_size(w: &Window) -> (usize, usize) {
    (w.w + BORDER * 2, w.h + TITLE_H + BORDER * 2)
}

/// Where a window sits on the screen, frame included.
fn framed_rect(idx: usize) -> Rect {
    let win = unsafe { &WINDOWS[idx] };
    let (fw, fh) = framed_size(win);
    Rect { x0: win.x, y0: win.y, x1: win.x + fw, y1: win.y + fh }
}

fn draw_window(idx: usize) {
    let win = unsafe { WINDOWS[idx] };
    if !win.used {
        return;
    }
    // Nothing of this window is in the region being painted.
    if framed_rect(idx).clip_to(unsafe { &CLIP }).is_empty() {
        return;
    }
    let focused = unsafe { FOCUS } == idx;
    let (fw, fh) = framed_size(&win);

    let frame = if focused { pack_colour(0x40, 0x70, 0xC0) } else { pack_colour(0x30, 0x30, 0x38) };
    let title_bg = frame;
    let title_fg = if focused { pack_colour(0xFF, 0xFF, 0xFF) } else { pack_colour(0xA0, 0xA0, 0xA8) };

    // Frame and title bar in one fill, then the contents over the top of it.
    fill_rect(win.x, win.y, fw, TITLE_H + BORDER, title_bg);
    fill_rect(win.x, win.y + TITLE_H + BORDER, BORDER, win.h + BORDER, frame);
    fill_rect(win.x + BORDER + win.w, win.y + TITLE_H + BORDER, BORDER, win.h + BORDER, frame);
    fill_rect(win.x, win.y + fh - BORDER, fw, BORDER, frame);

    draw_text(win.x + BORDER + 3, win.y + 3, &win.title[..win.title_len], title_fg);

    // The client's pixels, straight out of the memory it shares with us —
    // only the rows and columns the region being painted actually covers.
    let s = unsafe { &SCREEN };
    let bpp = s.bpp / 8;
    let ox = win.x + BORDER;
    let oy = win.y + TITLE_H + BORDER;
    let clip = unsafe { CLIP };
    let x_from = clip.x0.saturating_sub(ox);
    let x_to = win.w.min(clip.x1.saturating_sub(ox));
    let y_from = clip.y0.saturating_sub(oy);
    let y_to = win.h.min(clip.y1.saturating_sub(oy));
    if x_from >= x_to || y_from >= y_to {
        return;
    }
    let bytes = (x_to - x_from) * bpp;
    for row in y_from..y_to {
        let dst_y = oy + row;
        if dst_y >= s.height {
            break;
        }
        let src = win.buf + row * win.stride + x_from * bpp;
        let dst = s.back + dst_y * s.pitch + (ox + x_from) * bpp;
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes);
        }
    }
}

/// Repaint one region: backdrop, then every window that reaches into it,
/// bottom first — then put that region, and only that region, on the screen.
///
/// Whole windows rather than damaged sub-regions of them. A client shares a
/// buffer it writes whenever it likes, so what changed inside a window is
/// something only the client could say; what this does know is *which* window
/// committed, and that is where nearly all of the saving is.
fn refresh(region: Rect) {
    // `draw` decides what part of the screen this is and lays the backdrop
    // down; what goes on top of it is the only part that knows about windows.
    let Some(region) = draw::begin_region(region) else {
        return;
    };
    unsafe {
        for i in 0..STACK_LEN {
            draw_window(STACK[i]);
        }
    }
    present(region);
}

/// Redraw the whole screen. For anything structural — a window appearing,
/// moving, being raised or going away — where what changed is not one window's
/// contents.
fn composite() {
    refresh(draw::screen_rect());
}

/// Redraw one window and whatever overlaps it. What a commit costs.
fn refresh_window(idx: usize) {
    refresh(framed_rect(idx));
}


fn raise(idx: usize) {
    unsafe {
        let mut at = None;
        for i in 0..STACK_LEN {
            if STACK[i] == idx {
                at = Some(i);
                break;
            }
        }
        let Some(pos) = at else { return };
        for i in pos..STACK_LEN - 1 {
            STACK[i] = STACK[i + 1];
        }
        STACK[STACK_LEN - 1] = idx;
        FOCUS = idx;
    }
    announce_focus();
}

/// Tell whoever has keyboard focus that it has it, and whoever had it that it
/// does not. Called wherever `FOCUS` moves — and, for a window being adopted,
/// once the surface behind it can be found from it.
pub fn announce_focus() {
    unsafe {
        let ptr = &raw mut CLIENTS;
        seat::focus_changed(&mut *ptr, FOCUS);
    }
}

/// End the session.
const KEY_ESCAPE: u8 = 0x1B;
/// Move focus to the next window.
const KEY_TAB: u8 = b'\t';

/// Move focus to the next window round.
///
/// Raising the bottom one is the whole of it: the stack is bottom to top, so
/// promoting the bottom rotates the order and lands focus somewhere new every
/// time until it comes back round.
fn cycle_focus() {
    unsafe {
        if STACK_LEN < 2 {
            return;
        }
        raise(STACK[0]);
    }
}

fn pack_event(press: bool, ascii: u8, scancode: u8, modifiers: u8) -> u32 {
    (if press { 1u32 << 24 } else { 0 })
        | ((modifiers as u32) << 16)
        | ((scancode as u32) << 8)
        | ascii as u32
}

/// Give a key to a window, dropping the oldest one it has not read if it is
/// behind.
fn push_event(idx: usize, ev: u32) {
    unsafe {
        let w = &mut WINDOWS[idx];
        if !w.used {
            return;
        }
        if w.ev_len == EVENT_QUEUE {
            w.ev_head = (w.ev_head + 1) % EVENT_QUEUE;
            w.ev_len -= 1;
        }
        let at = (w.ev_head + w.ev_len) % EVENT_QUEUE;
        w.events[at] = ev;
        w.ev_len += 1;
    }
}

fn pop_event(idx: usize) -> Option<u32> {
    unsafe {
        let w = &mut WINDOWS[idx];
        if w.ev_len == 0 {
            return None;
        }
        let ev = w.events[w.ev_head];
        w.ev_head = (w.ev_head + 1) % EVENT_QUEUE;
        w.ev_len -= 1;
        Some(ev)
    }
}

/// Take the keyboard, so that keys come here rather than to the shell that
/// launched this. Not fatal if it fails: a session with a screen and no
/// keyboard is still worth more than no session.
fn claim_input() {
    let Some(input) = nameserver::lookup_retry(b"input", 20) else {
        println!("wm: no input server; running without a keyboard");
        return;
    };
    let msg = Message { sender: 0, tag: TAG_INPUT_CLAIM, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(input, &msg, &mut reply).is_err() || reply.tag == TAG_ERROR {
        println!("wm: could not claim the keyboard");
        return;
    }
    unsafe { INPUT_TID = input };
}

fn release_input() {
    let input = unsafe { INPUT_TID };
    if input == 0 {
        return;
    }
    unsafe { INPUT_TID = 0 };
    let msg = Message { sender: 0, tag: TAG_INPUT_RELEASE, data: [0; 6] };
    let mut reply = Message::empty();
    let _ = syscall::sys_call(input, &msg, &mut reply);
}

/// Collect whatever has been typed since the last look and route it.
///
/// Bounded so that holding a key down cannot keep this from ever getting back
/// to compositing: what is left stays in the driver's ring and arrives next
/// time round.
fn pump_input() {
    let input = unsafe { INPUT_TID };
    if input == 0 {
        return;
    }
    for _ in 0..EVENT_QUEUE {
        let msg = Message { sender: 0, tag: TAG_INPUT_POLL, data: [0; 6] };
        let mut reply = Message::empty();
        // Timed, because asking the keyboard must not be able to stop the
        // screen. A plain call to a service that has stopped answering hangs
        // the compositor, and with it every client waiting on it — one wedged
        // server should cost the keyboard, not the display.
        match syscall::sys_call_timeout(input, &msg, &mut reply, INPUT_CALL_TICKS) {
            syscall::CallOutcome::Replied => {}
            syscall::CallOutcome::TimedOut => {
                println!("[wm] input server did not answer");
                return;
            }
            syscall::CallOutcome::Failed => return,
        }
        if reply.tag != TAG_INPUT_KEY {
            return;
        }
        dispatch_key(
            reply.data[0] != 0,
            reply.data[1] as u8,
            reply.data[2] as u8,
            reply.data[3] as u8,
        );
    }
}

/// Decide where a key goes.
///
/// The compositor's own bindings come first and are never passed on — a client
/// cannot be allowed to swallow the way out of the session. Their releases are
/// held back too: a client shown the release of a key it was never told was
/// pressed would be tracking a phantom. Everything else, releases included,
/// goes to the focused window and nowhere else.
fn dispatch_key(press: bool, ascii: u8, scancode: u8, modifiers: u8) {
    match ascii {
        KEY_ESCAPE => {
            if press {
                quit();
            }
            return;
        }
        KEY_TAB => {
            if press {
                cycle_focus();
                composite();
            }
            return;
        }
        _ => {}
    }

    let focus = unsafe { FOCUS };
    if focus == usize::MAX {
        return;
    }
    // Both, because a window is one or the other and the compositor does not
    // ask which: a Wayland client never reads the event queue, and a client of
    // the compositor's own IPC has no seat to hear from. Sending to both costs
    // a queue push nobody reads, and needs no flag that could be wrong.
    push_event(focus, pack_event(press, ascii, scancode, modifiers));
    unsafe {
        let ptr = &raw mut CLIENTS;
        seat::key(&mut *ptr, press, scancode, modifiers);
    }
}

/// Lay a new window out.
///
/// Cascaded from the top left, which is the least surprising thing to do with
/// no user to ask and no window to be relative to. A window bigger than the
/// screen is pinned to the corner rather than placed off it.
fn place(idx: usize) {
    let s = unsafe { &SCREEN };
    let win = unsafe { &WINDOWS[idx] };
    let (fw, fh) = framed_size(win);

    let step = TITLE_H + BORDER * 2;
    let n = unsafe { STACK_LEN };
    let mut x = 8 + n * step;
    let mut y = 8 + n * step;
    if x + fw > s.width {
        x = if s.width > fw { s.width - fw } else { 0 };
    }
    if y + fh > s.height {
        y = if s.height > fh { s.height - fh } else { 0 };
    }
    unsafe {
        WINDOWS[idx].x = x;
        WINDOWS[idx].y = y;
    }
}

fn window_of(id: usize, sender: usize) -> Option<usize> {
    if id >= MAX_WINDOWS {
        return None;
    }
    let w = unsafe { &WINDOWS[id] };
    // A window belongs to whoever asked for it. Without this, any task could
    // move or raise another's.
    if w.used && w.owner == sender { Some(id) } else { None }
}

fn handle_create(sender: usize, msg: &Message) -> Message {
    let w = (msg.data[0] >> 32) as usize;
    let h = (msg.data[0] & 0xFFFF_FFFF) as usize;
    let s = unsafe { &SCREEN };

    if w == 0 || h == 0 || w > s.width || h > s.height {
        println!("[wm] refused a window for tid {}: bad size", sender);
        return error(1);
    }

    let mut idx = MAX_WINDOWS;
    for i in 0..MAX_WINDOWS {
        if !unsafe { WINDOWS[i].used } {
            idx = i;
            break;
        }
    }
    if idx == MAX_WINDOWS {
        println!("[wm] refused a window for tid {}: no free window", sender);
        return error(2);
    }

    let stride = w * (s.bpp / 8);
    let bytes = stride * h;
    let pages = (bytes + 4095) / 4096;
    if pages * 4096 > WIN_STRIDE {
        println!("[wm] refused a window for tid {}: too big", sender);
        return error(3);
    }

    let Ok(shmem) = syscall::sys_shmem_create(pages) else {
        println!("[wm] refused a window for tid {}: no shared memory", sender);
        return error(4);
    };
    let buf = WIN_BASE + idx * WIN_STRIDE;
    if syscall::sys_shmem_map(shmem, buf).is_err() {
        let _ = syscall::sys_shmem_destroy(shmem);
        println!("[wm] refused a window for tid {}: could not map it", sender);
        return error(5);
    }
    // The client cannot map what it has not been granted, and it is the whole
    // point that it can: this is the memory it draws into.
    if syscall::sys_shmem_grant(shmem, sender).is_err() {
        let _ = syscall::sys_shmem_unmap(shmem, buf);
        let _ = syscall::sys_shmem_destroy(shmem);
        println!("[wm] refused a window for tid {}: could not share it", sender);
        return error(6);
    }
    unsafe { core::ptr::write_bytes(buf as *mut u8, 0, bytes) };

    // A window would otherwise outlive its owner: nothing else says the memory
    // can go back, and the picture stays on the screen.
    let _ = syscall::sys_task_watch(sender);

    let mut title = [0u8; MAX_TITLE];
    let mut title_len = 0;
    // The title rides in the remaining five data words, as bytes.
    'outer: for word in 1..6 {
        for b in msg.data[word].to_le_bytes() {
            if b == 0 || title_len == MAX_TITLE {
                break 'outer;
            }
            title[title_len] = b;
            title_len += 1;
        }
    }

    unsafe {
        WINDOWS[idx] = Window {
            used: true,
            owner: sender,
            shmem,
            buf,
            owns_buf: true,
            w,
            h,
            x: 0,
            y: 0,
            stride,
            title,
            title_len,
            ..NO_WINDOW
        };
        STACK[STACK_LEN] = idx;
        STACK_LEN += 1;
        FOCUS = idx;
    }
    place(idx);
    composite();

    println!("[wm] window {} for tid {} ({}x{})", idx, sender, w, h);

    Message {
        sender: 0,
        tag: TAG_OK,
        data: [
            idx as u64,
            shmem as u64,
            ((stride as u64) << 32) | (s.bpp as u64),
            0,
            0,
            0,
        ],
    }
}

/// Put a window on the screen over memory this compositor does not own.
///
/// This is what a Wayland surface's first commit does. Everything after the
/// buffer is the same as an ordinary window — a frame, a title bar, a place in
/// the stack and the focus — because from the screen's point of view it is one.
pub fn adopt_window(owner: usize, buf: usize, w: usize, h: usize, stride: usize) -> Option<usize> {
    let s = unsafe { &SCREEN };
    if w == 0 || h == 0 || w > s.width || h > s.height {
        return None;
    }
    let idx = unsafe { WINDOWS.iter().position(|win| !win.used) }?;
    // A window would otherwise outlive its owner: nothing else says the
    // picture can come off the screen.
    let _ = syscall::sys_task_watch(owner);
    unsafe {
        WINDOWS[idx] = Window {
            used: true,
            owner,
            shmem: 0,
            buf,
            owns_buf: false,
            w,
            h,
            stride,
            ..NO_WINDOW
        };
        STACK[STACK_LEN] = idx;
        STACK_LEN += 1;
        FOCUS = idx;
    }
    place(idx);
    composite();
    // No focus announcement here, though this is where focus moves: the
    // surface does not know its window until `surface::commit` records it, so
    // the announcement waits for that and happens there.
    Some(idx)
}

/// Show different pixels in a window that already exists.
///
/// A resize means the frame moved, so the whole screen is repainted; a buffer
/// of the same size means only this window changed, which is the case a client
/// hits sixty times a second.
pub fn set_window_buffer(idx: usize, buf: usize, w: usize, h: usize, stride: usize) {
    let resized = unsafe { WINDOWS[idx].w != w || WINDOWS[idx].h != h };
    unsafe {
        let win = &mut WINDOWS[idx];
        if !win.used {
            return;
        }
        win.buf = buf;
        win.w = w;
        win.h = h;
        win.stride = stride;
    }
    if resized {
        composite();
    }
}

/// Give a window a title. A toplevel's title arrives after the window does.
pub fn set_window_title(idx: usize, title: &[u8]) {
    unsafe {
        let win = &mut WINDOWS[idx];
        if !win.used {
            return;
        }
        win.title_len = title.len().min(MAX_TITLE);
        win.title[..win.title_len].copy_from_slice(&title[..win.title_len]);
    }
    refresh_window(idx);
}

/// The screen, as a client is told about it.
pub fn screen_size() -> (usize, usize) {
    let s = unsafe { &SCREEN };
    (s.width, s.height)
}

/// Milliseconds since boot, which is what a frame callback carries.
///
/// A client uses the difference between two of them to decide how far to move
/// something, so what matters is that it advances at the right rate, not what
/// it counts from. The PIT is 100 Hz, so this is accurate to ten milliseconds.
pub fn now_ms() -> u32 {
    (syscall::sys_ticks() * 10) as u32
}

/// What a client should be told to be: the size the frame leaves for it.
pub fn suggested_size() -> (usize, usize) {
    let s = unsafe { &SCREEN };
    let w = s.width.saturating_sub(BORDER * 2 + 16).min(640);
    let h = s.height.saturating_sub(TITLE_H + BORDER * 2 + 16).min(480);
    (w, h)
}

/// Take a window off the screen and give its memory back.
fn destroy_window(idx: usize) {
    unsafe {
        let win = WINDOWS[idx];
        if !win.used {
            return;
        }
        if win.owns_buf {
            let _ = syscall::sys_shmem_unmap(win.shmem, win.buf);
            let _ = syscall::sys_shmem_destroy(win.shmem);
        }
        WINDOWS[idx] = NO_WINDOW;

        let mut out = 0;
        for i in 0..STACK_LEN {
            if STACK[i] != idx {
                STACK[out] = STACK[i];
                out += 1;
            }
        }
        STACK_LEN = out;
        // Focus follows the window that is now on top, which is the least
        // surprising place for it to go.
        FOCUS = if STACK_LEN > 0 { STACK[STACK_LEN - 1] } else { usize::MAX };
    }
    announce_focus();
}

fn error(code: u64) -> Message {
    Message { sender: 0, tag: TAG_ERROR, data: [code, 0, 0, 0, 0, 0] }
}

fn ok() -> Message {
    Message { sender: 0, tag: TAG_OK, data: [0; 6] }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(fb) = nameserver::lookup_retry(b"fb", 20) else {
        println!("wm: no framebuffer device");
        syscall::sys_exit_code(1);
    };
    unsafe { FB_TID = fb };

    // Take the display. Whoever had it — the text console, on a fresh boot —
    // is told to stop before this returns.
    let claim = Message { sender: 0, tag: TAG_FB_CLAIM, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(fb, &claim, &mut reply).is_err() || reply.tag == TAG_ERROR {
        println!("wm: could not claim the display");
        syscall::sys_exit_code(1);
    }
    if !init_screen(&reply) {
        syscall::sys_exit_code(1);
    }

    let _ = nameserver::register(b"wm");
    // The keyboard follows the screen. Claimed after the display, so that a
    // failure here leaves a compositor that draws rather than one that has
    // taken the keyboard away from a console it never displaced.
    claim_input();
    composite();

    // What this session is for. Without one there is nothing to composite and
    // nothing to wait for, so say so rather than sit on the display.
    if args::argv(1).is_none() {
        println!("usage: wm <program> [program...]");
        quit();
    }
    let mut n = 0;
    while n < MAX_SESSION {
        let Some(program) = args::argv(1 + n) else { break };
        let Some(tid) = start_session(program, n) else {
            quit();
        };
        unsafe {
            SESSION[n] = tid;
            SESSION_LEN += 1;
        }
        // Watched from the start rather than from its first window: a program
        // that dies before it draws anything still ends the session.
        let _ = syscall::sys_task_watch(tid);
        n += 1;
    }

    let mut last_pump: u64 = 0;
    let mut last_check: u64 = 0;
    loop {
        // Two things happen that nobody sends a message about. Keys are one:
        // the input server cannot send here uninvited, so the keyboard has to
        // be asked. The session ending is the other — a program that exits
        // says nothing, and a compositor sitting on the display for a session
        // that finished is a machine with no way back to its console.
        //
        // Both are done on the clock rather than off the receive timing out.
        // A session whose clients poll for their own events keeps this loop
        // busy, and hanging the keyboard off an idle moment would mean it went
        // unread for exactly as long as anything was happening.
        let now = syscall::sys_ticks();
        if now != last_pump {
            last_pump = now;
            pump_input();
        }

        // A client's requests arrive on a stream rather than as IPC, and the
        // kernel has no single wait that covers both. Until it does, this asks
        // each connection whether it has anything, which costs one system call
        // per client per tick and is the same shape the keyboard already has.
        serve_clients();
        if now.wrapping_sub(last_check) >= SESSION_CHECK_TICKS {
            last_check = now;
            if session_finished() {
                quit();
            }
        }

        // A timed receive rather than a blocking one, so that the two above
        // still happen on a screen nothing is drawing to.
        let mut msg = Message::empty();
        if syscall::sys_recv_timeout(TID_ANY, &mut msg, POLL_TICKS).is_err() {
            continue;
        }
        let sender = msg.sender;

        let reply = match msg.tag {
            TAG_WM_CREATE => handle_create(sender, &msg),
            TAG_WM_COMMIT => match window_of(msg.data[0] as usize, sender) {
                // Just this window's patch of screen. Windows above it are
                // redrawn within that patch, so the stacking still comes out
                // right without touching the rest of the display.
                Some(i) => {
                    refresh_window(i);
                    ok()
                }
                None => error(1),
            },
            TAG_WM_MOVE => match window_of(msg.data[0] as usize, sender) {
                Some(i) => {
                    unsafe {
                        WINDOWS[i].x = (msg.data[1] >> 32) as usize;
                        WINDOWS[i].y = (msg.data[1] & 0xFFFF_FFFF) as usize;
                    }
                    composite();
                    ok()
                }
                None => error(1),
            },
            TAG_WM_FOCUS => match window_of(msg.data[0] as usize, sender) {
                Some(i) => {
                    raise(i);
                    composite();
                    ok()
                }
                None => error(1),
            },
            TAG_WM_POLL_EVENT => match window_of(msg.data[0] as usize, sender) {
                Some(i) => {
                    let focused = if unsafe { FOCUS } == i { 1 } else { 0 };
                    match pop_event(i) {
                        Some(ev) => Message {
                            sender: 0,
                            tag: TAG_OK,
                            data: [
                                1,
                                (ev & 0xFF) as u64,
                                ((ev >> 8) & 0xFF) as u64,
                                ((ev >> 16) & 0xFF) as u64,
                                ((ev >> 24) & 1) as u64,
                                focused,
                            ],
                        },
                        None => Message {
                            sender: 0,
                            tag: TAG_OK,
                            data: [0, 0, 0, 0, 0, focused],
                        },
                    }
                }
                None => error(1),
            },
            TAG_WM_DESTROY => match window_of(msg.data[0] as usize, sender) {
                Some(i) => {
                    destroy_window(i);
                    composite();
                    ok()
                }
                None => error(1),
            },
            // A client has gone. Its windows go with it, and the session ends
            // when the last of its programs has stopped.
            TAG_TASK_DIED => {
                let dead = msg.data[0] as usize;
                for i in 0..MAX_WINDOWS {
                    if unsafe { WINDOWS[i].used && WINDOWS[i].owner == dead } {
                        destroy_window(i);
                    }
                }
                let mut was_session = false;
                unsafe {
                    let mut out = 0;
                    for i in 0..SESSION_LEN {
                        if SESSION[i] == dead {
                            was_session = true;
                        } else {
                            SESSION[out] = SESSION[i];
                            out += 1;
                        }
                    }
                    SESSION_LEN = out;
                    if SESSION_LEN == 0 {
                        quit();
                    }
                }
                if was_session {
                    // Collect it. A task this one started keeps its slot and
                    // its address space until its parent asks, and there is a
                    // dead child waiting right now — so this answers at once
                    // rather than blocking.
                    let _ = syscall::sys_wait();
                }
                composite();
                continue; // the kernel is not waiting for a reply
            }

            // The framebuffer device wants the display back for somebody
            // else. There is nowhere for a compositor to go without a screen,
            // so acknowledge and quit rather than linger invisibly.
            TAG_FB_LOST => {
                let _ = syscall::sys_reply(sender, &ok());
                unsafe {
                    SCREEN.fb = 0;
                    SCREEN.back = 0;
                }
                // The keyboard came with the screen and goes back with it.
                release_input();
                println!("wm: display taken; exiting");
                syscall::sys_exit_code(0);
            }
            TAG_WM_SCREEN => {
                let s = unsafe { &SCREEN };
                Message {
                    sender: 0,
                    tag: TAG_OK,
                    data: [
                        ((s.width as u64) << 32) | (s.height as u64),
                        ((s.r_pos as u64) << 16) | ((s.g_pos as u64) << 8) | (s.b_pos as u64),
                        0,
                        0,
                        0,
                        0,
                    ],
                }
            }
            _ => error(0),
        };

        let _ = syscall::sys_reply(sender, &reply);
    }
}

/// Map the framebuffer the device just lent us, and a back buffer beside it.
fn init_screen(reply: &Message) -> bool {
    let w = (reply.data[0] >> 32) as usize;
    let h = (reply.data[0] & 0xFFFF_FFFF) as usize;
    let pitch = (reply.data[1] >> 32) as usize;
    let bpp = (reply.data[1] & 0xFF) as usize;
    let phys = reply.data[3] as usize;

    let pages = (pitch * h + 4095) / 4096;
    if syscall::sys_map_phys(phys, FB_VADDR, pages).is_err() {
        println!("wm: could not map the framebuffer");
        return false;
    }
    // Ordinary memory, the same shape as the screen. sys_mmap needs no
    // capability — a task may always grow its own address space — but it maps
    // at most 256 pages a call, and a screenful is four times that.
    const MMAP_MAX: usize = 256;
    let mut done = 0;
    while done < pages {
        let chunk = (pages - done).min(MMAP_MAX);
        if syscall::sys_mmap(BACK_VADDR + done * 4096, chunk).is_err() {
            println!("wm: could not allocate a back buffer");
            return false;
        }
        done += chunk;
    }

    unsafe {
        SCREEN = Screen {
            fb: FB_VADDR,
            back: BACK_VADDR,
            pitch,
            width: w,
            height: h,
            bpp,
            r_pos: ((reply.data[2] >> 16) & 0xFF) as u8,
            g_pos: ((reply.data[2] >> 8) & 0xFF) as u8,
            b_pos: (reply.data[2] & 0xFF) as u8,
        };
    }
    true
}

/// How long to wait for a message before looking around, in PIT ticks.
///
/// One, which is ten milliseconds, because this is also how often the keyboard
/// is asked and typing at a tenth of a second is typing through treacle.
const POLL_TICKS: u64 = 1;
/// How long to wait for the input server before giving up on this round.
const INPUT_CALL_TICKS: u64 = 20;
/// How long between checks on whether the session is over, in the same ticks.
///
/// A backstop, not the mechanism: the kernel says when a session program dies
/// and that is what normally ends things. This stays because a compositor that
/// never gives the screen back leaves a machine with no way out, and one
/// dropped notification should not be able to cause that. Two seconds, since
/// nothing is waiting on the answer.
const SESSION_CHECK_TICKS: u64 = 200;

/// Have all the session's programs stopped?
///
/// A task that has exited but not been waited for stays in the table as Dead,
/// so asking about its state is the same question either way. The session
/// lasts as long as its last program: closing one window of two is not a
/// reason to take the screen away from the other.
fn session_finished() -> bool {
    unsafe {
        if SESSION_LEN == 0 {
            return false;
        }
        for i in 0..SESSION_LEN {
            let alive = match syscall::sys_task_info(SESSION[i]) {
                Ok((state, _, _)) => state != 3, // 3 is Dead
                Err(()) => false,                // gone entirely
            };
            if alive {
                return false;
            }
        }
        true
    }
}

/// Start one of the programs this session is for.
///
/// The compositor holds the display for as long as those programs run, and
/// gives it back when the last of them stops — which is what `startx` does,
/// and for the same reason: something has to decide when the graphical session
/// is over, and the thing the user asked to run is the obvious candidate.
fn start_session(name: &[u8], index: usize) -> Option<usize> {
    let vfs_tid = nameserver::lookup_retry(b"vfs", 20)?;

    // The same two spellings the shell tries: lowercase for ext2, uppercase
    // with .ELF for FAT32.
    let mut lower = [0u8; 64];
    let mut upper = [0u8; 64];
    let prefix = b"/usr/bin/";
    let n = name.len().min(48);
    lower[..prefix.len()].copy_from_slice(prefix);
    upper[..prefix.len()].copy_from_slice(prefix);
    let mut lp = prefix.len();
    let mut up = prefix.len();
    for i in 0..n {
        let c = name[i];
        lower[lp] = if c.is_ascii_uppercase() { c + 32 } else { c };
        upper[up] = if c.is_ascii_lowercase() { c - 32 } else { c };
        lp += 1;
        up += 1;
    }
    upper[up..up + 4].copy_from_slice(b".ELF");
    up += 4;

    let (handle, size, _) = match vfs::open(vfs_tid, &lower[..lp]) {
        Ok(h) => h,
        Err(_) => match vfs::open(vfs_tid, &upper[..up]) {
            Ok(h) => h,
            Err(_) => {
                println!("wm: cannot find that program");
                return None;
            }
        },
    };

    let size = size as usize;
    let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
    let mut ok = true;
    for p in 0..pages {
        let Ok(frame) = syscall::sys_phys_alloc(1) else { ok = false; break };
        if syscall::sys_map_phys(frame, FILE_BUF + p * PAGE_SIZE, 1).is_err() {
            ok = false;
            break;
        }
        let want = PAGE_SIZE.min(size - p * PAGE_SIZE) as u32;
        if vfs::read(vfs_tid, handle, frame, (p * PAGE_SIZE) as u32, want).is_err() {
            ok = false;
            break;
        }
    }
    let _ = vfs::close(vfs_tid, handle);
    if !ok {
        println!("wm: could not read that program");
        return None;
    }

    let image = unsafe { core::slice::from_raw_parts(FILE_BUF as *const u8, size) };
    let Ok(info) = spawn::load(image, &SPAWN_SCRATCH) else {
        println!("wm: could not load that program");
        return None;
    };
    // A client needs no authority over anything — the memory it draws into is
    // memory this hands it — but it does need to be able to *ask*. Two grants:
    // this compositor's own IPC reach, so it can find the nameserver, and
    // permission to call this compositor, which nothing else can give it.
    // A task's own destination bit is the one an Endpoint may always add.
    let _ = syscall::sys_cap_grant(info.tid, syscall::SLOT_ENDPOINT, syscall::SLOT_ENDPOINT);
    let me = syscall::sys_getpid() as u64;
    let slot = syscall::SLOT_ENDPOINT_EXTRA;
    if syscall::sys_cap_mint(slot, syscall::CAP_TYPE_ENDPOINT, 1u64 << me, 0).is_ok() {
        let _ = syscall::sys_cap_grant(info.tid, slot, slot);
        let _ = syscall::sys_cap_delete(slot);
    }

    // Where a client's diagnostics go. Not stdin: a session program takes its
    // keys from this compositor, and a read on the input server would only
    // block until the display went back to the console anyway.
    let _ = syscall::sys_fd_dup(info.tid, 1, 1);
    let _ = syscall::sys_fd_dup(info.tid, 2, 2);

    // argv[1] is which of the session's programs this one is. Two copies of
    // the same program are otherwise indistinguishable on screen, and telling
    // which window has focus is the entire point of having two.
    let tag = [b'1' + (index % 9) as u8];

    // A Wayland connection, if there is room for one. The child gets its end
    // at descriptor 3 and is told so; we keep ours and close our copy of its,
    // which is safe because an end is reference counted — the peer is not told
    // the connection has gone just because we let go of its half.
    let mut env: [&[u8]; 1] = [b""];
    let mut env_len = 0;
    if let Some(slot) = unsafe { CLIENTS.iter().position(|c| !c.used) } {
        if let Ok((mine, theirs)) = syscall::sys_socketpair() {
            if syscall::sys_fd_dup(info.tid, WAYLAND_FD, theirs).is_ok() {
                let _ = syscall::sys_fd_close(theirs);
                unsafe { CLIENTS[slot].open(slot, mine, info.tid) };
                env[0] = b"WAYLAND_SOCKET=3";
                env_len = 1;
            } else {
                let _ = syscall::sys_fd_close(mine);
                let _ = syscall::sys_fd_close(theirs);
            }
        }
    }

    let _ = spawn::set_args_env(&info, &[name, &tag], &env[..env_len], &SPAWN_SCRATCH);
    if info.start().is_err() {
        println!("wm: could not start that program");
        return None;
    }
    Some(info.tid)
}

/// Where a client finds its end of the connection.
const WAYLAND_FD: usize = 3;

/// Let every connected client speak, and drop the ones that have stopped.
fn serve_clients() {
    let now = syscall::sys_ticks();
    unsafe {
        for i in 0..client::MAX_CLIENTS {
            if !CLIENTS[i].used {
                continue;
            }
            // Before reading: a client waiting on a frame has nothing to say
            // until it gets one, so answering first is what gives it something
            // to send on this pass rather than the next.
            CLIENTS[i].fire_frames(now);
            let mut fds = [syscall::PollFd::new(CLIENTS[i].fd, syscall::POLL_READABLE)];
            // Zero timeout: this is a question, not a wait. The waiting is
            // done by the main loop's timed receive, which also hears about
            // windows and input; a wait here would stop it hearing either.
            if syscall::sys_poll(&mut fds, 0) != Ok(1) {
                continue;
            }
            if fds[0].revents & (syscall::POLL_READABLE | syscall::POLL_HANGUP) == 0 {
                continue;
            }
            if !CLIENTS[i].dispatch() {
                CLIENTS[i].close();
            }
        }
    }
}

/// Give the display back and stop.
fn quit() -> ! {
    let fb = unsafe { FB_TID };

    // Take the session down first. A client outliving its compositor is a task
    // calling a dead TID for windows it can no longer draw, and — because TIDs
    // are recycled — eventually calling whatever lands in that slot next.
    unsafe {
        for i in 0..SESSION_LEN {
            let _ = syscall::sys_task_kill(SESSION[i]);
        }
        SESSION_LEN = 0;
    }

    unsafe {
        for i in 0..client::MAX_CLIENTS {
            CLIENTS[i].close();
        }
    }
    for i in 0..MAX_WINDOWS {
        if unsafe { WINDOWS[i].used } {
            destroy_window(i);
        }
    }
    release_input();
    if fb != 0 {
        // Hand the capability back with the display: a slot must be empty to
        // be granted into, so leaving it filled would stop the next claimant
        // ever being given it.
        let _ = syscall::sys_cap_delete(FB_LEASE_SLOT);
        let msg = Message { sender: 0, tag: TAG_FB_RELEASE, data: [0; 6] };
        let mut reply = Message::empty();
        let _ = syscall::sys_call(fb, &msg, &mut reply);
    }
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[wm] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
