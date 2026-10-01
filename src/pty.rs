//! Pseudo-terminals: the pair of descriptors a terminal emulator runs a shell
//! through.
//!
//! One end is the *master*, held by whatever is drawing the terminal; the
//! other is the *slave*, which is the shell's standard input, output and
//! error. What is written to the master is what the shell reads as typing;
//! what the shell writes comes out of the master to be drawn.
//!
//! It is in the kernel for the same reason a pipe is. A terminal emulator
//! waits on its master with `poll`, and readiness the kernel cannot see is
//! readiness `poll` cannot report — a user-space pty server would have to be
//! asked, and asking is a call, and a call is not a wait. The parts that are
//! genuinely policy stay out: this holds a `termios` and a window size and
//! never interprets them beyond the three flags a line discipline is.
//!
//! The line discipline is small and exactly the part programs depend on:
//!
//! - `ECHO`: what is written to the master comes back out of it, so that
//!   typing appears without the shell having to print it.
//! - `ICANON`: input is held until a newline, and backspace takes a character
//!   back — the reason a shell does not see half a line. A character, with
//!   `IUTF8`, and not a byte of one.
//! - `ICRNL` and `ONLCR`: Return arrives as a newline, and a newline goes out
//!   as carriage return and newline, which is what puts the cursor at the left.
//! - The characters a line is edited with, from `c_cc`: erase, kill the line,
//!   erase a word — and end of file, which hands over what has been typed with
//!   no newline, and typed on an empty line is a read of nothing. That last is
//!   the only way a program reading a terminal is ever told there is no more.
//! - `ISIG`: the interrupt and quit characters are not input. They are taken
//!   out of what is typed and reported to whoever asks (`take_signal`), and
//!   the line they interrupted is thrown away.
//!
//! Everything else a `termios` can say is stored and given back unchanged, so
//! that a program which saves and restores it gets what it left.

use crate::scheduler;
use crate::task::{FdKind, MAX_FDS};

/// Save RFLAGS and disable interrupts, as `pipe.rs` does and for the same
/// reason: a timer interrupt in the middle of a ring's bookkeeping is another
/// task in the middle of the same ring's bookkeeping.
#[inline(always)]
fn irq_save() -> u64 {
    let flags: u64;
    unsafe {
        core::arch::asm!("pushfq; pop {}; cli", out(reg) flags, options(nostack));
    }
    flags
}

#[inline(always)]
fn irq_restore(flags: u64) {
    unsafe {
        core::arch::asm!("push {}; popfq", in(reg) flags, options(nostack));
    }
}

/// Pairs at once. A terminal emulator holds one; eight is more terminals than
/// this machine's screen has room for, and each is 8 KiB of kernel memory
/// spent up front.
pub const MAX_PTYS: usize = 8;
const BUF: usize = 4096;
const MAX_WAITERS: usize = 8;
/// A line being gathered in canonical mode. Longer than this and the line is
/// released as it stands, which is what Linux does at 4096 too.
const LINE: usize = 1024;

/// `termios.c_iflag`
pub const ICRNL: u32 = 0o400;
/// What is typed is UTF-8, so a character may be more than one byte.
pub const IUTF8: u32 = 0o40000;
/// `termios.c_oflag`
pub const OPOST: u32 = 0o1;
pub const ONLCR: u32 = 0o4;
/// `termios.c_lflag`
pub const ISIG: u32 = 0o1;
pub const ICANON: u32 = 0o2;
pub const ECHO: u32 = 0o10;

/// Where in `c_cc` each editing character is, as Linux numbers them.
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VSUSP: usize = 10;
const VWERASE: usize = 14;

/// The signals a terminal raises, by Linux's numbers.
pub const SIGINT: u8 = 2;
pub const SIGQUIT: u8 = 3;

/// What `TCGETS` and `TCSETS` carry, in Linux's layout: four flag words, a
/// line discipline byte and nineteen control characters.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Termios {
    pub c_iflag: u32,
    pub c_oflag: u32,
    pub c_cflag: u32,
    pub c_lflag: u32,
    pub c_line: u8,
    pub c_cc: [u8; 19],
}

/// What `TIOCGWINSZ` and `TIOCSWINSZ` carry. The kernel stores it and reads
/// nothing in it: how big a terminal is concerns the program in it and the
/// program drawing it, and neither of those is here.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct WinSize {
    pub ws_row: u16,
    pub ws_col: u16,
    pub ws_xpixel: u16,
    pub ws_ypixel: u16,
}

/// A ring of bytes, in one direction.
struct Ring {
    buf: [u8; BUF],
    head: usize,
    tail: usize,
    len: usize,
    /// Tasks waiting for something to read from it.
    waiters: [usize; MAX_WAITERS],
    nwaiters: usize,
    /// Tasks waiting for room to write into it.
    writers: [usize; MAX_WAITERS],
    nwriters: usize,
}

impl Ring {
    const fn new() -> Self {
        Ring {
            buf: [0; BUF],
            head: 0,
            tail: 0,
            len: 0,
            waiters: [0; MAX_WAITERS],
            nwaiters: 0,
            writers: [0; MAX_WAITERS],
            nwriters: 0,
        }
    }

    /// Everybody parked on this ring, either way round, into `wake`.
    fn take_waiters(&mut self, wake: &mut [usize], n: &mut usize) {
        for i in 0..self.nwaiters {
            wake[*n] = self.waiters[i];
            *n += 1;
        }
        self.nwaiters = 0;
        self.take_writers(wake, n);
    }

    fn take_writers(&mut self, wake: &mut [usize], n: &mut usize) {
        for i in 0..self.nwriters {
            wake[*n] = self.writers[i];
            *n += 1;
        }
        self.nwriters = 0;
    }

    fn push(&mut self, b: u8) -> bool {
        if self.len == BUF {
            return false;
        }
        self.buf[self.head] = b;
        self.head = (self.head + 1) % BUF;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<u8> {
        if self.len == 0 {
            return None;
        }
        let b = self.buf[self.tail];
        self.tail = (self.tail + 1) % BUF;
        self.len -= 1;
        Some(b)
    }

    fn room(&self) -> usize {
        BUF - self.len
    }
}

struct Pty {
    in_use: bool,
    /// Who made it, so that one never wired to a descriptor can be reclaimed.
    creator: usize,
    /// Descriptors naming each end: 0 is the master, 1 the slave.
    refs: [usize; 2],
    /// Whether the slave has ever been opened.
    ///
    /// A pair is made by opening the master, and the slave is opened after it.
    /// Between the two there is no slave, and a master reading then must wait
    /// rather than see an end of file — the program that will hold the slave
    /// has not been started yet. Once it has been opened, the last slave
    /// closing *is* the end of file, which is how a terminal emulator learns
    /// its shell has exited.
    slave_opened: bool,
    /// Towards the slave (what was typed) and towards the master (what the
    /// program printed).
    to_slave: Ring,
    to_master: Ring,
    /// The line being gathered, in canonical mode.
    line: [u8; LINE],
    line_len: usize,
    /// Ends of file typed and not yet read: each is one read of nothing.
    eofs: u32,
    /// A signal a typed character raised and nobody has collected.
    signal: u8,
    termios: Termios,
    size: WinSize,
}

const NO_PTY: Pty = Pty {
    in_use: false,
    creator: 0,
    refs: [0; 2],
    slave_opened: false,
    to_slave: Ring::new(),
    to_master: Ring::new(),
    line: [0; LINE],
    line_len: 0,
    eofs: 0,
    signal: 0,
    // What a terminal looks like before anybody has said otherwise: canonical
    // input with echo, Return read as a newline, newline written as carriage
    // return and newline. A program that wants raw bytes turns them off, which
    // is what `tcsetattr` is for.
    termios: Termios {
        c_iflag: ICRNL | IUTF8,
        c_oflag: OPOST | ONLCR,
        c_cflag: 0o2277, // B38400 | CS8 | CREAD, as Linux's default
        c_lflag: ISIG | ICANON | ECHO,
        c_line: 0,
        // INTR, QUIT, ERASE, KILL, EOF, and the rest as Linux leaves them.
        c_cc: [3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0],
    },
    size: WinSize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 },
};

static mut PTYS: [Pty; MAX_PTYS] = [NO_PTY; MAX_PTYS];

fn ptys() -> &'static mut [Pty; MAX_PTYS] {
    unsafe { &mut *core::ptr::addr_of_mut!(PTYS) }
}

/// Make a pair. Returns its index, with neither end referenced yet.
pub fn create(creator: usize) -> Option<usize> {
    let flags = irq_save();
    let out = (0..MAX_PTYS).find(|&i| !ptys()[i].in_use).inspect(|&i| {
        ptys()[i] = NO_PTY;
        ptys()[i].in_use = true;
        ptys()[i].creator = creator;
    });
    irq_restore(flags);
    out
}

/// One more descriptor naming an end.
pub fn retain(pty: usize, end: u8) {
    if pty >= MAX_PTYS || end > 1 {
        return;
    }
    let flags = irq_save();
    let p = &mut ptys()[pty];
    if p.in_use {
        p.refs[end as usize] += 1;
        if end == 1 {
            p.slave_opened = true;
        }
    }
    irq_restore(flags);
}

/// Does this pty exist, and is its master still held? Asked by the open of a
/// slave, which must not resurrect a pair nobody has.
pub fn slave_openable(pty: usize) -> bool {
    if pty >= MAX_PTYS {
        return false;
    }
    let flags = irq_save();
    let ok = {
        let p = &ptys()[pty];
        p.in_use && p.refs[0] > 0
    };
    irq_restore(flags);
    ok
}

/// One fewer. The last of either end wakes whoever was waiting on the other,
/// because a read with nobody left to write is an end of file rather than a
/// wait; the pair goes when both ends have gone.
pub fn release(pty: usize, end: u8) {
    if pty >= MAX_PTYS || end > 1 {
        return;
    }
    let mut wake = [0usize; MAX_WAITERS * 4];
    let mut n = 0;
    let flags = irq_save();
    {
        let p = &mut ptys()[pty];
        if !p.in_use {
            irq_restore(flags);
            return;
        }
        let e = end as usize;
        p.refs[e] = p.refs[e].saturating_sub(1);
        if p.refs[e] == 0 {
            // Readers, for whom this is the end of the file, and writers, for
            // whom there will never be room.
            for side in [&mut p.to_slave, &mut p.to_master] {
                side.take_waiters(&mut wake, &mut n);
            }
        }
        if p.refs[0] == 0 && p.refs[1] == 0 {
            *p = NO_PTY;
        }
    }
    irq_restore(flags);
    for &tid in &wake[..n] {
        scheduler::unblock_task(tid);
    }
    // An end going is a hangup, which a set is waiting to hear about.
    crate::pollset::note_pty(pty);
}

/// Throw away pairs a task made and never wired to a descriptor.
pub fn cleanup_orphans(creator: usize) {
    let flags = irq_save();
    for p in ptys().iter_mut() {
        if p.in_use && p.creator == creator && p.refs[0] == 0 && p.refs[1] == 0 {
            *p = NO_PTY;
        }
    }
    irq_restore(flags);
}

/// Is the other end still held? A read with nobody to write it is over.
///
/// For the master, a slave that has not been opened *yet* is not a slave that
/// has gone: the pair is made before the program that will hold it is started.
fn peer_gone(p: &Pty, end: u8) -> bool {
    if end == 0 { p.slave_opened && p.refs[1] == 0 } else { p.refs[0] == 0 }
}

/// Write `bytes` to one end, returning how many were taken.
///
/// From the master this is typing: it goes through the line discipline and may
/// be echoed back. From the slave it is output: it goes towards the master,
/// with newlines expanded if `ONLCR` says so.
pub fn write(pty: usize, end: u8, bytes: &[u8]) -> usize {
    if pty >= MAX_PTYS || end > 1 {
        return 0;
    }
    let mut wake = [0usize; MAX_WAITERS * 2];
    let mut n = 0;
    let flags = irq_save();
    let written = {
        let p = &mut ptys()[pty];
        if !p.in_use {
            irq_restore(flags);
            return 0;
        }
        let done = if end == 0 { input(p, bytes) } else { output(p, bytes) };
        // Whoever was waiting for something to read now has it.
        let side = if end == 0 { &mut p.to_slave } else { &mut p.to_master };
        for i in 0..side.nwaiters {
            wake[n] = side.waiters[i];
            n += 1;
        }
        side.nwaiters = 0;
        // Echo wakes a reader on the master as well.
        if end == 0 {
            let back = &mut p.to_master;
            for i in 0..back.nwaiters {
                wake[n] = back.waiters[i];
                n += 1;
            }
            back.nwaiters = 0;
        }
        done
    };
    irq_restore(flags);
    for &tid in &wake[..n] {
        scheduler::unblock_task(tid);
    }
    // And whoever is waiting on a set rather than on a read.
    crate::pollset::note_pty(pty);
    written
}

/// Take the last character of the line being gathered back, and un-draw it:
/// a terminal that echoed it has already shown it.
fn rub_out(p: &mut Pty, echo: bool) -> bool {
    if p.line_len == 0 {
        return false;
    }
    p.line_len -= 1;
    // In UTF-8 a character is a first byte and the bytes that continue it,
    // and taking back the last byte of one leaves the rest as something that
    // is not a character at all — which is then what the program reads. With
    // `IUTF8` the whole of it goes: back through the continuing bytes to the
    // byte that began it.
    if p.termios.c_iflag & IUTF8 != 0 {
        while p.line_len > 0 && p.line[p.line_len] & 0xC0 == 0x80 {
            p.line_len -= 1;
        }
    }
    if echo && p.to_master.room() >= 3 {
        for c in *b"\x08 \x08" {
            p.to_master.push(c);
        }
    }
    true
}

/// Typing, through the line discipline.
fn input(p: &mut Pty, bytes: &[u8]) -> usize {
    let mut done = 0;
    for &raw in bytes {
        let mut b = raw;
        if p.termios.c_iflag & ICRNL != 0 && b == b'\r' {
            b = b'\n';
        }
        let canon = p.termios.c_lflag & ICANON != 0;
        let echo = p.termios.c_lflag & ECHO != 0;
        let cc = p.termios.c_cc;
        // A control character set to 0 is one that is switched off.
        let is = |which: usize| cc[which] != 0 && b == cc[which];

        // Characters that are not input at all: they say something about the
        // program in the terminal. The line they were typed into goes, and
        // what was waiting to be read with it, as on Linux.
        if p.termios.c_lflag & ISIG != 0 && (is(VINTR) || is(VQUIT) || is(VSUSP)) {
            if !is(VSUSP) {
                p.signal = if is(VINTR) { SIGINT } else { SIGQUIT };
                p.line_len = 0;
                p.to_slave.head = 0;
                p.to_slave.tail = 0;
                p.to_slave.len = 0;
                p.eofs = 0;
                // Shown as `^C`, and no more: the newline after it is the
                // shell's to print, when it finds what the signal did.
                if echo && p.to_master.room() >= 2 {
                    p.to_master.push(b'^');
                    p.to_master.push(b ^ 0x40);
                }
            }
            // Stopping a job needs jobs. The character is dropped.
            done += 1;
            continue;
        }

        if canon {
            // Backspace and delete both, whichever the terminal sends.
            if is(VERASE) || b == 8 || b == 127 {
                rub_out(p, echo);
                done += 1;
                continue;
            }
            if is(VKILL) {
                while rub_out(p, echo) {}
                done += 1;
                continue;
            }
            if is(VWERASE) {
                while p.line_len > 0 && p.line[p.line_len - 1] == b' ' {
                    rub_out(p, echo);
                }
                while p.line_len > 0 && p.line[p.line_len - 1] != b' ' {
                    rub_out(p, echo);
                }
                done += 1;
                continue;
            }
            if is(VEOF) {
                // What has been typed is handed over as it stands, with no
                // newline; nothing typed is a read of nothing, which is what
                // tells a program there is no more. Not echoed.
                if p.to_slave.room() < p.line_len {
                    break;
                }
                if p.line_len == 0 {
                    p.eofs += 1;
                }
                for i in 0..p.line_len {
                    let c = p.line[i];
                    p.to_slave.push(c);
                }
                p.line_len = 0;
                done += 1;
                continue;
            }
        }

        if echo {
            if b == b'\n' && p.termios.c_oflag & ONLCR != 0 {
                if p.to_master.room() < 2 {
                    break;
                }
                p.to_master.push(b'\r');
                p.to_master.push(b'\n');
            } else if !p.to_master.push(b) {
                break;
            }
        }

        if canon {
            if p.line_len < LINE {
                p.line[p.line_len] = b;
                p.line_len += 1;
            }
            // A line is only a line once it ends, which is what makes a shell
            // see whole commands. A line that fills the buffer is released as
            // it stands rather than lost.
            if b == b'\n' || p.line_len == LINE {
                if p.to_slave.room() < p.line_len {
                    // No room for the whole line: leave it gathered and stop
                    // taking input, rather than deliver half of one.
                    break;
                }
                for i in 0..p.line_len {
                    let c = p.line[i];
                    p.to_slave.push(c);
                }
                p.line_len = 0;
            }
        } else if !p.to_slave.push(b) {
            break;
        }
        done += 1;
    }
    done
}

/// Output, on its way to whatever is drawing the terminal.
fn output(p: &mut Pty, bytes: &[u8]) -> usize {
    let mut done = 0;
    let expand = p.termios.c_oflag & OPOST != 0 && p.termios.c_oflag & ONLCR != 0;
    for &b in bytes {
        if expand && b == b'\n' {
            if p.to_master.room() < 2 {
                break;
            }
            p.to_master.push(b'\r');
            p.to_master.push(b'\n');
        } else if !p.to_master.push(b) {
            break;
        }
        done += 1;
    }
    done
}

/// What a read from an end would find.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// This many bytes. None is a read that waits.
    Bytes(usize),
    /// An end of file somebody typed: a read of nothing, once, and at once.
    End,
    /// Nothing, and the other end has gone: the end, for good.
    Gone,
}

fn pending(p: &Pty, end: u8) -> Pending {
    let waiting = if end == 0 { p.to_master.len } else { p.to_slave.len };
    if waiting > 0 {
        Pending::Bytes(waiting)
    } else if end == 1 && p.eofs > 0 {
        Pending::End
    } else if peer_gone(p, end) {
        Pending::Gone
    } else {
        Pending::Bytes(0)
    }
}

/// What a read from `end` would find now.
pub fn readable(pty: usize, end: u8) -> Pending {
    if pty >= MAX_PTYS || end > 1 {
        return Pending::Gone;
    }
    let flags = irq_save();
    let p = &ptys()[pty];
    let out = if p.in_use { pending(p, end) } else { Pending::Gone };
    irq_restore(flags);
    out
}

/// Take up to `buf.len()` bytes. `Ok(0)` means end of file; `Err(())` means
/// there is nothing yet and the caller should wait.
pub fn read(pty: usize, end: u8, buf: &mut [u8]) -> Result<usize, ()> {
    if pty >= MAX_PTYS || end > 1 {
        return Ok(0);
    }
    let mut wake = [0usize; MAX_WAITERS];
    let mut nwake = 0;
    let flags = irq_save();
    let out = {
        let p = &mut ptys()[pty];
        if !p.in_use {
            Ok(0)
        } else {
            let found = pending(p, end);
            // An end of file somebody typed is read once, as nothing.
            if found == Pending::End {
                p.eofs -= 1;
            }
            let side = if end == 0 { &mut p.to_master } else { &mut p.to_slave };
            if side.len == 0 {
                if found == Pending::Bytes(0) { Err(()) } else { Ok(0) }
            } else {
                let mut n = 0;
                while n < buf.len() {
                    match side.pop() {
                        Some(b) => {
                            buf[n] = b;
                            n += 1;
                        }
                        None => break,
                    }
                }
                // There is room where there was none: whoever was waiting to
                // write can.
                side.take_writers(&mut wake, &mut nwake);
                Ok(n)
            }
        }
    };
    irq_restore(flags);
    if nwake > 0 {
        for &tid in &wake[..nwake] {
            scheduler::unblock_task(tid);
        }
        // And a set waiting for this end to be writable.
        crate::pollset::note_pty(pty);
    }
    out
}

/// What became of a wait for something to read.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// Look again: something was written, or was there all along.
    Look,
    /// A signal the program has a handler for arrived instead.
    Interrupted,
    /// There was no room to record the waiter, which must not become a wait:
    /// a waiter nobody knows about is never woken.
    NoRoom,
}

/// Wait for something to read at this end, then come back and look again.
///
/// It looks again before it parks. The caller found nothing a moment ago,
/// with interrupts on since: a writer that ran in between woke nobody,
/// because nobody was recorded yet, and a wait begun then would last until
/// the *next* thing was written — a line typed and not delivered until the
/// key after it. A signal is looked for in the same place, for the same
/// reason: one raised a moment ago is seen, and one raised a moment from now
/// finds this task parked.
pub fn wait_readable(pty: usize, end: u8) -> Waited {
    if pty >= MAX_PTYS || end > 1 {
        return Waited::NoRoom;
    }
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let (out, parked) = {
        let p = &mut ptys()[pty];
        if !p.in_use {
            (Waited::NoRoom, false)
        } else if pending(p, end) != Pending::Bytes(0) {
            (Waited::Look, false)
        } else if crate::signal::interrupted(tid) {
            (Waited::Interrupted, false)
        } else {
            let side = if end == 0 { &mut p.to_master } else { &mut p.to_slave };
            if side.nwaiters >= MAX_WAITERS {
                (Waited::NoRoom, false)
            } else {
                side.waiters[side.nwaiters] = tid;
                side.nwaiters += 1;
                scheduler::block_task(tid);
                (Waited::Look, true)
            }
        }
    };
    irq_restore(flags);
    if parked {
        scheduler::yield_now();
    }
    out
}

/// Wait for room to write what a program prints, then come back and try
/// again. False when waiting is no use: the master has gone, and nothing will
/// ever take what is there; or there was nowhere to record the waiter.
///
/// For the slave only. What the master writes is typing, which goes through
/// the line discipline and can be refused for want of room in *either*
/// direction — the echo comes back at the writer — so a terminal emulator
/// writes without waiting and keeps what did not fit.
///
/// A write that returned nothing instead of waiting is what this replaced:
/// `cat` of a file longer than the buffer reported "No space left on device",
/// because a write of nothing is what a full disk looks like.
pub fn wait_writable(pty: usize) -> bool {
    if pty >= MAX_PTYS {
        return false;
    }
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let (ok, parked) = {
        let p = &mut ptys()[pty];
        if !p.in_use || peer_gone(p, 1) {
            (false, false)
        } else if p.to_master.room() > 0 {
            // Somebody read between the write and this: look again.
            (true, false)
        } else if p.to_master.nwriters >= MAX_WAITERS {
            (false, false)
        } else {
            p.to_master.writers[p.to_master.nwriters] = tid;
            p.to_master.nwriters += 1;
            scheduler::block_task(tid);
            (true, true)
        }
    };
    irq_restore(flags);
    if parked {
        scheduler::yield_now();
    }
    ok
}

/// A signal has arrived for a task that may be parked reading a terminal:
/// if it is, it stops waiting and goes to see.
pub fn interrupt(tid: usize) {
    let mut found = false;
    let flags = irq_save();
    for p in ptys().iter_mut() {
        if !p.in_use {
            continue;
        }
        for side in [&mut p.to_slave, &mut p.to_master] {
            let before = side.nwaiters;
            crate::pipe::forget_in(&mut side.waiters, &mut side.nwaiters, tid);
            found |= side.nwaiters != before;
        }
    }
    irq_restore(flags);
    if found {
        scheduler::unblock_task(tid);
    }
}

/// Is the other end of this one gone for good?
pub fn other_end_gone(pty: usize, end: u8) -> bool {
    if pty >= MAX_PTYS || end > 1 {
        return true;
    }
    let flags = irq_save();
    let p = &ptys()[pty];
    let gone = !p.in_use || peer_gone(p, end);
    irq_restore(flags);
    gone
}

/// A task parked on this pty has died: it is waiting for nothing now.
pub fn forget_waiter(pty: usize, tid: usize) {
    if pty >= MAX_PTYS {
        return;
    }
    let flags = irq_save();
    let p = &mut ptys()[pty];
    if p.in_use {
        for side in [&mut p.to_slave, &mut p.to_master] {
            crate::pipe::forget_in(&mut side.waiters, &mut side.nwaiters, tid);
            crate::pipe::forget_in(&mut side.writers, &mut side.nwriters, tid);
        }
    }
    irq_restore(flags);
}

/// Is this end writable? A pty's buffer is the only limit; when it is full a
/// writer waits, as it would on a pipe.
pub fn writable(pty: usize, end: u8) -> bool {
    if pty >= MAX_PTYS || end > 1 {
        return false;
    }
    let flags = irq_save();
    let out = {
        let p = &ptys()[pty];
        p.in_use && (if end == 0 { p.to_slave.room() } else { p.to_master.room() }) > 0
    };
    irq_restore(flags);
    out
}

/// The signal a character typed at this terminal raised, if one has been and
/// nobody has collected it. Collected by whoever wrote the character: the
/// write is where it is noticed, and the caller decides who it is for.
pub fn take_signal(pty: usize) -> Option<u8> {
    if pty >= MAX_PTYS {
        return None;
    }
    let flags = irq_save();
    let sig = core::mem::replace(&mut ptys()[pty].signal, 0);
    irq_restore(flags);
    (sig != 0).then_some(sig)
}

pub fn get_termios(pty: usize) -> Option<Termios> {
    if pty >= MAX_PTYS {
        return None;
    }
    let flags = irq_save();
    let out = if ptys()[pty].in_use { Some(ptys()[pty].termios) } else { None };
    irq_restore(flags);
    out
}

pub fn set_termios(pty: usize, t: &Termios) -> bool {
    if pty >= MAX_PTYS {
        return false;
    }
    let flags = irq_save();
    let ok = ptys()[pty].in_use;
    if ok {
        ptys()[pty].termios = *t;
        // Leaving canonical mode releases what was gathered: a program that
        // turns it off is asking for the bytes it has already typed, not for
        // them to be held until a newline that will never come.
        if t.c_lflag & ICANON == 0 {
            let p = &mut ptys()[pty];
            let n = p.line_len;
            for i in 0..n {
                let c = p.line[i];
                p.to_slave.push(c);
            }
            p.line_len = 0;
        }
    }
    irq_restore(flags);
    ok
}

pub fn get_winsize(pty: usize) -> Option<WinSize> {
    if pty >= MAX_PTYS {
        return None;
    }
    let flags = irq_save();
    let out = if ptys()[pty].in_use { Some(ptys()[pty].size) } else { None };
    irq_restore(flags);
    out
}

pub fn set_winsize(pty: usize, size: &WinSize) -> bool {
    if pty >= MAX_PTYS {
        return false;
    }
    let flags = irq_save();
    let ok = ptys()[pty].in_use;
    if ok {
        ptys()[pty].size = *size;
    }
    irq_restore(flags);
    ok
}

/// The pty and end a descriptor names, if it names one.
pub fn of_fd(tid: usize, fd: usize) -> Option<(usize, u8)> {
    if fd >= MAX_FDS {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        FdKind::PtyEnd { pty, end } => Some((pty, end)),
        _ => None,
    }
}
