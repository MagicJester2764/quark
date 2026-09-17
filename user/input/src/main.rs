#![no_std]
#![no_main]

//! The keyboard, as a device.
//!
//! Two clients want two different things from it. A shell wants *lines*: with
//! echo, with backspace, delivered when Enter is pressed. A compositor wants
//! *keys*: every one of them, as they happen, with no interpretation at all.
//! Both are served from here, because both cannot be served at once — there is
//! one keyboard, and whatever is reading it takes keys out of everyone else's
//! reach.
//!
//! So the raw side is a claim, the way the display is:
//!
//! ```text
//!     wm ---- CLAIM ----->  input        line readers wait
//!     wm ---- POLL ------>  input ---->  keyboard (non-blocking)
//!        <--- key -------
//!     wm ---- RELEASE --->  input        deferred readers are served
//! ```
//!
//! A claimant polls rather than being pushed to, because pushing needs an
//! Endpoint capability aimed at the claimant and this server has no way to
//! mint one for a program that did not exist when it started. Answering a
//! caller needs no capability at all, which is what makes the pull the shape
//! that works.
//!
//! Claims stack, as the display's do: a compositor started inside another
//! takes the keys, and they go back to the outer one when it lets go. Line
//! readers are served when nobody at all holds the keyboard.

use quark_rt::ipc::{death_notice, Message, TAG_NOTIFICATION, TID_ANY};
use quark_rt::nameserver;
use quark_rt::{print, println, syscall};

use quark_rt::manifest::CapReq;

// Sets the foreground task so Ctrl-C reaches the right one.
quark_rt::manifest!([
    CapReq::priority(quark_rt::syscall::PRIO_SERVER),
    CapReq::task_mgmt(0),
]);

// Keyboard protocol (client side)
const TAG_GET_KEY: u64 = 1;
const TAG_KEY_EVENT: u64 = 2;
const TAG_GET_KEY_NB: u64 = 5;

// Input server protocol (serving readers)
const TAG_READ: u64 = 1;
const TAG_SET_FOREGROUND: u64 = 2;

// Keyboard registration
const TAG_REGISTER_SIGINT: u64 = 4;
/// The keyboard driver's own tags for the pointer half of its controller.
const TAG_GET_MOUSE_NB: u64 = 6;
const TAG_MOUSE_EVENT: u64 = 7;

/// Take the keyboard: raw key events, no line discipline, until released.
///
/// Numbered well clear of everything above. These arrive at the claimant's own
/// receive loop alongside whatever protocol it already speaks, and a collision
/// there would be silent.
const TAG_INPUT_CLAIM: u64 = 0x200;
/// Give it back.
const TAG_INPUT_RELEASE: u64 = 0x201;
/// Is there a key? Answers [`TAG_INPUT_KEY`] or [`TAG_INPUT_NONE`], now.
const TAG_INPUT_POLL: u64 = 0x202;
/// `data[0] = press`, `data[1] = ascii`, `data[2] = scancode`,
/// `data[3] = modifiers`.
const TAG_INPUT_KEY: u64 = 0x203;
/// Nothing typed.
const TAG_INPUT_NONE: u64 = 0x204;
/// Has the pointer moved? Answers [`TAG_INPUT_MOUSE`] or [`TAG_INPUT_NONE`].
///
/// Behind the same claim as the keys, because they are the same device as far
/// as this system is concerned: whoever owns the screen owns the input, and a
/// pointer delivered to somebody other than the holder of the display would be
/// clicking on windows it cannot see.
const TAG_INPUT_POLL_MOUSE: u64 = 0x205;
/// `data[0] = dx`, `data[1] = dy` as signed values, `data[2] = buttons`.
const TAG_INPUT_MOUSE: u64 = 0x206;

const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

const KEY_PRESS: u64 = 1;

const LINE_BUF_SIZE: usize = 256;

/// How many line readers can be waiting on the keyboard coming back.
///
/// One is the number that occurs: there is a console and a shell on it. The
/// rest is so that a second reader waits its turn rather than being stranded.
const MAX_DEFERRED: usize = 4;

/// A line that was finished but not yet all handed over.
///
/// A read gets at most forty bytes, one message's worth, and the rest waits
/// here for the next read, which is how a terminal hands a long line to a
/// short read. Before this, whatever did not fit in the first forty bytes was
/// thrown away: an eighty-character command ran as its first forty.
struct Pending {
    buf: [u8; LINE_BUF_SIZE],
    len: usize,
    at: usize,
}

impl Pending {
    const fn new() -> Self {
        Pending { buf: [0; LINE_BUF_SIZE], len: 0, at: 0 }
    }

    /// The next piece of the line, if any is left.
    fn take(&mut self, max: usize) -> Option<Message> {
        if self.at >= self.len {
            return None;
        }
        let n = (self.len - self.at).min(max);
        let reply = pack_read_reply(&self.buf[self.at..], n);
        self.at += n;
        Some(reply)
    }

    fn clear(&mut self) {
        self.len = 0;
        self.at = 0;
    }
}

/// Answer a reader: from what is left of the last line, or with a new one.
fn answer_reader(
    kbd_tid: usize,
    reader_tid: usize,
    max_bytes: usize,
    pending: &mut Pending,
    line_buf: &mut [u8; LINE_BUF_SIZE],
    line_len: &mut usize,
    foreground_tid: &mut usize,
) {
    match pending.take(max_bytes) {
        Some(reply) => {
            let _ = syscall::sys_reply(reader_tid, &reply);
        }
        None => serve_read(kbd_tid, reader_tid, max_bytes, pending, line_buf, line_len, foreground_tid),
    }
}

/// How many programs can hold the keyboard, one above another.
const MAX_CLAIMANTS: usize = 8;

/// Who holds the keyboard raw: oldest first, and the last one gets the keys.
struct Claims {
    tids: [usize; MAX_CLAIMANTS],
    depth: usize,
}

impl Claims {
    /// Who gets the keys, or 0 for the line readers.
    fn top(&self) -> usize {
        self.tids[..self.depth].last().copied().unwrap_or(0)
    }

    /// Put `tid` on top, out of wherever it was. False if there is no room.
    fn push(&mut self, tid: usize) -> bool {
        let _ = self.remove(tid);
        if self.depth == MAX_CLAIMANTS {
            return false;
        }
        self.tids[self.depth] = tid;
        self.depth += 1;
        true
    }

    /// Take `tid` out of the stack. Whether it had the keys, or `None` if it
    /// held no claim.
    fn remove(&mut self, tid: usize) -> Option<bool> {
        let i = self.tids[..self.depth].iter().position(|&t| t == tid)?;
        let top = i + 1 == self.depth;
        self.tids.copy_within(i + 1..self.depth, i);
        self.depth -= 1;
        Some(top)
    }
}

#[derive(Clone, Copy)]
struct KeyEvent {
    press: bool,
    ascii: u8,
    scancode: u8,
    modifiers: u8,
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[input] Started.");

    // Discover keyboard service
    let kbd_tid = match nameserver::lookup(b"keyboard") {
        Some(tid) => tid,
        None => {
            println!("[input] Keyboard service not found!");
            syscall::sys_exit();
        }
    };
    println!("[input] Found keyboard at TID {}", kbd_tid);

    // Register as "input" with nameserver
    if nameserver::register(b"input").is_ok() {
        println!("[input] Registered with nameserver.");
    }

    // Register with keyboard driver for Ctrl+C notifications
    register_sigint(kbd_tid);

    println!("[input] Ready.");

    let mut line_buf = [0u8; LINE_BUF_SIZE];
    let mut line_len: usize = 0;
    let mut foreground_tid: usize = 0;
    let mut pending = Pending::new();

    let mut claims = Claims { tids: [0; MAX_CLAIMANTS], depth: 0 };
    let mut deferred: [(usize, usize); MAX_DEFERRED] = [(0, 0); MAX_DEFERRED];
    let mut deferred_len: usize = 0;

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }
        let sender = msg.sender;

        // A claimant has died: out of the stack, and if nobody is left
        // holding the keyboard, the readers waiting on it are served. The
        // kernel is not waiting for an answer; the same tag from anybody else
        // is an unknown request.
        if let Some(dead) = death_notice(&msg) {
            if let Some(top) = claims.remove(dead) {
                if top {
                    println!("[input] tid {} died holding the keyboard", dead);
                    handed_down(kbd_tid, claims.top());
                }
                if claims.top() == 0 {
                    for &(tid, max) in &deferred[..deferred_len] {
                        answer_reader(
                            kbd_tid,
                            tid,
                            max,
                            &mut pending,
                            &mut line_buf,
                            &mut line_len,
                            &mut foreground_tid,
                        );
                    }
                    deferred_len = 0;
                }
            }
            continue;
        }

        match msg.tag {
            TAG_SET_FOREGROUND => {
                foreground_tid = msg.data[0] as usize;
                let _ = syscall::sys_reply(sender, &ok());
            }
            TAG_NOTIFICATION if sender == 0 => {
                // Ctrl+C from the keyboard driver, which sends it whether or
                // not anyone is reading. While the keyboard belongs to someone
                // else, ^C is theirs to interpret — it is sitting in the
                // driver's buffer and will be handed over with everything
                // else. Acting on it here would kill the compositor, which is
                // the foreground task, and it would die still holding the
                // display.
                if claims.top() == 0 {
                    handle_ctrl_c(&mut foreground_tid, &mut line_len);
                    pending.clear();
                }
            }

            TAG_INPUT_CLAIM => {
                let reply = if claims.top() == sender {
                    ok() // already theirs
                } else if claims.push(sender) {
                    // A claimant that dies without releasing would otherwise
                    // keep the keys from everybody below it, down to a
                    // console nobody can type at.
                    let _ = syscall::sys_task_watch(sender);
                    // Whatever was typed before the claim was typed at
                    // something else. Throw it away rather than delivering a
                    // shell command's tail to a compositor.
                    while get_key_nb(kbd_tid).is_some() {}
                    println!("[input] keyboard claimed by tid {}", sender);
                    ok()
                } else {
                    error()
                };
                let _ = syscall::sys_reply(sender, &reply);
            }

            TAG_INPUT_RELEASE => match claims.remove(sender) {
                None => {
                    let _ = syscall::sys_reply(sender, &error());
                }
                Some(top) => {
                    // Answer the releaser first: serving a deferred reader
                    // blocks in here until a whole line is typed, and the
                    // program giving the keyboard back is usually on its way
                    // out.
                    let _ = syscall::sys_reply(sender, &ok());
                    println!("[input] keyboard released by tid {}", sender);
                    if top {
                        handed_down(kbd_tid, claims.top());
                    }
                    if claims.top() == 0 {
                        for &(tid, max) in &deferred[..deferred_len] {
                            answer_reader(
                                kbd_tid,
                                tid,
                                max,
                                &mut pending,
                                &mut line_buf,
                                &mut line_len,
                                &mut foreground_tid,
                            );
                        }
                        deferred_len = 0;
                    }
                }
            },

            TAG_INPUT_POLL => {
                let reply = if claims.top() != sender {
                    error()
                } else {
                    match get_key_nb(kbd_tid) {
                        Some(ev) => Message {
                            sender: 0,
                            tag: TAG_INPUT_KEY,
                            data: [
                                if ev.press { 1 } else { 0 },
                                ev.ascii as u64,
                                ev.scancode as u64,
                                ev.modifiers as u64,
                                0,
                                0,
                            ],
                        },
                        None => Message { sender: 0, tag: TAG_INPUT_NONE, data: [0; 6] },
                    }
                };
                let _ = syscall::sys_reply(sender, &reply);
            }

            TAG_INPUT_POLL_MOUSE => {
                let reply = if claims.top() != sender {
                    error()
                } else {
                    let ask = Message { sender: 0, tag: TAG_GET_MOUSE_NB, data: [0; 6] };
                    let mut got = Message::empty();
                    match syscall::sys_call(kbd_tid, &ask, &mut got) {
                        Ok(()) if got.tag == TAG_MOUSE_EVENT => Message {
                            sender: 0,
                            tag: TAG_INPUT_MOUSE,
                            data: got.data,
                        },
                        _ => Message { sender: 0, tag: TAG_INPUT_NONE, data: [0; 6] },
                    }
                };
                let _ = syscall::sys_reply(sender, &reply);
            }

            TAG_READ => {
                let max_bytes = (msg.data[0] as usize).min(40);
                if claims.top() != 0 {
                    // Someone else has the keyboard. Hold the reader instead
                    // of answering it: reading here would take keys out of the
                    // owner's hands, and an empty answer would only bring the
                    // reader straight back.
                    if deferred_len < MAX_DEFERRED {
                        deferred[deferred_len] = (sender, max_bytes);
                        deferred_len += 1;
                    } else {
                        let _ = syscall::sys_reply(sender, &pack_read_reply(&line_buf, 0));
                    }
                } else {
                    answer_reader(
                        kbd_tid,
                        sender,
                        max_bytes,
                        &mut pending,
                        &mut line_buf,
                        &mut line_len,
                        &mut foreground_tid,
                    );
                }
            }

            quark_rt::ipc::TAG_PING => {
                // Liveness probe: reply immediately, do nothing else. Without
                // this arm the default drops the message and the caller waits
                // out its timeout against a perfectly healthy service.
                let reply = Message {
                    sender: 0,
                    tag: quark_rt::ipc::TAG_PING,
                    data: [0; 6],
                };
                let _ = syscall::sys_reply(sender, &reply);
            }
            _ => {
                let _ = syscall::sys_reply(sender, &error());
            }
        }
    }
}

/// The keyboard has gone back down the stack, to `tid`. What was typed before
/// now was typed at the program that let go, and is thrown away as a claim
/// throws away what was typed before it. Nothing to do for the line readers:
/// a line in progress is theirs.
fn handed_down(kbd_tid: usize, tid: usize) {
    if tid != 0 {
        while get_key_nb(kbd_tid).is_some() {}
        println!("[input] keyboard back with tid {}", tid);
    }
}

fn ok() -> Message {
    Message { sender: 0, tag: TAG_OK, data: [0; 6] }
}

fn error() -> Message {
    Message { sender: 0, tag: TAG_ERROR, data: [0; 6] }
}

/// Read a line for `reader_tid` and reply with it.
///
/// This blocks until Enter, so nothing else is served while it runs — a claim
/// arriving mid-line waits for the line to finish. That is the same
/// unresponsiveness the server has always had while reading, and the case it
/// matters for (a compositor launched from a shell prompt) cannot occur: the
/// shell's read has already been answered by the time it spawns anything.
fn serve_read(
    kbd_tid: usize,
    reader_tid: usize,
    max_bytes: usize,
    pending: &mut Pending,
    line_buf: &mut [u8; LINE_BUF_SIZE],
    line_len: &mut usize,
    foreground_tid: &mut usize,
) {
    loop {
        let ascii = get_key_blocking(kbd_tid);
        if ascii == 0 {
            continue;
        }

        match ascii {
            0x03 => {
                // Ctrl+C while reading
                print!("^C\n");
                *line_len = 0;
                if *foreground_tid != 0 {
                    let _ = syscall::sys_signal(*foreground_tid, syscall::SIG_INT);
                    *foreground_tid = 0;
                }
                // Reply with 0 bytes to unblock the reader
                pending.clear();
                let reply = pack_read_reply(line_buf, 0);
                let _ = syscall::sys_reply(reader_tid, &reply);
                return;
            }
            b'\n' | 13 => {
                // Newline — echo and deliver
                print!("\n");
                if *line_len < LINE_BUF_SIZE {
                    line_buf[*line_len] = b'\n';
                    *line_len += 1;
                }
                // The whole line waits in `pending`; this read gets its first
                // piece and later reads the rest.
                pending.buf[..*line_len].copy_from_slice(&line_buf[..*line_len]);
                pending.len = *line_len;
                pending.at = 0;
                *line_len = 0;
                if let Some(reply) = pending.take(max_bytes) {
                    let _ = syscall::sys_reply(reader_tid, &reply);
                }
                return;
            }
            8 | 127 => {
                // Backspace
                if *line_len > 0 {
                    *line_len -= 1;
                    print!("\x08 \x08");
                }
            }
            c if c >= 0x20 => {
                // Printable character
                if *line_len < LINE_BUF_SIZE - 1 {
                    line_buf[*line_len] = c;
                    *line_len += 1;
                    let ch = [c];
                    if let Ok(s) = core::str::from_utf8(&ch) {
                        print!("{}", s);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Get one key press from the keyboard driver (blocking).
/// Returns ASCII code, or 0 for non-printable/release events.
fn get_key_blocking(kbd_tid: usize) -> u8 {
    let msg = Message {
        sender: 0,
        tag: TAG_GET_KEY,
        data: [0; 6],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(kbd_tid, &msg, &mut reply).is_err() {
        return 0;
    }
    if reply.tag != TAG_KEY_EVENT {
        return 0;
    }
    // Only handle key presses
    if reply.data[0] != KEY_PRESS {
        return 0;
    }
    reply.data[1] as u8
}

/// Take a key from the driver if one is waiting, without blocking.
///
/// Releases come back too. The line discipline throws them away; a compositor's
/// client may well want them, and dropping them here would be this server
/// deciding that for everybody.
fn get_key_nb(kbd_tid: usize) -> Option<KeyEvent> {
    let msg = Message { sender: 0, tag: TAG_GET_KEY_NB, data: [0; 6] };
    let mut reply = Message::empty();
    // Timed for the same reason the compositor times this server: a driver
    // that stops answering should cost the keyboard, not everything holding
    // still behind it.
    match syscall::sys_call_timeout(kbd_tid, &msg, &mut reply, 20) {
        syscall::CallOutcome::Replied => {}
        syscall::CallOutcome::TimedOut => {
            println!("[input] keyboard driver did not answer");
            return None;
        }
        syscall::CallOutcome::Failed => return None,
    }
    if reply.tag != TAG_KEY_EVENT {
        return None;
    }
    Some(KeyEvent {
        press: reply.data[0] == KEY_PRESS,
        ascii: reply.data[1] as u8,
        scancode: reply.data[2] as u8,
        modifiers: reply.data[3] as u8,
    })
}

/// Pack a read reply: data[0] = byte count, data[1..6] = bytes
fn pack_read_reply(buf: &[u8], len: usize) -> Message {
    let mut data = [0u64; 6];
    data[0] = len as u64;
    for i in 0..5 {
        let base = i * 8;
        let mut w = [0u8; 8];
        for j in 0..8 {
            if base + j < len {
                w[j] = buf[base + j];
            }
        }
        data[i + 1] = u64::from_le_bytes(w);
    }
    Message {
        sender: 0,
        tag: TAG_READ,
        data,
    }
}

/// Ask the keyboard to say when Ctrl-C is pressed, which it does by calling
/// here — so the request carries the right to.
fn register_sigint(kbd_tid: usize) {
    let msg = Message {
        sender: 0,
        tag: TAG_REGISTER_SIGINT,
        data: [0; 6],
    };
    let mut reply = Message::empty();
    let _ = syscall::sys_call_offer_self(kbd_tid, &msg, &mut reply);
}

fn handle_ctrl_c(foreground_tid: &mut usize, line_len: &mut usize) {
    print!("^C\n");
    *line_len = 0;
    if *foreground_tid != 0 {
        let _ = syscall::sys_signal(*foreground_tid, syscall::SIG_INT);
        *foreground_tid = 0;
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[input] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
