#![no_std]
#![no_main]

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

// Nameserver well-known TID (init spawns nameserver first to guarantee this)
const NAMESERVER_TID: usize = 2;

// Nameserver protocol tags
const TAG_NS_REGISTER: u64 = 1;

// Keyboard IPC tags
const TAG_GET_KEY: u64 = 1;
const TAG_KEY_EVENT: u64 = 2;
const TAG_NO_KEY: u64 = 3;

// Key event types
const KEY_PRESS: u64 = 1;
const KEY_RELEASE: u64 = 2;

// Modifier flags
const MOD_SHIFT: u8 = 1 << 0;
const MOD_CTRL: u8 = 1 << 1;
const MOD_ALT: u8 = 1 << 2;
const MOD_CAPSLOCK: u8 = 1 << 3;

// Ring buffer for key events
const KEY_BUF_SIZE: usize = 64;

struct KeyEvent {
    press: bool,
    ascii: u8,
    scancode: u8,
    modifiers: u8,
}

struct KeyBuffer {
    buf: [KeyEvent; KEY_BUF_SIZE],
    head: usize,
    tail: usize,
}

impl KeyBuffer {
    const fn new() -> Self {
        const EMPTY: KeyEvent = KeyEvent {
            press: false,
            ascii: 0,
            scancode: 0,
            modifiers: 0,
        };
        KeyBuffer {
            buf: [EMPTY; KEY_BUF_SIZE],
            head: 0,
            tail: 0,
        }
    }

    fn push(&mut self, ev: KeyEvent) {
        let next = (self.head + 1) % KEY_BUF_SIZE;
        if next != self.tail {
            self.buf[self.head] = ev;
            self.head = next;
        }
        // Drop if full
    }

    fn pop(&mut self) -> Option<KeyEvent> {
        if self.head == self.tail {
            return None;
        }
        let ev = KeyEvent {
            press: self.buf[self.tail].press,
            ascii: self.buf[self.tail].ascii,
            scancode: self.buf[self.tail].scancode,
            modifiers: self.buf[self.tail].modifiers,
        };
        self.tail = (self.tail + 1) % KEY_BUF_SIZE;
        Some(ev)
    }
}

// Scancode set 1 tables (index = scancode, value = ASCII)
// Only the lower 128 entries (make codes); break code = make | 0x80
#[rustfmt::skip]
static SCANCODE_UNSHIFTED: [u8; 128] = [
    0,  27, b'1',b'2',b'3',b'4',b'5',b'6',b'7',b'8',b'9',b'0',b'-',b'=', 8,  9,   // 0x00-0x0F
    b'q',b'w',b'e',b'r',b't',b'y',b'u',b'i',b'o',b'p',b'[',b']', 10,  0, b'a',b's', // 0x10-0x1F
    b'd',b'f',b'g',b'h',b'j',b'k',b'l',b';',b'\'',b'`', 0, b'\\',b'z',b'x',b'c',b'v', // 0x20-0x2F
    b'b',b'n',b'm',b',',b'.',b'/', 0, b'*', 0, b' ', 0,  0,  0,  0,  0,  0,   // 0x30-0x3F
    0,   0,  0,  0,  0,  0,  0,  b'7',b'8',b'9',b'-',b'4',b'5',b'6',b'+',b'1', // 0x40-0x4F
    b'2',b'3',b'0',b'.', 0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,   // 0x50-0x5F
    0,   0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,       // 0x60-0x6F
    0,   0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,       // 0x70-0x7F
];

#[rustfmt::skip]
static SCANCODE_SHIFTED: [u8; 128] = [
    0,  27, b'!',b'@',b'#',b'$',b'%',b'^',b'&',b'*',b'(',b')',b'_',b'+', 8,  9,   // 0x00-0x0F
    b'Q',b'W',b'E',b'R',b'T',b'Y',b'U',b'I',b'O',b'P',b'{',b'}', 10,  0, b'A',b'S', // 0x10-0x1F
    b'D',b'F',b'G',b'H',b'J',b'K',b'L',b':',b'"',b'~', 0, b'|',b'Z',b'X',b'C',b'V', // 0x20-0x2F
    b'B',b'N',b'M',b'<',b'>',b'?', 0, b'*', 0, b' ', 0,  0,  0,  0,  0,  0,   // 0x30-0x3F
    0,   0,  0,  0,  0,  0,  0,  b'7',b'8',b'9',b'-',b'4',b'5',b'6',b'+',b'1', // 0x40-0x4F
    b'2',b'3',b'0',b'.', 0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,   // 0x50-0x5F
    0,   0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,       // 0x60-0x6F
    0,   0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,       // 0x70-0x7F
];

// Scancodes for modifier keys
const SC_LSHIFT: u8 = 0x2A;
const SC_RSHIFT: u8 = 0x36;
const SC_LCTRL: u8 = 0x1D;
const SC_LALT: u8 = 0x38;
const SC_CAPSLOCK: u8 = 0x3A;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[keyboard] Started.");

    // Register for IRQ 1 (keyboard)
    if syscall::sys_irq_register(1).is_err() {
        println!("[keyboard] Failed to register IRQ 1!");
        syscall::sys_exit();
    }

    // Register with nameserver as "keyboard"
    register_with_nameserver();

    let mut keybuf = KeyBuffer::new();
    let mut modifiers: u8 = 0;
    let mut extended = false;
    let mut waiting_client: Option<usize> = None;

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        if msg.sender == 0 {
            // IRQ notification from kernel — read scancode
            let raw = syscall::sys_ioport_read(0x60) as u8;
            syscall::sys_irq_ack(1);

            if raw == 0xE0 {
                extended = true;
                continue;
            }

            if extended {
                // Ignore extended scancodes for now
                extended = false;
                continue;
            }

            let press = raw & 0x80 == 0;
            let scancode = raw & 0x7F;

            // Update modifier state
            match scancode {
                SC_LSHIFT | SC_RSHIFT => {
                    if press {
                        modifiers |= MOD_SHIFT;
                    } else {
                        modifiers &= !MOD_SHIFT;
                    }
                }
                SC_LCTRL => {
                    if press {
                        modifiers |= MOD_CTRL;
                    } else {
                        modifiers &= !MOD_CTRL;
                    }
                }
                SC_LALT => {
                    if press {
                        modifiers |= MOD_ALT;
                    } else {
                        modifiers &= !MOD_ALT;
                    }
                }
                SC_CAPSLOCK => {
                    if press {
                        modifiers ^= MOD_CAPSLOCK;
                    }
                }
                _ => {}
            }

            // Translate to ASCII
            let use_shifted = (modifiers & MOD_SHIFT != 0) ^ (modifiers & MOD_CAPSLOCK != 0);
            let ascii = if use_shifted {
                SCANCODE_SHIFTED[scancode as usize]
            } else {
                SCANCODE_UNSHIFTED[scancode as usize]
            };

            let ev = KeyEvent {
                press,
                ascii,
                scancode,
                modifiers,
            };

            // If a client is blocked waiting, reply immediately
            if press {
                if let Some(client_tid) = waiting_client.take() {
                    let reply = make_key_reply(&ev);
                    let _ = syscall::sys_reply(client_tid, &reply);
                    continue;
                }
            }

            keybuf.push(ev);
        } else {
            // Client IPC request
            match msg.tag {
                TAG_GET_KEY => {
                    if let Some(ev) = keybuf.pop() {
                        let reply = make_key_reply(&ev);
                        let _ = syscall::sys_reply(msg.sender, &reply);
                    } else {
                        // No key available — save client to reply later
                        waiting_client = Some(msg.sender);
                    }
                }
                _ => {
                    let reply = Message {
                        sender: 0,
                        tag: TAG_NO_KEY,
                        data: [0; 6],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                }
            }
        }
    }
}

fn make_key_reply(ev: &KeyEvent) -> Message {
    Message {
        sender: 0,
        tag: TAG_KEY_EVENT,
        data: [
            if ev.press { KEY_PRESS } else { KEY_RELEASE },
            ev.ascii as u64,
            ev.scancode as u64,
            ev.modifiers as u64,
            0,
            0,
        ],
    }
}

fn register_with_nameserver() {
    // Encode "keyboard" as 3 x u64 (zero-padded)
    let name = b"keyboard";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
    let w1 = u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]);
    let w2 = u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]);

    let msg = Message {
        sender: 0,
        tag: TAG_NS_REGISTER,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
        println!("[keyboard] Registered with nameserver.");
    } else {
        println!("[keyboard] Failed to register with nameserver.");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[keyboard] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
