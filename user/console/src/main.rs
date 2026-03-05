#![no_std]
#![no_main]

use font8x8::legacy::BASIC_LEGACY;
use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;

// Console protocol
const TAG_WRITE: u64 = 1;

// Init -> console: framebuffer initialization
const TAG_FB_INIT: u64 = 100;

const GLYPH_W: usize = 8;
const GLYPH_H: usize = 8;

// Max payload bytes per IPC message (5 data words × 8 bytes)
const MAX_WRITE_BYTES: usize = 40;

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

    // Main loop: serve write requests
    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        match msg.tag {
            TAG_WRITE => {
                let len = (msg.data[0] as usize).min(MAX_WRITE_BYTES);
                let bytes = data_to_bytes(&msg.data[1..6], len);
                write_bytes(&bytes[..len]);
                let reply = Message { sender: 0, tag: TAG_WRITE, data: [len as u64, 0, 0, 0, 0, 0] };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            _ => {
                let reply = Message { sender: 0, tag: u64::MAX, data: [0; 6] };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
        }
    }
}

fn init_framebuffer(msg: &Message) {
    // data[0] = physical address
    // data[1] = (width << 32) | height
    // data[2] = (pitch << 32) | bpp
    // data[3] = (red_pos << 16) | (green_pos << 8) | blue_pos
    let phys_addr = msg.data[0] as usize;
    let w = (msg.data[1] >> 32) as usize;
    let h = (msg.data[1] & 0xFFFF_FFFF) as usize;
    let pitch = (msg.data[2] >> 32) as usize;
    let bpp = (msg.data[2] & 0xFF) as usize;
    let rp = ((msg.data[3] >> 16) & 0xFF) as u8;
    let gp = ((msg.data[3] >> 8) & 0xFF) as u8;
    let bp = (msg.data[3] & 0xFF) as u8;

    // Map the framebuffer into our address space
    let fb_size = pitch * h;
    let pages = (fb_size + 4095) / 4096;
    let fb_vaddr: usize = 0x81_0000_0000;

    if syscall::sys_map_phys(phys_addr, fb_vaddr, pages).is_err() {
        println!("[console] Failed to map framebuffer!");
        return;
    }

    unsafe {
        FB = fb_vaddr;
        PITCH = pitch;
        WIDTH = w;
        HEIGHT = h;
        BPP = bpp;
        COLS = w / GLYPH_W;
        ROWS = h / GLYPH_H;
        COL = 0;
        ROW = 0;
        R_POS = rp;
        G_POS = gp;
        B_POS = bp;
        INITIALIZED = true;
    }
}

fn data_to_bytes(words: &[u64], len: usize) -> [u8; MAX_WRITE_BYTES] {
    let mut buf = [0u8; MAX_WRITE_BYTES];
    for (i, &w) in words.iter().enumerate() {
        let base = i * 8;
        let bytes = w.to_le_bytes();
        for j in 0..8 {
            if base + j < len {
                buf[base + j] = bytes[j];
            }
        }
    }
    buf
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
        let fg = encode_color(0xCC, 0xCC, 0xCC);

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

        for i in 0..(text_area - row_bytes) {
            let val = buf.add(i + row_bytes).read_volatile();
            buf.add(i).write_volatile(val);
        }

        let last_row_start = (ROWS - 1) * row_bytes;
        for i in 0..row_bytes {
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
