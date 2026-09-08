#![no_std]
#![no_main]
#![allow(static_mut_refs)]

//! A display server.
//!
//! Before this, the console mapped the framebuffer and drew straight into it.
//! That works for exactly one program, which is why there was only ever one
//! thing on the screen: the console had the framebuffer, so nothing else could
//! have it.
//!
//! A display server is the answer to that. It is the only thing that maps the
//! framebuffer; everything else asks it for a window, gets a slab of shared
//! memory back, draws into that, and says when it has finished. The server
//! composites the results.
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
//! Windows are composited in stacking order on every commit, whole. That is
//! more work than tracking damage, and at these sizes it is a memcpy per
//! window per update — worth revisiting when a client updates faster than it
//! can be redrawn, and not before.

use quark_rt::font::FONT;
use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::{nameserver, println, syscall};

// No manifest. The display server maps the framebuffer and nothing else, and
// that grant cannot come from here: the address is whatever mode the
// bootloader set, which nothing knowable at build time can name. `init` mints
// it and sends the geometry along with TAG_FB_INIT. Shared memory needs no
// capability — it is charged to whoever creates it.

/// From `init`: where the framebuffer is and what shape it has.
const TAG_FB_INIT: u64 = 100;

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
/// Where window buffers are mapped, one region each.
const WIN_BASE: usize = 0x82_0000_0000;
/// Room per window: 1280x800x4 is 4 MiB, so eight of them is the ceiling on
/// what a client may ask for.
const WIN_STRIDE: usize = 0x40_0000;

struct Screen {
    fb: usize,
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
    let at = s.fb + y * s.pitch + x * bpp;
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

fn fill_rect(x: usize, y: usize, w: usize, h: usize, colour: u32) {
    for dy in 0..h {
        for dx in 0..w {
            put_pixel(x + dx, y + dy, colour);
        }
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
        let dst = s.fb + dst_y * s.pitch + ox * bpp;
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
    let backdrop = pack_colour(0x10, 0x14, 0x1C);
    let s = unsafe { &SCREEN };
    fill_rect(0, 0, s.width, s.height, backdrop);

    unsafe {
        for i in 0..STACK_LEN {
            draw_window(STACK[i]);
        }
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
    println!("[wm] Started.");

    // The framebuffer, from init. Nothing can be drawn before it arrives, so
    // this is a blocking wait rather than part of the main loop.
    let mut msg = Message::empty();
    loop {
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }
        if msg.tag == TAG_FB_INIT {
            break;
        }
        let _ = syscall::sys_reply(msg.sender, &error(0));
    }
    init_screen(&msg);
    let _ = syscall::sys_reply(msg.sender, &ok());

    if nameserver::register(b"wm").is_ok() {
        println!("[wm] Registered with nameserver.");
    }
    composite();

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
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

fn init_screen(msg: &Message) {
    // Same packing init sends the console, because it is the same message.
    let phys = msg.data[0] as usize;
    let w = (msg.data[1] >> 32) as usize;
    let h = (msg.data[1] & 0xFFFF_FFFF) as usize;
    let pitch = (msg.data[2] >> 32) as usize;
    let bpp = (msg.data[2] & 0xFF) as usize;

    let pages = (pitch * h + 4095) / 4096;
    if syscall::sys_map_phys(phys, FB_VADDR, pages).is_err() {
        println!("[wm] Could not map the framebuffer.");
        syscall::sys_exit_code(1);
    }

    unsafe {
        SCREEN = Screen {
            fb: FB_VADDR,
            pitch,
            width: w,
            height: h,
            bpp,
            r_pos: ((msg.data[3] >> 16) & 0xFF) as u8,
            g_pos: ((msg.data[3] >> 8) & 0xFF) as u8,
            b_pos: (msg.data[3] & 0xFF) as u8,
        };
    }
    println!("[wm] {}x{} at {} bpp.", w, h, bpp);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[wm] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
