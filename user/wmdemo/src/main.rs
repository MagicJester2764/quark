#![no_std]
#![no_main]

//! A second thing on the screen.
//!
//! There is not much to a display server client: ask for a window, map the
//! memory it gives you, write pixels into it, and say when you have finished.
//! This does exactly that, in a loop, so the result is visibly animated rather
//! than merely present — a still image proves the buffer was shared, and a
//! moving one proves it stayed shared.

use quark_rt::font::FONT;
use quark_rt::ipc::Message;
use quark_rt::{args, nameserver, println, syscall};

// Nothing to declare: the window's memory belongs to the display server, and
// this only maps what it is given.

const TAG_WM_CREATE: u64 = 1;
const TAG_WM_COMMIT: u64 = 2;
const TAG_WM_DESTROY: u64 = 6;
const TAG_ERROR: u64 = u64::MAX;

const W: usize = 420;
const H: usize = 260;
const BUF: usize = 0x84_0000_0000;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

static mut STRIDE: usize = 0;
static mut R_POS: u8 = 16;
static mut G_POS: u8 = 8;
static mut B_POS: u8 = 0;

fn colour(r: u8, g: u8, b: u8) -> u32 {
    unsafe { ((r as u32) << R_POS) | ((g as u32) << G_POS) | ((b as u32) << B_POS) }
}

fn pixel(x: usize, y: usize, c: u32) {
    if x >= W || y >= H {
        return;
    }
    unsafe { ((BUF + y * STRIDE + x * 4) as *mut u32).write_volatile(c) };
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
    let Some(wm) = nameserver::lookup_retry(b"wm", 20) else {
        println!("wmdemo: no display server");
        syscall::sys_exit_code(1);
    };

    let mut data = [0u64; 6];
    data[0] = ((W as u64) << 32) | (H as u64);
    let title = args::argv(1).unwrap_or(b"Demo");
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

    // Draw for a while, then give the window back. A client that simply
    // exited would leave the display server holding a window nothing owns:
    // there is no notification when a task dies, so saying so is the client's
    // job for now.
    for frame in 0..60u32 {
        for y in 0..H {
            for x in 0..W {
                // A gradient that moves, so it is obvious the window is being
                // redrawn rather than left where it was.
                let r = ((x * 255 / W) as u32 + frame * 4) as u8;
                let g = ((y * 255 / H) as u32 + frame * 2) as u8;
                pixel(x, y, colour(r, g, 0x60));
            }
        }
        text(16, 24, b"A second window.", colour(0xFF, 0xFF, 0xFF));
        text(16, 48, b"Its pixels live in memory", colour(0xFF, 0xFF, 0xFF));
        text(16, 66, b"the display server reads.", colour(0xFF, 0xFF, 0xFF));
        text(16, 100, b"frame", colour(0x00, 0x00, 0x00));
        let mut n = [b' '; 4];
        let mut v = frame;
        for i in (0..4).rev() {
            n[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        text(16 + 6 * GLYPH_W, 100, &n, colour(0x00, 0x00, 0x00));

        let commit = Message { sender: 0, tag: TAG_WM_COMMIT, data: [id as u64, 0, 0, 0, 0, 0] };
        let _ = syscall::sys_call(wm, &commit, &mut reply);

        for _ in 0..5 {
            syscall::sys_yield();
        }
    }

    let destroy = Message { sender: 0, tag: TAG_WM_DESTROY, data: [id as u64, 0, 0, 0, 0, 0] };
    let _ = syscall::sys_call(wm, &destroy, &mut reply);
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("wmdemo: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
