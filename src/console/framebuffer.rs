use font8x8::legacy::BASIC_LEGACY;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

static mut FB: usize = 0;
static mut PITCH: usize = 0;
static mut WIDTH: usize = 0;  // pixels
static mut HEIGHT: usize = 0; // pixels
static mut BPP: usize = 0;
static mut COLS: usize = 0;   // text columns
static mut ROWS: usize = 0;   // text rows
static mut COL: usize = 0;
static mut ROW: usize = 0;
static mut DISABLED: bool = false;

// RGB channel bit positions
static mut R_POS: u8 = 16;
static mut G_POS: u8 = 8;
static mut B_POS: u8 = 0;

pub unsafe fn init(
    addr: u64,
    pitch: u32,
    width: u32,
    height: u32,
    bpp: u8,
    red_pos: u8,
    green_pos: u8,
    blue_pos: u8,
) {
    FB = addr as usize;
    PITCH = pitch as usize;
    WIDTH = width as usize;
    HEIGHT = height as usize;
    BPP = bpp as usize;
    COLS = WIDTH / GLYPH_W;
    ROWS = HEIGHT / GLYPH_H;
    COL = 0;
    ROW = 0;
    R_POS = red_pos;
    G_POS = green_pos;
    B_POS = blue_pos;
}

/// Returns the current cursor position and disables the kernel framebuffer
/// console. After this call, all framebuffer output is handled by the
/// user-space console server.
pub fn cursor_pos_and_disable() -> (usize, usize) {
    unsafe {
        let pos = (ROW, COL);
        DISABLED = true;
        pos
    }
}

pub fn clear() {
    unsafe {
        let buf = FB as *mut u8;
        let total = PITCH * HEIGHT;
        for i in 0..total {
            buf.add(i).write_volatile(0);
        }
        COL = 0;
        ROW = 0;
    }
}

pub fn puts(s: &[u8]) {
    unsafe {
        if DISABLED {
            return;
        }
    }
    for &b in s {
        putc(b);
    }
}

fn putc(c: u8) {
    unsafe {
        match c {
            b'\n' => {
                COL = 0;
                ROW += 1;
            }
            byte => {
                draw_glyph(COL, ROW, byte);
                COL += 1;
                if COL >= COLS {
                    COL = 0;
                    ROW += 1;
                }
            }
        }
        if ROW >= ROWS {
            scroll();
        }
    }
}

fn draw_glyph(col: usize, row: usize, ch: u8) {
    let glyph = if (ch as usize) < BASIC_LEGACY.len() {
        BASIC_LEGACY[ch as usize]
    } else {
        BASIC_LEGACY[b'?' as usize]
    };

    let pixel_x = col * GLYPH_W;
    let pixel_y = row * GLYPH_H;

    unsafe {
        let bytes_per_pixel = BPP / 8;
        let fg = encode_color(0xCC, 0xCC, 0xCC); // light gray foreground

        for (gy, &glyph_row) in glyph.iter().enumerate() {
            let y = pixel_y + gy;
            let row_base = FB + y * PITCH + pixel_x * bytes_per_pixel;

            for gx in 0..GLYPH_W {
                let on = (glyph_row >> gx) & 1 != 0;
                let color = if on { fg } else { 0 };
                let px = row_base + gx * bytes_per_pixel;

                if bytes_per_pixel == 4 {
                    (px as *mut u32).write_volatile(color);
                } else if bytes_per_pixel == 3 {
                    let ptr = px as *mut u8;
                    ptr.write_volatile(color as u8);
                    ptr.add(1).write_volatile((color >> 8) as u8);
                    ptr.add(2).write_volatile((color >> 16) as u8);
                }
            }
        }
    }
}

unsafe fn encode_color(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) << R_POS | (g as u32) << G_POS | (b as u32) << B_POS
}

fn scroll() {
    unsafe {
        let buf = FB as *mut u8;
        let row_bytes = GLYPH_H * PITCH;
        let text_area = ROWS * row_bytes;

        // Copy rows 1..ROWS up by one text row
        for i in 0..(text_area - row_bytes) {
            let val = buf.add(i + row_bytes).read_volatile();
            buf.add(i).write_volatile(val);
        }

        // Clear last text row
        let last_row_start = (ROWS - 1) * row_bytes;
        for i in 0..row_bytes {
            buf.add(last_row_start + i).write_volatile(0);
        }

        ROW = ROWS - 1;
    }
}
