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

use quark_rt::font::FONT;
use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::spawn::{self, Scratch};
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

/// Ask for a window. `data[0] = (width << 32) | height`, `data[1..]` the title.
///
/// Replies with `data[0] = window id`, `data[1] = shared memory handle`,
/// `data[2] = (stride << 32) | bytes per pixel`.
const TAG_WM_CREATE: u64 = 1;
/// This window's contents have changed: `data[0] = id`.
const TAG_WM_COMMIT: u64 = 2;
/// Put a window somewhere: `data[0] = id`, `data[1] = (x << 32) | y`.
const TAG_WM_MOVE: u64 = 3;
/// Raise a window and give it focus: `data[0] = id`.
const TAG_WM_FOCUS: u64 = 4;
/// Give a window back: `data[0] = id`.
///
/// A client has to say so. There is no notification when a task dies, so a
/// window whose owner simply exited stays on the screen until something else
/// needs the slot — which is a thing to fix with a death notification, not
/// with guesswork here.
const TAG_WM_DESTROY: u64 = 6;

/// How big is the screen, and how are its pixels laid out?
///
/// Replies `data[0] = (width << 32) | height` and
/// `data[1] = (red << 16) | (green << 8) | blue`, the bit position of each
/// channel. A client needs the second as much as the first: a window buffer is
/// copied to the screen verbatim, so it has to be in the screen's format.
const TAG_WM_SCREEN: u64 = 5;

const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

/// Height of a window's title bar, in pixels.
const TITLE_H: usize = GLYPH_H + 6;
/// Width of the frame drawn around a window.
const BORDER: usize = 2;

const MAX_WINDOWS: usize = 8;
const MAX_TITLE: usize = 32;

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

struct Screen {
    /// Where the finished frame is copied to: the framebuffer itself.
    fb: usize,
    /// Where it is drawn: memory nobody is looking at.
    back: usize,
    pitch: usize,
    width: usize,
    height: usize,
    bpp: usize,
    r_pos: u8,
    g_pos: u8,
    b_pos: u8,
}

static mut SCREEN: Screen = Screen {
    fb: 0,
    back: 0,
    pitch: 0,
    width: 0,
    height: 0,
    bpp: 32,
    r_pos: 16,
    g_pos: 8,
    b_pos: 0,
};

#[derive(Clone, Copy)]
struct Window {
    used: bool,
    owner: usize,
    shmem: usize,
    /// Where the client's pixels are mapped in *our* address space.
    buf: usize,
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    stride: usize,
    title: [u8; MAX_TITLE],
    title_len: usize,
}

const NO_WINDOW: Window = Window {
    used: false,
    owner: 0,
    shmem: 0,
    buf: 0,
    w: 0,
    h: 0,
    x: 0,
    y: 0,
    stride: 0,
    title: [0; MAX_TITLE],
    title_len: 0,
};

static mut WINDOWS: [Window; MAX_WINDOWS] = [NO_WINDOW; MAX_WINDOWS];
/// Bottom to top. A window's place here is its place on the screen.
static mut STACK: [usize; MAX_WINDOWS] = [usize::MAX; MAX_WINDOWS];
static mut STACK_LEN: usize = 0;
/// Which window input would go to, and which gets the lit title bar.
static mut FOCUS: usize = usize::MAX;
/// The framebuffer device that lent us the display.
static mut FB_TID: usize = 0;
/// The program this session is for. When it stops, so does this.
static mut SESSION: usize = 0;

fn pack_colour(r: u8, g: u8, b: u8) -> u32 {
    let s = unsafe { &SCREEN };
    ((r as u32) << s.r_pos) | ((g as u32) << s.g_pos) | ((b as u32) << s.b_pos)
}

fn put_pixel(x: usize, y: usize, colour: u32) {
    let s = unsafe { &SCREEN };
    if x >= s.width || y >= s.height {
        return;
    }
    let bpp = s.bpp / 8;
    let at = s.back + y * s.pitch + x * bpp;
    unsafe {
        if bpp == 4 {
            (at as *mut u32).write_volatile(colour);
        } else if bpp == 3 {
            let p = at as *mut u8;
            p.write_volatile(colour as u8);
            p.add(1).write_volatile((colour >> 8) as u8);
            p.add(2).write_volatile((colour >> 16) as u8);
        }
    }
}

/// Fill a rectangle in the back buffer.
///
/// Clipped once and then written a row at a time. Going through `put_pixel`
/// meant a bounds check and a volatile store per pixel, and the backdrop alone
/// is a million of them per frame — enough that a client animating at a dozen
/// frames a second could not keep up with itself.
fn fill_rect(x: usize, y: usize, w: usize, h: usize, colour: u32) {
    let s = unsafe { &SCREEN };
    if s.back == 0 || s.bpp != 32 {
        // Anything but 32-bit goes the slow way; nothing here produces it.
        for dy in 0..h {
            for dx in 0..w {
                put_pixel(x + dx, y + dy, colour);
            }
        }
        return;
    }

    let x1 = (x + w).min(s.width);
    let y1 = (y + h).min(s.height);
    if x >= x1 || y >= y1 {
        return;
    }

    for row in y..y1 {
        let start = s.back + row * s.pitch + x * 4;
        let pixels = unsafe { core::slice::from_raw_parts_mut(start as *mut u32, x1 - x) };
        pixels.fill(colour);
    }
}

fn draw_text(x: usize, y: usize, text: &[u8], colour: u32) {
    for (i, &ch) in text.iter().enumerate() {
        let glyph = &FONT[ch as usize];
        for (gy, &bits) in glyph.iter().enumerate() {
            for gx in 0..GLYPH_W {
                if (bits >> (7 - gx)) & 1 != 0 {
                    put_pixel(x + i * GLYPH_W + gx, y + gy, colour);
                }
            }
        }
    }
}

/// Total size of a window on screen, frame included.
fn framed_size(w: &Window) -> (usize, usize) {
    (w.w + BORDER * 2, w.h + TITLE_H + BORDER * 2)
}

fn draw_window(idx: usize) {
    let win = unsafe { WINDOWS[idx] };
    if !win.used {
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

    // The client's pixels, straight out of the memory it shares with us.
    let s = unsafe { &SCREEN };
    let bpp = s.bpp / 8;
    let ox = win.x + BORDER;
    let oy = win.y + TITLE_H + BORDER;
    for row in 0..win.h {
        let src = win.buf + row * win.stride;
        let dst_y = oy + row;
        if dst_y >= s.height {
            break;
        }
        let dst = s.back + dst_y * s.pitch + ox * bpp;
        let bytes = (win.w * bpp).min(s.pitch.saturating_sub(ox * bpp));
        unsafe {
            core::ptr::copy_nonoverlapping(src as *const u8, dst as *mut u8, bytes);
        }
    }
}

/// Redraw everything, bottom window first.
///
/// Whole windows rather than damaged regions: at one screenful it is a copy
/// per window, and tracking damage across a shared buffer the client writes
/// whenever it likes needs the client to say what changed. Worth doing when a
/// client updates faster than this can keep up.
fn composite() {
    let s = unsafe { &SCREEN };
    if s.fb == 0 || s.back == 0 {
        return; // the display is somebody else's at the moment
    }

    let backdrop = pack_colour(0x10, 0x14, 0x1C);
    fill_rect(0, 0, s.width, s.height, backdrop);

    unsafe {
        for i in 0..STACK_LEN {
            draw_window(STACK[i]);
        }
    }
    present();
}

/// Put the finished frame on the screen, in one pass.
///
/// Everything above drew into the back buffer. This is the only write to the
/// framebuffer, which is why the cleared backdrop is never what anybody sees.
fn present() {
    let s = unsafe { &SCREEN };
    unsafe {
        core::ptr::copy_nonoverlapping(
            s.back as *const u8,
            s.fb as *mut u8,
            s.pitch * s.height,
        );
    }
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
        return error(2);
    }

    let stride = w * (s.bpp / 8);
    let bytes = stride * h;
    let pages = (bytes + 4095) / 4096;
    if pages * 4096 > WIN_STRIDE {
        return error(3);
    }

    let Ok(shmem) = syscall::sys_shmem_create(pages) else {
        return error(4);
    };
    let buf = WIN_BASE + idx * WIN_STRIDE;
    if syscall::sys_shmem_map(shmem, buf).is_err() {
        let _ = syscall::sys_shmem_destroy(shmem);
        return error(5);
    }
    // The client cannot map what it has not been granted, and it is the whole
    // point that it can: this is the memory it draws into.
    if syscall::sys_shmem_grant(shmem, sender).is_err() {
        let _ = syscall::sys_shmem_unmap(shmem, buf);
        let _ = syscall::sys_shmem_destroy(shmem);
        return error(6);
    }
    unsafe { core::ptr::write_bytes(buf as *mut u8, 0, bytes) };

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
            w,
            h,
            x: 0,
            y: 0,
            stride,
            title,
            title_len,
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

/// Take a window off the screen and give its memory back.
fn destroy_window(idx: usize) {
    unsafe {
        let win = WINDOWS[idx];
        if !win.used {
            return;
        }
        let _ = syscall::sys_shmem_unmap(win.shmem, win.buf);
        let _ = syscall::sys_shmem_destroy(win.shmem);
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
    composite();

    // What this session is for. Without one there is nothing to composite and
    // nothing to wait for, so say so rather than sit on the display.
    let Some(program) = args::argv(1) else {
        println!("usage: wm <program>");
        quit();
    };
    let Some(session) = start_session(program) else {
        quit();
    };
    unsafe { SESSION = session };

    loop {
        // A timed receive rather than a blocking one, so the session ending is
        // noticed. Nothing else would wake this: a program that exits sends no
        // message, and a compositor sitting on the display for a session that
        // finished is a machine with no way back to its console.
        let mut msg = Message::empty();
        if syscall::sys_recv_timeout(TID_ANY, &mut msg, SESSION_POLL_TICKS).is_err() {
            if session_finished() {
                quit();
            }
            continue;
        }
        let sender = msg.sender;

        let reply = match msg.tag {
            TAG_WM_CREATE => handle_create(sender, &msg),
            TAG_WM_COMMIT => match window_of(msg.data[0] as usize, sender) {
                // Redraw everything: a window below this one may overlap it,
                // and only a full pass gets the stacking right.
                Some(_) => {
                    composite();
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
            TAG_WM_DESTROY => match window_of(msg.data[0] as usize, sender) {
                Some(i) => {
                    destroy_window(i);
                    composite();
                    ok()
                }
                None => error(1),
            },
            // The framebuffer device wants the display back for somebody
            // else. There is nowhere for a compositor to go without a screen,
            // so acknowledge and quit rather than linger invisibly.
            TAG_FB_LOST => {
                let _ = syscall::sys_reply(sender, &ok());
                unsafe {
                    SCREEN.fb = 0;
                    SCREEN.back = 0;
                }
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

/// How often to look at whether the session is still running, in PIT ticks.
/// A tenth of a second: unnoticeable to a person, and nothing to a machine
/// that is otherwise idle waiting for a client.
const SESSION_POLL_TICKS: u64 = 10;

/// Has the session program stopped?
///
/// Reaps it if so. A task that has exited but not been waited for stays in the
/// table as Dead, so asking about its state is the same question either way.
fn session_finished() -> bool {
    let session = unsafe { SESSION };
    if session == 0 {
        return false;
    }
    match syscall::sys_task_info(session) {
        Ok((state, _, _)) => state == 3, // Dead
        Err(()) => true,                 // gone entirely
    }
}

/// Start the program this session is for.
///
/// The compositor holds the display for as long as that program runs, and
/// gives it back when it stops — which is what `startx` does, and for the same
/// reason: something has to decide when the graphical session is over, and the
/// thing the user asked to run is the obvious candidate.
fn start_session(name: &[u8]) -> Option<usize> {
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

    let _ = spawn::set_args(&info, &[name], &SPAWN_SCRATCH);
    if info.start().is_err() {
        println!("wm: could not start that program");
        return None;
    }
    Some(info.tid)
}

/// Give the display back and stop.
fn quit() -> ! {
    let fb = unsafe { FB_TID };
    for i in 0..MAX_WINDOWS {
        if unsafe { WINDOWS[i].used } {
            destroy_window(i);
        }
    }
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
