#![no_std]
#![no_main]

//! A second thing on the screen.
//!
//! There is not much to a display server client: ask for a window, map the
//! memory it gives you, write pixels into it, and say when you have finished.
//! This does exactly that, in a loop, so the result is visibly animated rather
//! than merely present — a still image proves the buffer was shared, and a
//! moving one proves it stayed shared.
//!
//! Its companion `wmtype` is the same idea with the events attached: this one
//! shows that a window is a shared buffer, that one shows where keys go.

use quark_rt::font::FONT;
use quark_rt::wm;
use quark_rt::{args, println, syscall};

// Nothing to declare: the window's memory belongs to the display server, and
// this only maps what it is given.

const W: usize = 420;
const H: usize = 260;
const BUF: usize = 0x84_0000_0000;

const GLYPH_W: usize = 8;

/// PIT ticks between frames — 100 Hz, so this is about twelve frames a second.
const TICKS_PER_FRAME: u64 = 8;
/// How long the demo runs before giving its window back: long enough to look
/// at, short enough that the shell comes back on its own.
const FRAMES: u32 = 150;

static mut WIN: Option<wm::Window> = None;

fn win() -> &'static mut wm::Window {
    unsafe { (&mut *core::ptr::addr_of_mut!(WIN)).as_mut().unwrap() }
}

fn pixel(x: usize, y: usize, c: u32) {
    if x >= W || y >= H {
        return;
    }
    win().row(y)[x] = c;
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

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(server) = wm::connect() else {
        println!("wmdemo: no display server");
        syscall::sys_exit_code(1);
    };

    // Which of the session's windows this is, from the display server. Two
    // copies of this program are otherwise the same picture twice.
    let mut title = [0u8; 9];
    title[..8].copy_from_slice(b"wmdemo #");
    title[8] = args::argv(1).and_then(|a| a.first().copied()).unwrap_or(b'1');

    let Some(window) = wm::Window::create(server, W, H, &title, BUF) else {
        println!("wmdemo: no window");
        syscall::sys_exit_code(1);
    };
    unsafe { WIN = Some(window) };
    println!("wmdemo: window {} ({}x{})", win().id, W, H);

    // The red channel varies along a row and the green down the page, so a
    // frame is one value per column plus one per row — worked out once here
    // rather than a divide per pixel, sixty times a second.
    let mut col = [0u8; W];
    for (x, c) in col.iter_mut().enumerate() {
        *c = (x * 255 / W) as u8;
    }

    let white = win().colour(0xFF, 0xFF, 0xFF);
    let black = win().colour(0x00, 0x00, 0x00);
    let blue = win().colour(0x00, 0x00, 0x60);

    for frame in 0..FRAMES {
        // A gradient that moves, so it is obvious the window is being redrawn
        // rather than left where it was. Written a row at a time: the display
        // server reads this buffer only after the round trip a commit makes,
        // so there is nothing a per-pixel volatile store would order against
        // and plenty it would stop the compiler doing.
        let w = win();
        let r_pos = w.r_pos;
        let g_pos = w.g_pos;
        for y in 0..H {
            let g = ((y * 255 / H) as u32 + frame * 2) as u8;
            let g_bits = (g as u32) << g_pos;
            let row = w.row(y);
            for x in 0..W {
                let r = (col[x] as u32 + frame * 4) as u8;
                row[x] = ((r as u32) << r_pos) | g_bits | blue;
            }
        }

        text(16, 24, b"A second window.", white);
        text(16, 48, b"Its pixels live in memory", white);
        text(16, 66, b"the display server reads.", white);
        text(16, 100, b"frame", black);
        let mut n = [b' '; 4];
        let mut v = frame;
        for i in (0..4).rev() {
            n[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        text(16 + 6 * GLYPH_W, 100, &n, black);

        win().commit();

        // Paced against the clock, not against the scheduler. Asleep rather
        // than yielding in a loop: a yield loop is a delay that costs a whole
        // core to wait out, and on a machine where two of these and a
        // compositor are all doing it, the time goes to spinning instead of to
        // whoever is drawing.
        syscall::sleep_ticks(TICKS_PER_FRAME);
    }

    // Saying so is the polite way. A client that simply exits is handled too —
    // the display server is told when a task dies and takes its windows with
    // it — but that is the safety net, not the protocol.
    win().destroy();
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("wmdemo: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
