#![no_std]
#![no_main]
#![allow(static_mut_refs)]

//! A window you can type into.
//!
//! Its companion `wmdemo` shows that a window is a buffer the display server
//! reads; this one shows where keys go. Run two of them — `wm wmtype wmtype` —
//! and only one has focus, so only one sees what is typed, and which one that
//! is changes with Tab.
//!
//! Events are pulled rather than pushed. The display server cannot send to a
//! program it spawned: originating IPC needs an `Endpoint` capability naming
//! the destination, and a task ID that did not exist when the server started
//! is not one it can mint. Answering a caller needs no capability, so the
//! client does the asking — which is the same shape as `XNextEvent` anyway.

use quark_rt::font::FONT;
use quark_rt::wm;
use quark_rt::{args, println, syscall};

// Nothing to declare: the window's memory belongs to the display server, and
// this only maps what it is given.

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

static mut WIN: Option<wm::Window> = None;
static mut TEXT: [[u8; COLS]; ROWS] = [[b' '; COLS]; ROWS];
static mut CUR_ROW: usize = 0;
static mut CUR_COL: usize = 0;

fn win() -> &'static mut wm::Window {
    unsafe { (&mut *core::ptr::addr_of_mut!(WIN)).as_mut().unwrap() }
}

fn pixel(x: usize, y: usize, c: u32) {
    if x >= W || y >= H {
        return;
    }
    win().row(y)[x] = c;
}

fn fill(x: usize, y: usize, w: usize, h: usize, c: u32) {
    let x1 = (x + w).min(W);
    let y1 = (y + h).min(H);
    if x >= x1 {
        return;
    }
    for row in y..y1 {
        win().row(row)[x..x1].fill(c);
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
            c if (0x20..0x7F).contains(&c) => {
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

fn draw(name: &[u8], frame: u32, col: &[u8; W]) {
    let w = win();
    let focused = w.focused;
    // A background that moves, so it stays obvious the window is being
    // redrawn — and dimmer when this window is not the one being typed into.
    let dim: u8 = if focused { 1 } else { 3 };
    let r_pos = w.r_pos;
    let g_pos = w.g_pos;
    let blue = w.colour(0, 0, 0x30 / dim);
    for y in 0..H {
        let g = (((y * 100 / H) as u32 + frame) as u8) / (2 * dim) + 0x14;
        let g_bits = (g as u32) << g_pos;
        let row = w.row(y);
        for x in 0..W {
            let r = (col[x] as u32 + frame * 2) as u8 / (2 * dim) + 0x10;
            row[x] = ((r as u32) << r_pos) | g_bits | blue;
        }
    }

    // A band across the top that says, unmistakably, whether keys land here.
    let band = if focused { win().colour(0x30, 0xA0, 0x50) } else { win().colour(0x40, 0x40, 0x48) };
    let white = win().colour(0xFF, 0xFF, 0xFF);
    fill(0, 0, W, GLYPH_H + 8, band);
    let label: &[u8] = if focused { b"TYPING HERE" } else { b"not focused" };
    text(8, 4, name, white);
    text(W - 8 - label.len() * GLYPH_W, 4, label, white);

    // What has been typed into this window, and only this window.
    unsafe {
        for r in 0..ROWS {
            text(8, GLYPH_H + 20 + r * GLYPH_H, &TEXT[r], white);
        }
        // A block cursor, drawn only when this window would receive the next
        // key. Two of these side by side is the whole demonstration.
        if focused {
            fill(
                8 + CUR_COL * GLYPH_W,
                GLYPH_H + 20 + CUR_ROW * GLYPH_H,
                GLYPH_W,
                GLYPH_H,
                win().colour(0xFF, 0xFF, 0x80),
            );
        }
    }

    let hint = win().colour(0xC0, 0xC0, 0xC8);
    text(8, H - GLYPH_H - 6, b"Tab: switch  Esc: end session  ^D: close", hint);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(server) = wm::connect() else {
        println!("wmtype: no display server");
        syscall::sys_exit_code(1);
    };

    // Which of the session's windows this is, from the display server. Two
    // copies of this program are otherwise the same picture twice.
    let mut title = [0u8; 9];
    title[..8].copy_from_slice(b"wmtype #");
    title[8] = args::argv(1).and_then(|a| a.first().copied()).unwrap_or(b'1');

    let Some(window) = wm::Window::create(server, W, H, &title, BUF) else {
        println!("wmtype: no window");
        syscall::sys_exit_code(1);
    };
    unsafe { WIN = Some(window) };
    println!("wmtype: window {} ({}x{})", win().id, W, H);

    let mut col = [0u8; W];
    for (x, c) in col.iter_mut().enumerate() {
        *c = (x * 120 / W) as u8;
    }

    let mut frame: u32 = 0;
    let mut was_focused = false;
    let mut next_frame = syscall::sys_ticks();
    loop {
        // Everything that has happened since the last look. Draining rather
        // than taking one means a burst of typing arrives whole, at the cost
        // of nothing: the queue is empty almost every time.
        let mut dirty = false;
        loop {
            let event = match win().poll() {
                Ok(e) => e,
                // The display server has gone. So has the window.
                Err(()) => syscall::sys_exit_code(0),
            };
            if win().focused != was_focused {
                was_focused = win().focused;
                dirty = true;
            }
            let Some(event) = event else { break };
            if !event.press {
                continue; // releases are delivered; nothing here wants them
            }
            if event.ascii == 0x04 {
                // Ctrl-D: stop. Without saying so, on purpose — a client that
                // simply exits should not leave a window behind, and this is
                // where that gets tested.
                syscall::sys_exit_code(0);
            }
            type_char(event.ascii);
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
            draw(&title, frame, &col);
            win().commit();
        }

        // Asleep until the next tick, rather than spinning until it arrives.
        // A yield loop polls the display server as fast as the machine will
        // go, which buys latency nobody can use — a tick is ten milliseconds
        // and nobody types faster than that — and spends the time that the
        // drawing needed to get it.
        syscall::sleep_ticks(1);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("wmtype: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
