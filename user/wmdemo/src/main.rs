#![no_std]
#![no_main]
#![allow(static_mut_refs)]

//! A window you can type into.
//!
//! There is not much to a display server client: ask for a window, map the
//! memory it gives you, write pixels into it, say when you have finished, and
//! ask what has happened to you. The last of those is the interesting one when
//! two of these are running — only one window has focus, so only one of them
//! sees what is typed, and which one that is changes with Tab.
//!
//! Events are pulled rather than pushed. The compositor cannot send to a
//! program it spawned: originating IPC needs an Endpoint capability naming the
//! destination, and a task ID that did not exist when the compositor started
//! is not one it can mint. Answering a caller needs no capability, so the
//! client does the asking — which is the same shape as `XNextEvent` anyway.

use quark_rt::font::FONT;
use quark_rt::ipc::Message;
use quark_rt::{args, nameserver, println, syscall};

// Nothing to declare: the window's memory belongs to the display server, and
// this only maps what it is given.

const TAG_WM_CREATE: u64 = 1;
const TAG_WM_COMMIT: u64 = 2;
const TAG_WM_DESTROY: u64 = 6;
const TAG_WM_POLL_EVENT: u64 = 7;
const TAG_ERROR: u64 = u64::MAX;

const W: usize = 420;
const H: usize = 260;
const BUF: usize = 0x84_0000_0000;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

/// PIT ticks between animation frames — 100 Hz, so about twelve a second.
/// Typing is drawn as it arrives; this only paces the moving background.
const TICKS_PER_FRAME: u64 = 8;

/// How much typing is kept on screen.
const COLS: usize = 46;
const ROWS: usize = 6;

static mut STRIDE: usize = 0;
static mut R_POS: u8 = 16;
static mut G_POS: u8 = 8;
static mut B_POS: u8 = 0;

static mut TEXT: [[u8; COLS]; ROWS] = [[b' '; COLS]; ROWS];
static mut CUR_ROW: usize = 0;
static mut CUR_COL: usize = 0;

fn colour(r: u8, g: u8, b: u8) -> u32 {
    unsafe { ((r as u32) << R_POS) | ((g as u32) << G_POS) | ((b as u32) << B_POS) }
}

fn pixel(x: usize, y: usize, c: u32) {
    if x >= W || y >= H {
        return;
    }
    unsafe { ((BUF + y * STRIDE + x * 4) as *mut u32).write_volatile(c) };
}

fn fill(x: usize, y: usize, w: usize, h: usize, c: u32) {
    for dy in 0..h {
        for dx in 0..w {
            pixel(x + dx, y + dy, c);
        }
    }
}

fn text(x: usize, y: usize, s: &[u8], c: u32) {
    for (i, &ch) in s.iter().enumerate() {
        for (gy, &bits) in FONT[ch as usize].iter().enumerate() {
            for gx in 0..GLYPH_W {
                if (bits >> (7 - gx)) & 1 != 0 {
                    pixel(x + i * GLYPH_W + gx, y + gy, c);
                }
            }
        }
    }
}

/// Put a typed character into the visible buffer.
fn type_char(c: u8) {
    unsafe {
        match c {
            b'\n' | 13 => newline(),
            8 | 127 => {
                if CUR_COL > 0 {
                    CUR_COL -= 1;
                    TEXT[CUR_ROW][CUR_COL] = b' ';
                }
            }
            c if c >= 0x20 && c < 0x7F => {
                TEXT[CUR_ROW][CUR_COL] = c;
                CUR_COL += 1;
                if CUR_COL == COLS {
                    newline();
                }
            }
            _ => {}
        }
    }
}

fn newline() {
    unsafe {
        CUR_COL = 0;
        if CUR_ROW + 1 < ROWS {
            CUR_ROW += 1;
        } else {
            for r in 1..ROWS {
                TEXT[r - 1] = TEXT[r];
            }
            TEXT[ROWS - 1] = [b' '; COLS];
        }
    }
}

fn draw(name: &[u8], focused: bool, frame: u32) {
    // A background that moves, so it stays obvious that the window is being
    // redrawn rather than left where it was — and dimmer when this window is
    // not the one being typed into.
    let dim: u8 = if focused { 1 } else { 3 };
    for y in 0..H {
        for x in 0..W {
            let r = ((x * 120 / W) as u32 + frame * 2) as u8 / (2 * dim) + 0x10;
            let g = ((y * 100 / H) as u32 + frame) as u8 / (2 * dim) + 0x14;
            pixel(x, y, colour(r, g, 0x30 / dim));
        }
    }

    // A band across the top that says, unmistakably, whether keys land here.
    let band = if focused { colour(0x30, 0xA0, 0x50) } else { colour(0x40, 0x40, 0x48) };
    fill(0, 0, W, GLYPH_H + 8, band);
    let label: &[u8] = if focused { b"TYPING HERE" } else { b"not focused" };
    text(8, 4, name, colour(0xFF, 0xFF, 0xFF));
    text(W - 8 - label.len() * GLYPH_W, 4, label, colour(0xFF, 0xFF, 0xFF));

    // What has been typed into this window, and only this window.
    let ink = colour(0xFF, 0xFF, 0xFF);
    unsafe {
        for r in 0..ROWS {
            text(8, GLYPH_H + 20 + r * GLYPH_H, &TEXT[r], ink);
        }
        // A block cursor, drawn only when this window would receive the next
        // key. Two of these side by side is the whole demonstration.
        if focused {
            fill(
                8 + CUR_COL * GLYPH_W,
                GLYPH_H + 20 + CUR_ROW * GLYPH_H,
                GLYPH_W,
                GLYPH_H,
                colour(0xFF, 0xFF, 0x80),
            );
        }
    }

    let hint = colour(0xC0, 0xC0, 0xC8);
    text(8, H - GLYPH_H - 6, b"Tab: switch  Esc: end session  ^D: close", hint);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(wm) = nameserver::lookup_retry(b"wm", 20) else {
        println!("wmdemo: no display server");
        syscall::sys_exit_code(1);
    };

    // Which of the session's windows this is, from the compositor. Two copies
    // of this program are otherwise the same picture twice.
    let mut name = [0u8; 9];
    name[..8].copy_from_slice(b"wmdemo #");
    name[8] = args::argv(1).and_then(|a| a.first().copied()).unwrap_or(b'1');
    let title: &[u8] = &name;

    let mut data = [0u64; 6];
    data[0] = ((W as u64) << 32) | (H as u64);
    for (i, chunk) in title.chunks(8).take(5).enumerate() {
        let mut word = [0u8; 8];
        word[..chunk.len()].copy_from_slice(chunk);
        data[1 + i] = u64::from_le_bytes(word);
    }

    let mut reply = Message::empty();
    let create = Message { sender: 0, tag: TAG_WM_CREATE, data };
    if syscall::sys_call(wm, &create, &mut reply).is_err() || reply.tag == TAG_ERROR {
        println!("wmdemo: no window (error {})", reply.data[0]);
        syscall::sys_exit_code(1);
    }
    let id = reply.data[0] as usize;
    let shmem = reply.data[1] as usize;
    unsafe { STRIDE = (reply.data[2] >> 32) as usize };

    if syscall::sys_shmem_map(shmem, BUF).is_err() {
        println!("wmdemo: could not map the window");
        syscall::sys_exit_code(1);
    }

    println!("wmdemo: window {} ({}x{})", id, W, H);

    let mut frame: u32 = 0;
    let mut focused = false;
    let mut next_frame = syscall::sys_ticks();
    loop {
        // Everything that has happened since the last look. Draining rather
        // than taking one means a burst of typing arrives whole, at the cost
        // of nothing: the queue is empty almost every time.
        let mut dirty = false;
        loop {
            let poll = Message { sender: 0, tag: TAG_WM_POLL_EVENT, data: [id as u64, 0, 0, 0, 0, 0] };
            if syscall::sys_call(wm, &poll, &mut reply).is_err() || reply.tag == TAG_ERROR {
                // The compositor has gone. So has the window.
                syscall::sys_exit_code(0);
            }
            let now_focused = reply.data[5] != 0;
            if now_focused != focused {
                focused = now_focused;
                dirty = true;
            }
            if reply.data[0] == 0 {
                break;
            }
            let ascii = reply.data[1] as u8;
            let press = reply.data[4] != 0;
            if !press {
                continue; // releases are delivered; nothing here wants them
            }
            if ascii == 0x04 {
                // Ctrl-D: close this window and stop. The session ends when
                // the last of these does.
                let destroy =
                    Message { sender: 0, tag: TAG_WM_DESTROY, data: [id as u64, 0, 0, 0, 0, 0] };
                let _ = syscall::sys_call(wm, &destroy, &mut reply);
                syscall::sys_exit_code(0);
            }
            type_char(ascii);
            dirty = true;
        }

        // Redraw when something was typed, and otherwise only as fast as the
        // background moves: a full window is a hundred thousand pixels, and
        // there is no reason to pay for it a hundred times a second.
        let now = syscall::sys_ticks();
        if now >= next_frame {
            next_frame = now + TICKS_PER_FRAME;
            frame = frame.wrapping_add(1);
            dirty = true;
        }
        if dirty {
            draw(title, focused, frame);
            let commit =
                Message { sender: 0, tag: TAG_WM_COMMIT, data: [id as u64, 0, 0, 0, 0, 0] };
            let _ = syscall::sys_call(wm, &commit, &mut reply);
        }

        // Paced against the clock, not against the scheduler. A bare yield
        // loop polls the compositor as fast as the machine will go, which
        // taxes it for latency it cannot use: a tick is ten milliseconds, and
        // nobody types faster than that.
        let tick = syscall::sys_ticks();
        while syscall::sys_ticks() == tick {
            syscall::sys_yield();
        }
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("wmdemo: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
