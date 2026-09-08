#![no_std]
#![no_main]
#![allow(static_mut_refs)]

use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::nameserver;
use quark_rt::{println, syscall};


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

/// The framebuffer device server, and whether the display is ours right now.
///
/// A compositor can take the screen while this keeps running: the text carries
/// on accumulating in the cell buffer, and is redrawn from it when the display
/// comes back.
static mut FB_TID: usize = 0;
static mut HAVE_DISPLAY: bool = false;

/// Where the framebuffer is mapped.
const FB_VADDR: usize = 0x81_0000_0000;
/// The slot the framebuffer device grants the display into. Fixed by that
/// device, and emptied again when the display goes back.
const FB_LEASE_SLOT: usize = 2;

const TAG_FB_CLAIM: u64 = 2;
const TAG_FB_LOST: u64 = 0x100;
const TAG_FB_GAINED: u64 = 0x101;
const TAG_FB_ERROR: u64 = u64::MAX;

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
    println!("[console] Started.");

    if !claim_display() {
        println!("[console] No display; nothing to draw on.");
        syscall::sys_exit_code(1);
    }

    // Register with nameserver
    if nameserver::register(b"console").is_ok() {
        println!("[console] Registered with nameserver.");
    }

    println!("[console] Ready.");

    unsafe { CURSOR_LAST_TOGGLE = syscall::sys_ticks(); }

    // Main loop: non-blocking read from pipe, blink cursor on idle
    loop {
        let mut buf = [0u8; 256];
        let n = syscall::sys_fd_read_nb(0, &mut buf);
        if n == 0 {
            break; // EOF
        } else if n == syscall::WOULD_BLOCK || n == u64::MAX {
            // Nothing to print. A good moment to notice the display changing
            // hands, and to blink.
            poll_display_handover();
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

/// Take the display and start drawing on it.
///
/// The console is an ordinary client of the framebuffer device: it asks for
/// the screen, is given the right to map it, and draws text across the whole
/// of it. No frame, no title bar — this is the text console the machine boots
/// into, and it is the only thing on the screen until something else asks for
/// it.
fn claim_display() -> bool {
    let Some(fb) = nameserver::lookup_retry(b"fb", 30) else {
        return false;
    };
    unsafe { FB_TID = fb };

    let msg = Message { sender: 0, tag: TAG_FB_CLAIM, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(fb, &msg, &mut reply).is_err() || reply.tag == TAG_FB_ERROR {
        println!("[console] the framebuffer would not give up the display");
        return false;
    }
    adopt_mode(&reply)
}

/// Map the framebuffer and lay the text grid out on it.
fn adopt_mode(reply: &Message) -> bool {
    let w = (reply.data[0] >> 32) as usize;
    let h = (reply.data[0] & 0xFFFF_FFFF) as usize;
    let pitch = (reply.data[1] >> 32) as usize;
    let bpp = (reply.data[1] & 0xFF) as usize;
    let phys = reply.data[3] as usize;

    let pages = (pitch * h + 4095) / 4096;
    if syscall::sys_map_phys(phys, FB_VADDR, pages).is_err() {
        println!("[console] could not map the framebuffer");
        return false;
    }

    unsafe {
        FB = FB_VADDR;
        PITCH = pitch;
        WIDTH = w;
        HEIGHT = h;
        BPP = bpp;
        R_POS = ((reply.data[2] >> 16) & 0xFF) as u8;
        G_POS = ((reply.data[2] >> 8) & 0xFF) as u8;
        B_POS = (reply.data[2] & 0xFF) as u8;
        COLS = (w / GLYPH_W).min(MAX_CELL_COLS);
        ROWS = (h / GLYPH_H).min(MAX_CELL_ROWS);
        if FG_COLOR == 0 {
            FG_COLOR = encode_color(0xCC, 0xCC, 0xCC);
        }
        HAVE_DISPLAY = true;
        INITIALIZED = true;
    }
    true
}

/// Unmap the framebuffer, however many pages that is.
///
/// `sys_munmap` takes at most 256 pages a call and a screenful is four times
/// that, so this loops. Leaving it mapped would mean holding a window onto the
/// screen after the right to do so had been revoked — revocation governs the
/// right to map, not mappings that already exist.
unsafe fn unmap_framebuffer(bytes: usize) {
    const MUNMAP_MAX: usize = 256;
    let pages = (bytes + 4095) / 4096;
    let mut done = 0;
    while done < pages {
        let chunk = (pages - done).min(MUNMAP_MAX);
        let _ = syscall::sys_munmap(FB_VADDR + done * 4096, chunk);
        done += chunk;
    }
}

/// Redraw every cell. Used when the display comes back from a compositor:
/// what is on the screen is whatever that left there.
fn redraw_all() {
    if !unsafe { HAVE_DISPLAY } {
        return;
    }
    unsafe {
        core::ptr::write_bytes(FB as *mut u8, 0, PITCH * HEIGHT);
        DIRTY_MIN = 0;
        DIRTY_MAX = ROWS.saturating_sub(1);
    }
    flush_dirty();
}

/// Answer the framebuffer device when the display changes hands.
///
/// Polled rather than waited for, since the main loop's real job is draining
/// the pipe. A zero timeout is a poll: nothing to collect, nothing lost.
fn poll_display_handover() {
    let mut msg = Message::empty();
    if syscall::sys_recv_timeout(TID_ANY, &mut msg, 0).is_err() {
        return;
    }
    match msg.tag {
        TAG_FB_LOST => {
            // Stop drawing before answering: the reply is what lets the new
            // owner start, and two programs writing the same pixels is the
            // thing this protocol exists to prevent.
            unsafe {
                HAVE_DISPLAY = false;
                unmap_framebuffer(PITCH * HEIGHT);
            }
            // Drop the capability along with the mapping. A granted slot must
            // be empty to be granted into again, so keeping a revoked one
            // means the display can never be handed back.
            let _ = syscall::sys_cap_delete(FB_LEASE_SLOT);
            let ack = Message { sender: 0, tag: 0, data: [0; 6] };
            let _ = syscall::sys_reply(msg.sender, &ack);
        }
        TAG_FB_GAINED => {
            let ok = adopt_mode(&msg);
            let ack = Message { sender: 0, tag: 0, data: [0; 6] };
            let _ = syscall::sys_reply(msg.sender, &ack);
            if ok {
                redraw_all();
            }
        }
        _ => {
            let ack = Message { sender: 0, tag: u64::MAX, data: [0; 6] };
            let _ = syscall::sys_reply(msg.sender, &ack);
        }
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
    if !unsafe { HAVE_DISPLAY } {
        return;
    }
    let glyph = &quark_rt::font::FONT[ch as usize];

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

/// Move everything up one line.
///
/// The pixel half is skipped when the display belongs to somebody else, but
/// the cell buffer is not: that is the text, and it is what the screen is
/// redrawn from when the display comes back.
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

        // Scroll framebuffer pixels up by one text row instead of a full
        // redraw, so pre-existing content (e.g. kernel boot text) is preserved.
        if HAVE_DISPLAY {
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
        }

        ROW = ROWS - 1;
    }
}

fn flush_dirty() {
    unsafe {
        if !INITIALIZED || !HAVE_DISPLAY || DIRTY_MIN > DIRTY_MAX {
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
    if !INITIALIZED || !HAVE_DISPLAY { return; }
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
    if !HAVE_DISPLAY {
        return;
    }
    if !INITIALIZED { return; }
    if COL < COLS && ROW < ROWS {
        let idx = cell_idx(COL, ROW);
        draw_glyph(COL, ROW, CELL_CH[idx], CELL_FG[idx]);
    }
}

/// Draw a solid underline cursor (bottom 2 rows of the glyph cell).
unsafe fn draw_cursor_block(color: u32) {
    if !HAVE_DISPLAY {
        return;
    }
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

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[console] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
