#![no_std]
#![no_main]

use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::nameserver;
use quark_rt::{println, syscall};

// PS/2 controller data and status ports, and the keyboard interrupt line.
quark_rt::manifest!([
    quark_rt::manifest::CapReq::priority(quark_rt::syscall::PRIO_DRIVER),
    quark_rt::manifest::CapReq::ioport(0x60, 0x64),
    quark_rt::manifest::CapReq::irq(1),
    quark_rt::manifest::CapReq::irq(12),
]);

// Keyboard IPC tags
const TAG_GET_KEY: u64 = 1;
const TAG_KEY_EVENT: u64 = 2;
const TAG_NO_KEY: u64 = 3;
const TAG_REGISTER_SIGINT: u64 = 4;
/// Take a key if one is waiting, but do not wait for one.
///
/// [`TAG_GET_KEY`] parks the caller until something is typed, which is right
/// for a line discipline and wrong for anything with a screen to redraw. A
/// server that has to stay answerable — the input server while a compositor
/// holds the keyboard — asks this instead and gets [`TAG_NO_KEY`] when there
/// is nothing.
///
/// A keyboard driver that predates this answers `TAG_NO_KEY` through its
/// default arm, so the failure is "no keys ever" rather than a caller that
/// hangs on an unrecognised tag.
const TAG_GET_KEY_NB: u64 = 5;
/// Take a pointer movement if one is waiting, but do not wait for one.
///
/// `data[0] = dx`, `data[1] = dy` as signed values widened to u64, and
/// `data[2] = buttons`, bit 0 left, bit 1 right, bit 2 middle. Answered with
/// [`TAG_NO_KEY`] when there is nothing, so a caller that asks a driver
/// predating the mouse gets "no movement ever" rather than hanging.
const TAG_GET_MOUSE_NB: u64 = 6;
const TAG_MOUSE_EVENT: u64 = 7;

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

/// The i8042's status port bits this driver acts on.
///
/// Bit 0 says a byte is waiting to be read. Bit 1 says the controller has not
/// yet taken the last byte written to it. Bit 5 says the waiting byte came from
/// the *auxiliary* device — the mouse — rather than the keyboard.
///
/// That last bit is why there is one driver here and not two. A PS/2 mouse is
/// not a second device with a second port: it is the same controller answering
/// on the same data port, and two tasks reading 0x60 would take each other's
/// bytes. Which interrupt fired is not the answer either, because a byte for
/// one device can be waiting when the other's interrupt arrives.
const STATUS_OUTPUT_FULL: u64 = 1 << 0;
const STATUS_INPUT_FULL: u64 = 1 << 1;
const STATUS_FROM_MOUSE: u64 = 1 << 5;

const PORT_DATA: u16 = 0x60;
const PORT_STATUS: u16 = 0x64;
const PORT_CMD: u16 = 0x64;

/// Controller commands.
const CMD_ENABLE_AUX: u8 = 0xA8;
const CMD_READ_CONFIG: u8 = 0x20;
const CMD_WRITE_CONFIG: u8 = 0x60;
/// "The next byte written to the data port is for the mouse, not the keyboard."
const CMD_TO_MOUSE: u8 = 0xD4;

/// Mouse commands, and the byte it answers them with.
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_REPORTING: u8 = 0xF4;
const MOUSE_ACK: u8 = 0xFA;

/// Configuration byte bits: bit 1 lets the auxiliary device raise IRQ 12, and
/// bit 5 *disables* its clock, so it has to be cleared.
const CONFIG_AUX_IRQ: u8 = 1 << 1;
const CONFIG_AUX_CLOCK_OFF: u8 = 1 << 5;

/// One movement, as the compositor will want it.
#[derive(Clone, Copy)]
struct MouseEvent {
    dx: i32,
    dy: i32,
    buttons: u8,
}

const MOUSE_BUF_SIZE: usize = 32;

/// Movements waiting to be collected.
///
/// Coalescing would be wrong here even though it is tempting: a click at the
/// end of a fast movement must not arrive at a position the pointer had already
/// left, and only the consumer knows which movements it can afford to merge.
struct MouseBuffer {
    buf: [MouseEvent; MOUSE_BUF_SIZE],
    head: usize,
    tail: usize,
}

impl MouseBuffer {
    const fn new() -> Self {
        const EMPTY: MouseEvent = MouseEvent { dx: 0, dy: 0, buttons: 0 };
        MouseBuffer { buf: [EMPTY; MOUSE_BUF_SIZE], head: 0, tail: 0 }
    }

    fn push(&mut self, ev: MouseEvent) {
        let next = (self.head + 1) % MOUSE_BUF_SIZE;
        if next != self.tail {
            self.buf[self.head] = ev;
            self.head = next;
        }
    }

    fn pop(&mut self) -> Option<MouseEvent> {
        if self.head == self.tail {
            return None;
        }
        let ev = self.buf[self.tail];
        self.tail = (self.tail + 1) % MOUSE_BUF_SIZE;
        Some(ev)
    }
}

/// Assembles the controller's three-byte packets.
///
/// Bit 3 of the first byte is always set, which is the only synchronisation
/// signal there is: a stream that has lost its place is found by a first byte
/// without it, and skipping that byte is how it is recovered.
struct MouseDecoder {
    bytes: [u8; 3],
    have: usize,
}

impl MouseDecoder {
    const fn new() -> Self {
        MouseDecoder { bytes: [0; 3], have: 0 }
    }

    fn feed(&mut self, b: u8) -> Option<MouseEvent> {
        if self.have == 0 && b & 0x08 == 0 {
            return None; // out of step; this cannot be a first byte
        }
        self.bytes[self.have] = b;
        self.have += 1;
        if self.have < 3 {
            return None;
        }
        self.have = 0;
        let flags = self.bytes[0];
        // Overflow means the controller gave up counting, and the magnitude it
        // reports is meaningless. Dropping the movement is better than jumping
        // the pointer across the screen; the buttons in it are still current
        // and are reported with no movement.
        let (dx, dy) = if flags & 0xC0 != 0 {
            (0, 0)
        } else {
            (sign_extend(self.bytes[1], flags & 0x10 != 0),
             sign_extend(self.bytes[2], flags & 0x20 != 0))
        };
        Some(MouseEvent {
            dx,
            // The mouse counts upwards and the screen counts downwards.
            dy: -dy,
            buttons: flags & 0x07,
        })
    }
}

/// A nine-bit signed quantity, split across a byte and a sign bit in the flags.
fn sign_extend(value: u8, negative: bool) -> i32 {
    if negative { value as i32 - 256 } else { value as i32 }
}

/// Wait for the controller to take what was last written to it.
///
/// Bounded: a controller that never clears the bit must not become a driver
/// that never returns, and on a machine with no mouse that is exactly what
/// would happen during start-up.
fn wait_writable() -> bool {
    for _ in 0..100_000 {
        if syscall::sys_ioport_read(PORT_STATUS) & STATUS_INPUT_FULL == 0 {
            return true;
        }
    }
    false
}

fn wait_readable() -> bool {
    for _ in 0..100_000 {
        if syscall::sys_ioport_read(PORT_STATUS) & STATUS_OUTPUT_FULL != 0 {
            return true;
        }
    }
    false
}

fn command(byte: u8) {
    if wait_writable() {
        syscall::sys_ioport_write(PORT_CMD, byte);
    }
}

/// Send a byte to the mouse and collect its acknowledgement.
fn mouse_command(byte: u8) -> bool {
    command(CMD_TO_MOUSE);
    if !wait_writable() {
        return false;
    }
    syscall::sys_ioport_write(PORT_DATA, byte);
    if !wait_readable() {
        return false;
    }
    syscall::sys_ioport_read(PORT_DATA) as u8 == MOUSE_ACK
}

/// Turn the auxiliary device on and ask it to report.
///
/// Returns whether there is a mouse. A machine without one is not an error —
/// it is most machines this has ever run on — so the failure is quiet and the
/// driver carries on being a keyboard.
fn enable_mouse() -> bool {
    command(CMD_ENABLE_AUX);

    command(CMD_READ_CONFIG);
    if !wait_readable() {
        return false;
    }
    let mut config = syscall::sys_ioport_read(PORT_DATA) as u8;
    config |= CONFIG_AUX_IRQ;
    config &= !CONFIG_AUX_CLOCK_OFF;
    command(CMD_WRITE_CONFIG);
    if !wait_writable() {
        return false;
    }
    syscall::sys_ioport_write(PORT_DATA, config);

    // Defaults first, so that whatever the firmware left behind — a different
    // sample rate, a different resolution — is not inherited.
    mouse_command(MOUSE_SET_DEFAULTS) && mouse_command(MOUSE_ENABLE_REPORTING)
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
    // And IRQ 12, the same controller's other device. Registered before the
    // mouse is enabled, so that the first packet it sends has somewhere to go.
    let mouse_irq = syscall::sys_irq_register(12).is_ok();
    if !mouse_irq {
        println!("[keyboard] No IRQ 12; the mouse will not be heard.");
    }

    // Register with nameserver as "keyboard"
    if nameserver::register(b"keyboard").is_ok() {
        println!("[keyboard] Registered with nameserver.");
    } else {
        println!("[keyboard] Failed to register with nameserver.");
    }

    let mut keybuf = KeyBuffer::new();
    let mut modifiers: u8 = 0;
    let mut extended = false;
    let mut waiting_client: Option<usize> = None;
    let mut sigint_tid: usize = 0;

    let mut mousebuf = MouseBuffer::new();
    let mut mouse = MouseDecoder::new();
    let have_mouse = mouse_irq && enable_mouse();
    if have_mouse {
        println!("[keyboard] Mouse enabled on the auxiliary port.");
    } else {
        println!("[keyboard] No mouse; keys only.");
    }

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        if msg.sender == 0 {
            // Take everything the controller has, not one byte per
            // notification. The kernel's per-IRQ queue is eight deep and drops
            // silently when it is full, so a burst of typing on a busy machine
            // loses notifications — and with one byte each, that lost every
            // keystroke behind them. Draining means a lost notification costs
            // nothing: the next wake-up collects what the last one left.
            //
            // Bit 0 of the status port says there is a byte waiting, and it is
            // asked before the first read as well as before each one after it.
            // The drain takes everything the controller has, so when two
            // notifications are queued the first one's loop consumes both
            // bytes and the second arrives to an empty buffer — and reading
            // 0x60 when it is empty hands back the last byte again. That is
            // one duplicated key per burst of typing: invisible in a shell
            // that echoes, and obvious to a Wayland client counting presses
            // against releases.
            //
            // Which device a byte came from is read from the status, not from
            // which interrupt fired: a keyboard byte can be waiting when IRQ 12
            // arrives, and feeding one to the packet decoder loses the mouse's
            // place in its three-byte stream.
            let mut budget = 64;
            loop {
                let status = syscall::sys_ioport_read(PORT_STATUS);
                if status & STATUS_OUTPUT_FULL == 0 {
                    break;
                }
                let raw = syscall::sys_ioport_read(PORT_DATA) as u8;
                if status & STATUS_FROM_MOUSE != 0 {
                    if let Some(ev) = mouse.feed(raw) {
                        mousebuf.push(ev);
                    }
                } else {
                    handle_scancode(
                        raw,
                        &mut extended,
                        &mut modifiers,
                        &mut keybuf,
                        sigint_tid,
                        &mut waiting_client,
                    );
                }
                budget -= 1;
                // The bound is in case a controller lies about bit 0, so a
                // wedged keyboard cannot become a wedged system.
                if budget == 0 {
                    break;
                }
            }
            // Both lines, because one notification can carry bytes for either
            // device and an unacknowledged line stops delivering for good.
            syscall::sys_irq_ack(1);
            if have_mouse {
                syscall::sys_irq_ack(12);
            }
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
                TAG_GET_KEY_NB => {
                    let reply = match keybuf.pop() {
                        Some(ev) => make_key_reply(&ev),
                        None => Message { sender: 0, tag: TAG_NO_KEY, data: [0; 6] },
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                }
                TAG_GET_MOUSE_NB => {
                    let reply = match mousebuf.pop() {
                        Some(ev) => Message {
                            sender: 0,
                            tag: TAG_MOUSE_EVENT,
                            data: [
                                ev.dx as i64 as u64,
                                ev.dy as i64 as u64,
                                ev.buttons as u64,
                                0,
                                0,
                                0,
                            ],
                        },
                        None => Message { sender: 0, tag: TAG_NO_KEY, data: [0; 6] },
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                }
                TAG_REGISTER_SIGINT => {
                    sigint_tid = msg.sender;
                    let reply = Message {
                        sender: 0,
                        tag: 0,
                        data: [0; 6],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
                }
                quark_rt::ipc::TAG_PING => {
                    // Liveness probe: reply immediately, do nothing else.
                    let reply = Message {
                        sender: 0,
                        tag: quark_rt::ipc::TAG_PING,
                        data: [0; 6],
                    };
                    let _ = syscall::sys_reply(msg.sender, &reply);
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

/// Translate one scancode and deliver or buffer the event it makes.
fn handle_scancode(
    raw: u8,
    extended: &mut bool,
    modifiers: &mut u8,
    keybuf: &mut KeyBuffer,
    sigint_tid: usize,
    waiting_client: &mut Option<usize>,
) {
    if raw == 0xE0 {
        *extended = true;
        return;
    }
    if *extended {
        // Ignore extended scancodes for now
        *extended = false;
        return;
    }

    let press = raw & 0x80 == 0;
    let scancode = raw & 0x7F;

    // Update modifier state
    match scancode {
        SC_LSHIFT | SC_RSHIFT => {
            if press {
                *modifiers |= MOD_SHIFT;
            } else {
                *modifiers &= !MOD_SHIFT;
            }
        }
        SC_LCTRL => {
            if press {
                *modifiers |= MOD_CTRL;
            } else {
                *modifiers &= !MOD_CTRL;
            }
        }
        SC_LALT => {
            if press {
                *modifiers |= MOD_ALT;
            } else {
                *modifiers &= !MOD_ALT;
            }
        }
        SC_CAPSLOCK => {
            if press {
                *modifiers ^= MOD_CAPSLOCK;
            }
        }
        _ => {}
    }

    // Translate to ASCII
    let use_shifted = (*modifiers & MOD_SHIFT != 0) ^ (*modifiers & MOD_CAPSLOCK != 0);
    let mut ascii = if use_shifted {
        SCANCODE_SHIFTED[scancode as usize]
    } else {
        SCANCODE_UNSHIFTED[scancode as usize]
    };

    // Ctrl transformation: Ctrl+letter produces 0x01-0x1A
    if *modifiers & MOD_CTRL != 0 && ascii.is_ascii_lowercase() {
        ascii &= 0x1F;
    }

    // Notify input server on Ctrl+C key press
    if press && ascii == 0x03 && sigint_tid != 0 {
        let _ = syscall::sys_notify(sigint_tid, 1);
    }

    let ev = KeyEvent { press, ascii, scancode, modifiers: *modifiers };

    // If a client is blocked waiting, reply immediately
    if press {
        if let Some(client_tid) = waiting_client.take() {
            let reply = make_key_reply(&ev);
            let _ = syscall::sys_reply(client_tid, &reply);
            return;
        }
    }

    keybuf.push(ev);
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

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[keyboard] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
