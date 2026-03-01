const WIDTH: usize = 80;
const HEIGHT: usize = 25;
const WHITE_ON_BLACK: u8 = 0x0F;

static mut BASE: usize = 0xB8000;
static mut COL: usize = 0;
static mut ROW: usize = 0;

pub unsafe fn init(addr: u64, _width: u32, _height: u32) {
    BASE = addr as usize;
    COL = 0;
    ROW = 0;
}

pub fn clear() {
    unsafe {
        let buf = BASE as *mut u8;
        for i in 0..(WIDTH * HEIGHT) {
            buf.add(i * 2).write_volatile(b' ');
            buf.add(i * 2 + 1).write_volatile(WHITE_ON_BLACK);
        }
        COL = 0;
        ROW = 0;
    }
}

pub fn puts(s: &[u8]) {
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
                let offset = (ROW * WIDTH + COL) * 2;
                let buf = BASE as *mut u8;
                buf.add(offset).write_volatile(byte);
                buf.add(offset + 1).write_volatile(WHITE_ON_BLACK);
                COL += 1;
                if COL >= WIDTH {
                    COL = 0;
                    ROW += 1;
                }
            }
        }
        if ROW >= HEIGHT {
            scroll();
        }
    }
}

fn scroll() {
    unsafe {
        let buf = BASE as *mut u8;
        for row in 1..HEIGHT {
            for col in 0..WIDTH {
                let src = (row * WIDTH + col) * 2;
                let dst = ((row - 1) * WIDTH + col) * 2;
                buf.add(dst).write_volatile(buf.add(src).read_volatile());
                buf.add(dst + 1).write_volatile(buf.add(src + 1).read_volatile());
            }
        }
        for col in 0..WIDTH {
            let offset = ((HEIGHT - 1) * WIDTH + col) * 2;
            buf.add(offset).write_volatile(b' ');
            buf.add(offset + 1).write_volatile(WHITE_ON_BLACK);
        }
        ROW = HEIGHT - 1;
    }
}
