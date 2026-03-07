use font8x8::legacy::BASIC_LEGACY;

use crate::sync::IrqSpinLock;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

struct FbState {
    fb: usize,
    pitch: usize,
    width: usize,
    height: usize,
    bpp: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    disabled: bool,
    r_pos: u8,
    g_pos: u8,
    b_pos: u8,
}

impl FbState {
    fn encode_color(&self, r: u8, g: u8, b: u8) -> u32 {
        (r as u32) << self.r_pos | (g as u32) << self.g_pos | (b as u32) << self.b_pos
    }

    fn draw_glyph(&self, col: usize, row: usize, ch: u8) {
        let glyph = if (ch as usize) < BASIC_LEGACY.len() {
            BASIC_LEGACY[ch as usize]
        } else {
            BASIC_LEGACY[b'?' as usize]
        };

        let pixel_x = col * GLYPH_W;
        let pixel_y = row * GLYPH_H;

        let bytes_per_pixel = self.bpp / 8;
        let fg = self.encode_color(0xCC, 0xCC, 0xCC);

        for (gy, &glyph_row) in glyph.iter().enumerate() {
            let y = pixel_y + gy;
            let row_base = self.fb + y * self.pitch + pixel_x * bytes_per_pixel;

            for gx in 0..GLYPH_W {
                let on = (glyph_row >> gx) & 1 != 0;
                let color = if on { fg } else { 0 };
                let px = row_base + gx * bytes_per_pixel;

                unsafe {
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

    fn scroll(&mut self) {
        let buf = self.fb as *mut u8;
        let row_bytes = GLYPH_H * self.pitch;
        let text_area = self.rows * row_bytes;

        unsafe {
            for i in 0..(text_area - row_bytes) {
                let val = buf.add(i + row_bytes).read_volatile();
                buf.add(i).write_volatile(val);
            }

            let last_row_start = (self.rows - 1) * row_bytes;
            for i in 0..row_bytes {
                buf.add(last_row_start + i).write_volatile(0);
            }
        }

        self.row = self.rows - 1;
    }

    fn putc(&mut self, c: u8) {
        match c {
            b'\n' => {
                self.col = 0;
                self.row += 1;
            }
            byte => {
                self.draw_glyph(self.col, self.row, byte);
                self.col += 1;
                if self.col >= self.cols {
                    self.col = 0;
                    self.row += 1;
                }
            }
        }
        if self.row >= self.rows {
            self.scroll();
        }
    }
}

static FB_STATE: IrqSpinLock<FbState> = IrqSpinLock::new(FbState {
    fb: 0,
    pitch: 0,
    width: 0,
    height: 0,
    bpp: 0,
    cols: 0,
    rows: 0,
    col: 0,
    row: 0,
    disabled: false,
    r_pos: 16,
    g_pos: 8,
    b_pos: 0,
});

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
    let mut state = FB_STATE.lock();
    state.fb = addr as usize;
    state.pitch = pitch as usize;
    state.width = width as usize;
    state.height = height as usize;
    state.bpp = bpp as usize;
    state.cols = state.width / GLYPH_W;
    state.rows = state.height / GLYPH_H;
    state.col = 0;
    state.row = 0;
    state.r_pos = red_pos;
    state.g_pos = green_pos;
    state.b_pos = blue_pos;
}

pub fn cursor_pos_and_disable() -> (usize, usize) {
    let mut state = FB_STATE.lock();
    let pos = (state.row, state.col);
    state.disabled = true;
    pos
}

pub fn clear() {
    let mut state = FB_STATE.lock();
    let total = state.pitch * state.height;
    let buf = state.fb as *mut u8;
    unsafe {
        for i in 0..total {
            buf.add(i).write_volatile(0);
        }
    }
    state.col = 0;
    state.row = 0;
}

pub fn puts(s: &[u8]) {
    let mut state = FB_STATE.lock();
    if state.disabled {
        return;
    }
    for &b in s {
        state.putc(b);
    }
}
