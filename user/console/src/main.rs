#![no_std]
#![no_main]
#![allow(static_mut_refs)]

mod font8x16;

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;

// Init -> console: framebuffer initialization
const TAG_FB_INIT: u64 = 100;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

static mut FB: usize = 0;
static mut PITCH: usize = 0;
static mut WIDTH: usize = 0;
static mut HEIGHT: usize = 0;
static mut BPP: usize = 0;
static mut COLS: usize = 0;
static mut ROWS: usize = 0;
static mut COL: usize = 0;
static mut ROW: usize = 0;
static mut R_POS: u8 = 16;
static mut G_POS: u8 = 8;
static mut B_POS: u8 = 0;
static mut INITIALIZED: bool = false;

// ANSI escape sequence state machine
static mut ESC_STATE: u8 = 0;       // 0=normal, 1=got ESC, 2=got CSI
static mut ESC_PARAMS: [u16; 4] = [0; 4];
static mut ESC_PARAM_COUNT: usize = 0;

// Foreground color (set via SGR escape codes)
static mut FG_COLOR: u32 = 0;

// Cursor blink state
static mut CURSOR_VISIBLE: bool = true;
static mut CURSOR_LAST_TOGGLE: u64 = 0;
const CURSOR_BLINK_TICKS: u64 = 50; // 500ms at 100 Hz

// Text cell buffer — avoids expensive framebuffer reads during scroll
const MAX_CELL_COLS: usize = 320;
const MAX_CELL_ROWS: usize = 200;

static mut CELL_CH: [u8; MAX_CELL_COLS * MAX_CELL_ROWS] = [0; MAX_CELL_COLS * MAX_CELL_ROWS];
static mut CELL_FG: [u32; MAX_CELL_COLS * MAX_CELL_ROWS] = [0; MAX_CELL_COLS * MAX_CELL_ROWS];
static mut DIRTY_MIN: usize = usize::MAX;
static mut DIRTY_MAX: usize = 0;

fn cell_idx(col: usize, row: usize) -> usize {
    row * MAX_CELL_COLS + col
}

unsafe fn mark_dirty(row: usize) {
    if row < DIRTY_MIN { DIRTY_MIN = row; }
    if row > DIRTY_MAX { DIRTY_MAX = row; }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[console] Started, waiting for FB init.");

    // Wait for framebuffer init message from init
    let mut msg = Message::empty();
    if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
        println!("[console] Failed to receive FB init.");
        syscall::sys_exit();
    }

    if msg.tag != TAG_FB_INIT {
        println!("[console] Unexpected first message.");
        syscall::sys_exit();
    }

    init_framebuffer(&msg);

    // Reply to init so it knows we're ready
    let reply = Message { sender: 0, tag: 0, data: [0; 6] };
    let _ = syscall::sys_reply(msg.sender, &reply);

    // Register with nameserver
    register_with_nameserver();

    println!("[console] Ready.");

    unsafe { CURSOR_LAST_TOGGLE = syscall::sys_ticks(); }

    // Main loop: non-blocking read from pipe, blink cursor on idle
    loop {
        let mut buf = [0u8; 256];
        let n = syscall::sys_fd_read_nb(0, &mut buf);
        if n == 0 {
            break; // EOF
        } else if n == syscall::WOULD_BLOCK || n == u64::MAX {
            // No data — check cursor blink
            let now = syscall::sys_ticks();
            unsafe {
                if now.wrapping_sub(CURSOR_LAST_TOGGLE) >= CURSOR_BLINK_TICKS {
                    CURSOR_VISIBLE = !CURSOR_VISIBLE;
                    CURSOR_LAST_TOGGLE = now;
                    draw_cursor();
                }
            }
            syscall::sys_yield();
        } else {
            // Got data — hide cursor, write, show cursor
            unsafe { hide_cursor(); }
            write_bytes(&buf[..n as usize]);
            unsafe {
                CURSOR_VISIBLE = true;
                CURSOR_LAST_TOGGLE = syscall::sys_ticks();
                draw_cursor();
            }
        }
    }

    syscall::sys_exit();
}

fn init_framebuffer(msg: &Message) {
    // data[0] = physical address
    // data[1] = (width << 32) | height
    // data[2] = (pitch << 32) | bpp
    // data[3] = (red_pos << 16) | (green_pos << 8) | blue_pos
    // data[4] = (cursor_row << 32) | cursor_col
    let phys_addr = msg.data[0] as usize;
    let w = (msg.data[1] >> 32) as usize;
    let h = (msg.data[1] & 0xFFFF_FFFF) as usize;
    let pitch = (msg.data[2] >> 32) as usize;
    let bpp = (msg.data[2] & 0xFF) as usize;
    let rp = ((msg.data[3] >> 16) & 0xFF) as u8;
    let gp = ((msg.data[3] >> 8) & 0xFF) as u8;
    let bp = (msg.data[3] & 0xFF) as u8;
    let cursor_row = (msg.data[4] >> 32) as usize;
    let cursor_col = (msg.data[4] & 0xFFFF_FFFF) as usize;

    // Map the framebuffer into our address space
    let fb_size = pitch * h;
    let pages = (fb_size + 4095) / 4096;
    let fb_vaddr: usize = 0x81_0000_0000;

    if syscall::sys_map_phys(phys_addr, fb_vaddr, pages).is_err() {
        println!("[console] Failed to map framebuffer!");
        return;
    }

    let rows = h / GLYPH_H;
    let cols = w / GLYPH_W;

    unsafe {
        FB = fb_vaddr;
        PITCH = pitch;
        WIDTH = w;
        HEIGHT = h;
        BPP = bpp;
        COLS = cols;
        ROWS = rows;
        // Continue where the kernel console left off
        COL = cursor_col.min(cols.saturating_sub(1));
        ROW = cursor_row.min(rows.saturating_sub(1));
        R_POS = rp;
        G_POS = gp;
        B_POS = bp;
        INITIALIZED = true;
        FG_COLOR = encode_color(0xCC, 0xCC, 0xCC);
    }
}

fn write_bytes(s: &[u8]) {
    unsafe {
        if !INITIALIZED {
            return;
        }
    }
    for &b in s {
        putc(b);
    }
    flush_dirty();
}

fn putc(c: u8) {
    unsafe {
        match ESC_STATE {
            0 => match c {
                0x1b => { ESC_STATE = 1; }
                b'\n' => { COL = 0; ROW += 1; }
                b'\r' => { COL = 0; }
                b'\t' => {
                    let next = (COL + 8) & !7;
                    COL = if next < COLS { next } else { COLS - 1 };
                }
                0x08 => {
                    if COL > 0 { COL -= 1; }
                }
                byte => {
                    let idx = cell_idx(COL, ROW);
                    CELL_CH[idx] = byte;
                    CELL_FG[idx] = FG_COLOR;
                    mark_dirty(ROW);
                    COL += 1;
                    if COL >= COLS { COL = 0; ROW += 1; }
                }
            },
            1 => {
                if c == b'[' {
                    ESC_STATE = 2;
                    ESC_PARAMS = [0; 4];
                    ESC_PARAM_COUNT = 0;
                } else {
                    ESC_STATE = 0;
                }
            },
            2 => {
                if c >= b'0' && c <= b'9' {
                    let idx = ESC_PARAM_COUNT;
                    if idx < 4 {
                        ESC_PARAMS[idx] = ESC_PARAMS[idx] * 10 + (c - b'0') as u16;
                    }
                } else if c == b';' {
                    if ESC_PARAM_COUNT < 3 { ESC_PARAM_COUNT += 1; }
                } else {
                    if ESC_PARAM_COUNT < 4 { ESC_PARAM_COUNT += 1; }
                    dispatch_csi(c);
                    ESC_STATE = 0;
                }
            },
            _ => { ESC_STATE = 0; }
        }
        if ROW >= ROWS { scroll(); }
    }
}

fn draw_glyph(col: usize, row: usize, ch: u8, fg: u32) {
    let glyph = &font8x16::FONT[ch as usize];

    let pixel_x = col * GLYPH_W;
    let pixel_y = row * GLYPH_H;

    unsafe {
        let bytes_per_pixel = BPP / 8;

        for (gy, &glyph_row) in glyph.iter().enumerate() {
            let y = pixel_y + gy;
            let row_base = FB + y * PITCH + pixel_x * bytes_per_pixel;

            for gx in 0..8 {
                let on = (glyph_row >> (7 - gx)) & 1 != 0;
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

fn dispatch_csi(cmd: u8) {
    unsafe {
        let p0 = ESC_PARAMS[0] as usize;
        let p1 = ESC_PARAMS[1] as usize;
        match cmd {
            b'A' => { ROW = ROW.saturating_sub(p0.max(1)); }
            b'B' => { ROW = (ROW + p0.max(1)).min(ROWS - 1); }
            b'C' => { COL = (COL + p0.max(1)).min(COLS - 1); }
            b'D' => { COL = COL.saturating_sub(p0.max(1)); }
            b'H' => {
                ROW = if p0 > 0 { (p0 - 1).min(ROWS - 1) } else { 0 };
                COL = if p1 > 0 { (p1 - 1).min(COLS - 1) } else { 0 };
            }
            b'J' => {
                if p0 == 2 {
                    // Clear cell buffer
                    for i in 0..(ROWS * MAX_CELL_COLS) {
                        CELL_CH[i] = 0;
                        CELL_FG[i] = 0;
                    }
                    // Clear framebuffer directly
                    let buf = FB as *mut u8;
                    let total = HEIGHT * PITCH;
                    for i in 0..total { buf.add(i).write_volatile(0); }
                    ROW = 0; COL = 0;
                    DIRTY_MIN = usize::MAX;
                    DIRTY_MAX = 0;
                }
            }
            b'K' => {
                if p0 == 0 {
                    for c in COL..COLS {
                        let idx = cell_idx(c, ROW);
                        CELL_CH[idx] = 0;
                        CELL_FG[idx] = 0;
                    }
                    mark_dirty(ROW);
                }
            }
            b'm' => {
                for i in 0..ESC_PARAM_COUNT {
                    apply_sgr(ESC_PARAMS[i]);
                }
                if ESC_PARAM_COUNT == 0 {
                    apply_sgr(0);
                }
            }
            _ => {}
        }
    }
}

fn apply_sgr(code: u16) {
    unsafe {
        match code {
            0  => { FG_COLOR = encode_color(0xCC, 0xCC, 0xCC); }
            1  => { FG_COLOR = encode_color(0xFF, 0xFF, 0xFF); }
            30 => { FG_COLOR = encode_color(0x00, 0x00, 0x00); }
            31 => { FG_COLOR = encode_color(0xCC, 0x00, 0x00); }
            32 => { FG_COLOR = encode_color(0x00, 0xCC, 0x00); }
            33 => { FG_COLOR = encode_color(0xCC, 0xCC, 0x00); }
            34 => { FG_COLOR = encode_color(0x00, 0x00, 0xCC); }
            35 => { FG_COLOR = encode_color(0xCC, 0x00, 0xCC); }
            36 => { FG_COLOR = encode_color(0x00, 0xCC, 0xCC); }
            37 => { FG_COLOR = encode_color(0xCC, 0xCC, 0xCC); }
            _  => {}
        }
    }
}

fn scroll() {
    unsafe {
        // Flush any pending dirty rows to the framebuffer BEFORE scrolling pixels,
        // so the FB is in sync when we copy pixels upward.
        flush_dirty();

        let stride = MAX_CELL_COLS;
        let used = ROWS * stride;

        // Shift cell arrays up by one row (fast — normal cached RAM)
        core::ptr::copy(
            CELL_CH.as_ptr().add(stride),
            CELL_CH.as_mut_ptr(),
            used - stride,
        );
        core::ptr::copy(
            CELL_FG.as_ptr().add(stride),
            CELL_FG.as_mut_ptr(),
            used - stride,
        );

        // Clear last cell row
        let last = (ROWS - 1) * stride;
        for i in last..last + COLS {
            CELL_CH[i] = 0;
            CELL_FG[i] = 0;
        }

        // Scroll framebuffer pixels up by one text row instead of full redraw,
        // so pre-existing content (e.g. kernel boot text) is preserved.
        let shift = GLYPH_H * PITCH;
        let total = ROWS * GLYPH_H * PITCH;
        core::ptr::copy(
            (FB + shift) as *const u8,
            FB as *mut u8,
            total - shift,
        );

        // Clear the last text row in the framebuffer
        core::ptr::write_bytes(
            (FB + (ROWS - 1) * GLYPH_H * PITCH) as *mut u8,
            0,
            GLYPH_H * PITCH,
        );

        ROW = ROWS - 1;
    }
}

fn flush_dirty() {
    unsafe {
        if !INITIALIZED || DIRTY_MIN > DIRTY_MAX {
            return;
        }
        let min = DIRTY_MIN;
        let max = if DIRTY_MAX >= ROWS { ROWS - 1 } else { DIRTY_MAX };
        for row in min..=max {
            for col in 0..COLS {
                let idx = cell_idx(col, row);
                draw_glyph(col, row, CELL_CH[idx], CELL_FG[idx]);
            }
        }
        DIRTY_MIN = usize::MAX;
        DIRTY_MAX = 0;
    }
}

/// Draw cursor block at current position.
unsafe fn draw_cursor() {
    if !INITIALIZED { return; }
    if CURSOR_VISIBLE {
        // Draw a solid block at (COL, ROW) using FG_COLOR
        draw_cursor_block(FG_COLOR);
    } else {
        // Restore the cell content at cursor position
        hide_cursor();
    }
}

/// Erase cursor by redrawing the cell content at cursor position.
unsafe fn hide_cursor() {
    if !INITIALIZED { return; }
    if COL < COLS && ROW < ROWS {
        let idx = cell_idx(COL, ROW);
        draw_glyph(COL, ROW, CELL_CH[idx], CELL_FG[idx]);
    }
}

/// Draw a solid underline cursor (bottom 2 rows of the glyph cell).
unsafe fn draw_cursor_block(color: u32) {
    if COL >= COLS || ROW >= ROWS { return; }
    let pixel_x = COL * GLYPH_W;
    let pixel_y = ROW * GLYPH_H;
    let bytes_per_pixel = BPP / 8;

    // Draw bottom 2 pixel rows as a solid underline
    for gy in (GLYPH_H - 2)..GLYPH_H {
        let y = pixel_y + gy;
        let row_base = FB + y * PITCH + pixel_x * bytes_per_pixel;
        for gx in 0..GLYPH_W {
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

fn register_with_nameserver() {
    let name = b"console";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_REGISTER,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
        println!("[console] Registered with nameserver.");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[console] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
