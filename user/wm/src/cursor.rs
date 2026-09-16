//! The pointer, as something on the screen.
//!
//! Drawn by the compositor rather than by the client. Wayland lets a client
//! supply its own cursor surface, and until one does there has to be something
//! to look at — a pointer nobody can see is indistinguishable from a pointer
//! that does not move.
//!
//! The shape is two colours with a hole in it: an outline that is visible on
//! white and a fill that is visible on black. One colour would disappear over
//! half the things it can be over, which is why every cursor ever drawn has
//! looked like this.

use crate::draw::{self, pack_colour, put_pixel, Rect, SCREEN};

pub const W: usize = 8;
pub const H: usize = 12;

/// 0 is nothing, 1 is the outline, 2 is the fill. The hot spot — the pixel the
/// position actually refers to — is the top left, which is why the arrow points
/// up and to the left rather than being centred.
#[rustfmt::skip]
const SHAPE: [u8; W * H] = [
    1, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 0, 0, 0, 0, 0, 0,
    1, 2, 1, 0, 0, 0, 0, 0,
    1, 2, 2, 1, 0, 0, 0, 0,
    1, 2, 2, 2, 1, 0, 0, 0,
    1, 2, 2, 2, 2, 1, 0, 0,
    1, 2, 2, 2, 2, 2, 1, 0,
    1, 2, 2, 2, 2, 2, 2, 1,
    1, 2, 2, 2, 1, 1, 1, 1,
    1, 2, 1, 2, 2, 1, 0, 0,
    1, 1, 0, 1, 2, 2, 1, 0,
    0, 0, 0, 0, 1, 1, 1, 0,
];

static mut X: usize = 0;
static mut Y: usize = 0;
/// Nothing is drawn before the pointer has been heard from. A machine with no
/// mouse should not grow an arrow in the corner of its screen.
static mut SEEN: bool = false;

pub fn position() -> (usize, usize) {
    unsafe { (X, Y) }
}

pub fn visible() -> bool {
    unsafe { SEEN }
}

/// Put it in the middle, which is where a pointer that has never moved should
/// be: a corner reads as "stuck" and the middle reads as "here".
pub fn centre() {
    let s = unsafe { &SCREEN };
    unsafe {
        X = s.width / 2;
        Y = s.height / 2;
    }
}

/// Move by a delta the driver reported, clamped to the screen.
///
/// Clamped rather than wrapped, and clamped to the last *pixel* rather than to
/// the width: a pointer allowed to reach `width` is a pointer one past the end
/// of every row.
pub fn move_by(dx: i32, dy: i32) {
    let s = unsafe { &SCREEN };
    unsafe {
        SEEN = true;
        X = clamp(X as i32 + dx, s.width);
        Y = clamp(Y as i32 + dy, s.height);
    }
}

fn clamp(v: i32, limit: usize) -> usize {
    if v < 0 {
        0
    } else if v as usize >= limit {
        limit.saturating_sub(1)
    } else {
        v as usize
    }
}

/// The screen area the cursor covers, which is what has to be repainted when
/// it moves.
pub fn rect() -> Rect {
    let s = unsafe { &SCREEN };
    let (x, y) = position();
    Rect {
        x0: x,
        y0: y,
        x1: (x + W).min(s.width),
        y1: (y + H).min(s.height),
    }
}

/// Draw it into the back buffer, clipped to the region being painted.
///
/// Called last, after every window, because a pointer behind a window is not a
/// pointer.
pub fn draw() {
    if !visible() {
        return;
    }
    let clip = unsafe { draw::CLIP };
    let area = rect().clip_to(&clip);
    if area.is_empty() {
        return;
    }
    let (ox, oy) = position();
    let outline = pack_colour(0x00, 0x00, 0x00);
    let fill = pack_colour(0xFF, 0xFF, 0xFF);
    for y in area.y0..area.y1 {
        for x in area.x0..area.x1 {
            let colour = match SHAPE[(y - oy) * W + (x - ox)] {
                1 => outline,
                2 => fill,
                _ => continue,
            };
            put_pixel(x, y, colour);
        }
    }
}
