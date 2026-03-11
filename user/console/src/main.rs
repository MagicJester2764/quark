#![no_std]
#![no_main]

use font8x8::legacy::BASIC_LEGACY;
use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;

// Init -> console: framebuffer initialization
const TAG_FB_INIT: u64 = 100;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

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

    // Main loop: read from pipe (fd 0) and render
    loop {
        let mut buf = [0u8; 256];
        let n = syscall::sys_fd_read(0, &mut buf);
        if n == 0 || n == u64::MAX {
            break; // EOF or error
        }
        write_bytes(&buf[..n as usize]);
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
                    draw_glyph(COL, ROW, byte);
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
        let fg = FG_COLOR;

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
                    let buf = FB as *mut u8;
                    let total = HEIGHT * PITCH;
                    for i in 0..total { buf.add(i).write_volatile(0); }
                    ROW = 0; COL = 0;
                }
            }
            b'K' => {
                if p0 == 0 {
                    for c in COL..COLS { draw_glyph(c, ROW, b' '); }
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
        let buf = FB as *mut u64;
        let row_bytes = GLYPH_H * PITCH;
        let text_area = ROWS * row_bytes;

        // Copy in u64 chunks (8x fewer memory accesses)
        let words = (text_area - row_bytes) / 8;
        let src = (FB + row_bytes) as *const u64;
        for i in 0..words {
            let val = src.add(i).read_volatile();
            buf.add(i).write_volatile(val);
        }

        let last_row_start = ((ROWS - 1) * row_bytes) / 8;
        let last_row_words = row_bytes / 8;
        for i in 0..last_row_words {
            buf.add(last_row_start + i).write_volatile(0);
        }

        ROW = ROWS - 1;
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
