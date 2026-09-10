//! Pixels, and nothing above them.
//!
//! Everything here knows about the framebuffer, a back buffer and a clip
//! rectangle. Nothing here knows what a window is: the compositor decides what
//! to draw and in what order, and this decides what that costs.
//!
//! Two rules live here rather than in the caller, because both were learned by
//! getting them wrong. Compositing goes into a back buffer and reaches the
//! screen in one copy, so the cleared backdrop is never what anybody sees.
//! And every primitive clips to a region, so a commit costs the size of one
//! window rather than the size of the screen.

use quark_rt::font::FONT;

/// The bitmap font's cell, which is what `draw_text` steps by.
pub const GLYPH_W: usize = 8;
pub const GLYPH_H: usize = 16;

#[derive(Clone, Copy)]
pub struct Screen {
    /// Where the finished frame is copied to: the framebuffer itself.
    pub fb: usize,
    /// Where it is drawn: memory nobody is looking at.
    pub back: usize,
    pub pitch: usize,
    pub width: usize,
    pub height: usize,
    pub bpp: usize,
    pub r_pos: u8,
    pub g_pos: u8,
    pub b_pos: u8,
}

pub static mut SCREEN: Screen = Screen {
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

/// A half-open region of the screen.
#[derive(Clone, Copy)]
pub struct Rect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl Rect {
    pub fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    /// The part of this rectangle that is also in `other`.
    pub fn clip_to(&self, other: &Rect) -> Rect {
        Rect {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        }
    }
}

/// The region currently being repainted. Everything that draws clips to it.
///
/// This is what makes a commit cost the size of one window rather than the
/// size of the screen. Repainting a whole screenful to find out that a 420x260
/// window changed is a megapixel of backdrop and a four-megabyte copy per
/// update — enough that a keystroke waits behind the compositor, which is
/// exactly how it felt.
pub static mut CLIP: Rect = Rect { x0: 0, y0: 0, x1: 0, y1: 0 };

pub fn pack_colour(r: u8, g: u8, b: u8) -> u32 {
    let s = unsafe { &SCREEN };
    ((r as u32) << s.r_pos) | ((g as u32) << s.g_pos) | ((b as u32) << s.b_pos)
}

pub fn put_pixel(x: usize, y: usize, colour: u32) {
    let s = unsafe { &SCREEN };
    let clip = unsafe { CLIP };
    if x < clip.x0 || x >= clip.x1 || y < clip.y0 || y >= clip.y1 {
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
pub fn fill_rect(x: usize, y: usize, w: usize, h: usize, colour: u32) {
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

    let r = Rect { x0: x, y0: y, x1: x + w, y1: y + h }.clip_to(unsafe { &CLIP });
    if r.is_empty() {
        return;
    }

    for row in r.y0..r.y1 {
        let start = s.back + row * s.pitch + r.x0 * 4;
        let pixels = unsafe { core::slice::from_raw_parts_mut(start as *mut u32, r.x1 - r.x0) };
        pixels.fill(colour);
    }
}

pub fn draw_text(x: usize, y: usize, text: &[u8], colour: u32) {
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

/// Put the finished region on the screen.
///
/// Everything above drew into the back buffer. This is the only write to the
/// framebuffer, which is why the cleared backdrop is never what anybody sees.
pub fn present(region: Rect) {
    let s = unsafe { &SCREEN };
    let bpp = s.bpp / 8;
    let bytes = (region.x1 - region.x0) * bpp;

    // Whole rows with no padding between them are one run, so a full repaint
    // stays the single copy it always was rather than becoming eight hundred.
    if region.x0 == 0 && bytes == s.pitch {
        let off = region.y0 * s.pitch;
        unsafe {
            core::ptr::copy_nonoverlapping(
                (s.back + off) as *const u8,
                (s.fb + off) as *mut u8,
                (region.y1 - region.y0) * s.pitch,
            );
        }
        return;
    }

    for row in region.y0..region.y1 {
        let off = row * s.pitch + region.x0 * bpp;
        unsafe {
            core::ptr::copy_nonoverlapping(
                (s.back + off) as *const u8,
                (s.fb + off) as *mut u8,
                bytes,
            );
        }
    }
}

/// Start painting a region: clip it to the screen, make it the clip rectangle,
/// and lay down the backdrop.
///
/// Returns the clipped region, or `None` if there is nothing to paint — either
/// the display belongs to somebody else at the moment, or the region fell off
/// the screen. The caller then draws whatever it has and calls [`present`].
pub fn begin_region(region: Rect) -> Option<Rect> {
    let s = unsafe { &SCREEN };
    if s.fb == 0 || s.back == 0 {
        return None; // the display is somebody else's at the moment
    }
    let screen = Rect { x0: 0, y0: 0, x1: s.width, y1: s.height };
    let region = region.clip_to(&screen);
    if region.is_empty() {
        return None;
    }
    unsafe { CLIP = region };

    let backdrop = pack_colour(0x10, 0x14, 0x1C);
    fill_rect(region.x0, region.y0, region.x1 - region.x0, region.y1 - region.y0, backdrop);
    Some(region)
}

/// The whole screen, as a region.
pub fn screen_rect() -> Rect {
    let s = unsafe { &SCREEN };
    Rect { x0: 0, y0: 0, x1: s.width, y1: s.height }
}
