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
//!     wm ---- RELEASE --->  input        waiting readers are served
//! ```
//!
//! A claimant polls rather than being pushed to, because pushing needs an
//! Endpoint capability aimed at the claimant and this server has no way to
//! mint one for a program that did not exist when it started. Answering a
//! caller needs no capability at all, which is what makes the pull the shape
//! that works.
//!
//! Claims stack, as the display's do: a compositor started inside another
//! takes the keys, and they go back to the outer one when it lets go.
//!
//! Nobody holding the keyboard, keys are cooked as they are typed: the driver
//! says when one arrives, this takes everything waiting, echoes it and edits
//! the line, and a finished line waits here until somebody reads it. A reader
//! is answered when there is a line for it, and until then this server goes
//! on answering everybody else. It used to read the keyboard in a loop until
//! Enter, and every request in the meantime — a compositor's claim included —
//! waited for somebody to type.

use quark_rt::ipc::{death_notice, Message, TAG_NOTIFICATION, TAG_PING, TID_ANY};
use quark_rt::manifest::CapReq;
use quark_rt::nameserver;
use quark_rt::{print, println, syscall};

// Sets the foreground task so Ctrl-C reaches the right one.
quark_rt::manifest!([
    CapReq::priority(quark_rt::syscall::PRIO_SERVER),
    CapReq::task_mgmt(0),
]);

// Keyboard protocol (client side)
const TAG_KEY_EVENT: u64 = 2;
/// Take the keyboard driver, offering it the right to say when keys arrive.
/// Nobody else is answered by it afterwards.
const TAG_KBD_CLAIM: u64 = 4;
const TAG_GET_KEY_NB: u64 = 5;
/// The keyboard driver's own tags for the pointer half of its controller.
const TAG_GET_MOUSE_NB: u64 = 6;
const TAG_MOUSE_EVENT: u64 = 7;

// Input server protocol (serving readers)
const TAG_READ: u64 = 1;
/// `data[0]`: the task Ctrl-C interrupts — the caller or one of its
/// children — or 0 for none.
const TAG_SET_FOREGROUND: u64 = 2;

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
/// `data[0] = dx`, `data[1] = dy` as signed values, `data[2] = buttons`,
/// `data[3] = wheel` in detents, positive towards the user.
///
/// The driver's reply is passed on whole rather than copied field by field:
/// what a pointer packet carries is the driver's business, and a field added
/// there reaches a compositor without a change here.
const TAG_INPUT_MOUSE: u64 = 0x206;

const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

const KEY_PRESS: u64 = 1;

/// The longest line that can be typed, newline included.
const LINE_BUF_SIZE: usize = 256;
/// Finished lines waiting to be read.
const COOKED_SIZE: usize = 1024;
/// A read gets at most one message's worth.
const READ_MAX: usize = 40;
/// Readers waiting: one per task at most, so as many as there can be tasks.
const MAX_READERS: usize = 64;
/// How many programs can hold the keyboard, one above another.
const MAX_CLAIMANTS: usize = 8;
/// Keys taken from the driver in one go. More are taken on the next notice.
const DRAIN_MAX: usize = 1024;

/// Finished lines, oldest first: what a terminal calls its input queue. A
/// read takes at most one line of it, and a long line in as many reads as it
/// takes — before this, whatever did not fit in the first forty bytes was
/// thrown away, and an eighty-character command ran as its first forty.
struct Cooked {
    buf: [u8; COOKED_SIZE],
    len: usize,
}

impl Cooked {
    fn push(&mut self, line: &[u8]) -> bool {
        if self.len + line.len() > COOKED_SIZE {
            return false;
        }
        self.buf[self.len..self.len + line.len()].copy_from_slice(line);
        self.len += line.len();
        true
    }

    /// How much a read of at most `max` bytes takes: up to the end of the
    /// first line.
    fn next_read(&self, max: usize) -> usize {
        let line = self.buf[..self.len].iter().position(|&b| b == b'\n').map_or(self.len, |i| i + 1);
        line.min(max)
    }

    fn consume(&mut self, n: usize) {
        self.buf.copy_within(n..self.len, 0);
        self.len -= n;
    }
}

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

struct Server {
    kbd: usize,
    /// The line being typed.
    line: [u8; LINE_BUF_SIZE],
    line_len: usize,
    cooked: Cooked,
    /// Readers waiting for a line, oldest first, with how much each wants.
    readers: [(usize, usize); MAX_READERS],
    nreaders: usize,
    claims: Claims,
    /// Who Ctrl-C interrupts, and who said so.
    foreground: usize,
    foreground_setter: usize,
}

impl Server {
    /// A task is in one call at a time, so one that sends anything is no
    /// longer waiting for a line it asked for earlier.
    fn forget_reader(&mut self, tid: usize) {
        if let Some(i) = self.readers[..self.nreaders].iter().position(|r| r.0 == tid) {
            self.readers.copy_within(i + 1..self.nreaders, i);
            self.nreaders -= 1;
        }
    }

    fn pop_reader(&mut self) -> Option<(usize, usize)> {
        if self.nreaders == 0 {
            return None;
        }
        let first = self.readers[0];
        self.readers.copy_within(1..self.nreaders, 0);
        self.nreaders -= 1;
        Some(first)
    }

    /// Answer waiting readers from the finished lines, oldest first. A reader
    /// that has stopped waiting cannot be answered, and what it would have
    /// taken goes to the next.
    fn serve_readers(&mut self) {
        while self.claims.top() == 0 && self.cooked.len > 0 {
            let Some((tid, max)) = self.pop_reader() else { break };
            let n = self.cooked.next_read(max);
            if syscall::sys_reply(tid, &pack_read_reply(&self.cooked.buf[..n])).is_ok() {
                self.cooked.consume(n);
            }
        }
    }

    /// Take the keys the driver has and cook them, unless somebody holds the
    /// keyboard raw and takes them itself.
    fn keys_waiting(&mut self) {
        if self.claims.top() != 0 {
            return;
        }
        for _ in 0..DRAIN_MAX {
            let Some(ev) = get_key_nb(self.kbd) else { break };
            if ev.press {
                self.typed(ev.ascii);
            }
        }
        self.serve_readers();
    }

    /// The line discipline: one key typed.
    fn typed(&mut self, c: u8) {
        match c {
            0x03 => self.interrupt(),
            b'\n' | b'\r' => {
                print!("\n");
                self.line[self.line_len] = b'\n';
                // A full queue loses the line, as a full terminal does.
                let _ = self.cooked.push(&self.line[..self.line_len + 1]);
                self.line_len = 0;
            }
            8 | 127 => {
                if self.line_len > 0 {
                    self.line_len -= 1;
                    print!("\x08 \x08");
                }
            }
            c if c >= 0x20 => {
                // One byte short of the buffer: the newline needs room.
                if self.line_len < LINE_BUF_SIZE - 1 {
                    self.line[self.line_len] = c;
                    self.line_len += 1;
                    if let Ok(s) = core::str::from_utf8(&[c]) {
                        print!("{}", s);
                    }
                }
            }
            _ => {}
        }
    }

    /// Ctrl-C: the foreground task is interrupted, what was typed goes, and
    /// whoever is reading is answered with nothing and asks again.
    fn interrupt(&mut self) {
        print!("^C\n");
        self.line_len = 0;
        self.cooked.len = 0;
        if self.foreground != 0 {
            let _ = syscall::sys_signal(self.foreground, syscall::SIG_INT);
            self.foreground = 0;
        }
        if let Some((tid, _)) = self.pop_reader() {
            let _ = syscall::sys_reply(tid, &pack_read_reply(&[]));
        }
    }

    fn claim(&mut self, sender: usize) -> Message {
        if self.claims.top() == sender {
            return ok(); // already theirs
        }
        if !self.claims.push(sender) {
            return error();
        }
        // A claimant that dies without releasing would otherwise keep the
        // keys from everybody below it, down to a console nobody can type at.
        let _ = syscall::sys_task_watch(sender);
        // Whatever the driver still has was typed at something else. Throw it
        // away rather than delivering it to a compositor.
        flush(self.kbd);
        ok()
    }

    /// The keyboard has gone back down the stack. What the driver has was
    /// typed at the program that let go: a claimant below does not get it,
    /// and with nobody left it is cooked for the line readers, which is where
    /// typing goes when no program has the keys.
    fn handed_down(&mut self) {
        match self.claims.top() {
            0 => self.keys_waiting(),
            _ => flush(self.kbd),
        }
    }

    fn task_died(&mut self, dead: usize) {
        if dead == self.foreground {
            self.foreground = 0;
        }
        self.forget_reader(dead);
        if let Some(top) = self.claims.remove(dead) {
            if top {
                println!("[input] tid {} died holding the keyboard", dead);
                self.handed_down();
            }
        }
    }

    /// Only a task's own children, or itself, can be put in the foreground,
    /// and only whoever put a task there, or the task itself, takes it out.
    /// Anything more would let any program aim Ctrl-C at any other.
    fn set_foreground(&mut self, sender: usize, tid: usize) -> Message {
        let allowed = if tid == 0 {
            self.foreground == 0 || self.foreground == sender || self.foreground_setter == sender
        } else {
            tid == sender
                || syscall::sys_task_info(tid)
                    .is_ok_and(|(state, parent, _)| parent == sender && state != 3)
        };
        if !allowed {
            return error();
        }
        self.foreground = tid;
        self.foreground_setter = sender;
        if tid != 0 {
            let _ = syscall::sys_task_watch(tid);
        }
        ok()
    }
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

    if !claim_keyboard(kbd_tid) {
        println!("[input] The keyboard belongs to somebody else.");
    }

    println!("[input] Ready.");

    let mut s = Server {
        kbd: kbd_tid,
        line: [0; LINE_BUF_SIZE],
        line_len: 0,
        cooked: Cooked { buf: [0; COOKED_SIZE], len: 0 },
        readers: [(0, 0); MAX_READERS],
        nreaders: 0,
        claims: Claims { tids: [0; MAX_CLAIMANTS], depth: 0 },
        foreground: 0,
        foreground_setter: 0,
    };
    // Whatever was typed before this server was listening.
    s.keys_waiting();

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }
        let sender = msg.sender;

        // From the kernel: a task this server watches has died, or the
        // driver has keys. Nobody is waiting for an answer. The same tags
        // from anybody else are unknown requests.
        if let Some(dead) = death_notice(&msg) {
            s.task_died(dead);
            continue;
        }
        if sender == 0 {
            if msg.tag == TAG_NOTIFICATION {
                // Ctrl-C comes with the keys, so there is only one thing to
                // do. While the keyboard belongs to somebody else, ^C is
                // theirs to interpret: acting on it here would interrupt the
                // compositor, which is the foreground task.
                s.keys_waiting();
            }
            continue;
        }
        s.forget_reader(sender);

        let reply = match msg.tag {
            TAG_READ => {
                let max = (msg.data[0] as usize).min(READ_MAX);
                // One entry per task never fills a table as long as the
                // kernel's; the check is for a kernel with more.
                if max == 0 || s.nreaders == MAX_READERS {
                    pack_read_reply(&[])
                } else {
                    // Held until there is a line, or answered from one now.
                    s.readers[s.nreaders] = (sender, max);
                    s.nreaders += 1;
                    s.keys_waiting();
                    continue;
                }
            }
            TAG_SET_FOREGROUND => s.set_foreground(sender, msg.data[0] as usize),
            TAG_INPUT_CLAIM => s.claim(sender),
            TAG_INPUT_RELEASE => match s.claims.remove(sender) {
                None => error(),
                Some(top) => {
                    // Answer the releaser first: it is usually on its way out.
                    let _ = syscall::sys_reply(sender, &ok());
                    if top {
                        s.handed_down();
                    }
                    continue;
                }
            },
            TAG_INPUT_POLL if s.claims.top() == sender => match get_key_nb(kbd_tid) {
                Some(ev) => Message {
                    sender: 0,
                    tag: TAG_INPUT_KEY,
                    data: [
                        ev.press as u64,
                        ev.ascii as u64,
                        ev.scancode as u64,
                        ev.modifiers as u64,
                        0,
                        0,
                    ],
                },
                None => Message { sender: 0, tag: TAG_INPUT_NONE, data: [0; 6] },
            },
            TAG_INPUT_POLL_MOUSE if s.claims.top() == sender => {
                let ask = Message { sender: 0, tag: TAG_GET_MOUSE_NB, data: [0; 6] };
                let mut got = Message::empty();
                match syscall::sys_call_timeout(kbd_tid, &ask, &mut got, 20) {
                    syscall::CallOutcome::Replied if got.tag == TAG_MOUSE_EVENT => {
                        Message { sender: 0, tag: TAG_INPUT_MOUSE, data: got.data }
                    }
                    _ => Message { sender: 0, tag: TAG_INPUT_NONE, data: [0; 6] },
                }
            }
            // Liveness probe: answered at once, doing nothing else.
            TAG_PING => Message { sender: 0, tag: TAG_PING, data: [0; 6] },
            _ => error(),
        };
        let _ = syscall::sys_reply(sender, &reply);
    }
}

fn ok() -> Message {
    Message { sender: 0, tag: TAG_OK, data: [0; 6] }
}

fn error() -> Message {
    Message { sender: 0, tag: TAG_ERROR, data: [0; 6] }
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

/// Throw away whatever the driver has.
fn flush(kbd_tid: usize) {
    for _ in 0..DRAIN_MAX {
        if get_key_nb(kbd_tid).is_none() {
            break;
        }
    }
}

/// A read's answer: `data[0]` the byte count, `data[1..6]` the bytes.
fn pack_read_reply(bytes: &[u8]) -> Message {
    let mut data = [0u64; 6];
    data[0] = bytes.len() as u64;
    for (i, chunk) in bytes.chunks(8).take(5).enumerate() {
        let mut w = [0u8; 8];
        w[..chunk.len()].copy_from_slice(chunk);
        data[i + 1] = u64::from_le_bytes(w);
    }
    Message { sender: 0, tag: TAG_READ, data }
}

/// Take the keyboard driver, offering it the right to say when keys arrive.
fn claim_keyboard(kbd_tid: usize) -> bool {
    let msg = Message { sender: 0, tag: TAG_KBD_CLAIM, data: [0; 6] };
    let mut reply = Message::empty();
    syscall::sys_call_offer_self(kbd_tid, &msg, &mut reply).is_ok() && reply.tag == TAG_OK
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[input] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
