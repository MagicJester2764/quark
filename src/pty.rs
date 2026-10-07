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
//! - `ISIG`: the interrupt, quit and suspend characters are not input. They
//!   are taken out of what is typed and reported to whoever asks
//!   (`take_signal`), and the line they interrupted is thrown away.
//!
//! And it knows who it is the terminal *of*: the session it is the
//! controlling terminal of, and which of that session's process groups is in
//! front (`job.rs`). The signal a typed character raises is for the group in
//! front, and a read by any other group of the session is not a read but a
//! reason to stop the reader.
//!
//! Everything else a `termios` can say is stored and given back unchanged, so
//! that a program which saves and restores it gets what it left.

use crate::scheduler;
use crate::task::{FdKind, FD_MOST};
use crate::waitlist::{self, On, Waiters};

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

const BUF: usize = 4096;
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
/// A job behind that writes is stopped, as one that reads is.
pub const TOSTOP: u32 = 0o400;

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
pub const SIGTSTP: u8 = 20;

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
    /// Tasks waiting for something to read from it, and for room to write
    /// into it (`waitlist.rs`). There were eight of each.
    readers: Waiters,
    writers: Waiters,
}

impl Ring {
    const fn new() -> Self {
        Ring { buf: [0; BUF], head: 0, tail: 0, len: 0, readers: Waiters::NONE, writers: Waiters::NONE }
    }

    /// Everybody parked on this ring, either way round, woken to look again.
    ///
    /// # Safety
    /// Interrupts off.
    unsafe fn wake_all(&mut self) {
        unsafe {
            waitlist::wake_all(&mut self.readers);
            waitlist::wake_all(&mut self.writers);
        }
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
    /// Who made it, so that one never wired to a descriptor can be reclaimed.
    creator: usize,
    /// And which user that was: until a session claims the terminal, its
    /// slave is that user's.
    creator_uid: u32,
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
    /// The session this is the controlling terminal of, and the process
    /// group of it that is in front. 0 for none: a terminal nobody has
    /// claimed, whose typed signals go to whoever has it open.
    session: u64,
    front: u64,
    termios: Termios,
    size: WinSize,
}

const NO_PTY: Pty = Pty {
    creator: 0,
    creator_uid: 0,
    refs: [0; 2],
    slave_opened: false,
    to_slave: Ring::new(),
    to_master: Ring::new(),
    line: [0; LINE],
    line_len: 0,
    eofs: 0,
    signal: 0,
    session: 0,
    front: 0,
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

/// Every pair, by its number — which is the `N` of `/dev/pts/N` — made when a
/// master is opened and given back when both ends have gone (`table.rs`).
/// There were eight, each spent up front.
static mut PTYS: crate::table::Table<Pty> = crate::table::Table::new(crate::table::MOST);

/// What a new pair is made from, in its room: nine kilobytes is not a
/// record to build on a kernel stack.
static PTY_TEMPLATE: crate::table::Template<Pty> = crate::table::Template(Some(NO_PTY));

/// Pair `pty`, if there is one.
///
/// Interrupts are off: every caller holds them off from asking to finishing.
fn pty_at(pty: usize) -> Option<&'static mut Pty> {
    unsafe { (*core::ptr::addr_of_mut!(PTYS)).get(pty) }
}

fn ptys() -> &'static mut crate::table::Table<Pty> {
    unsafe { &mut *core::ptr::addr_of_mut!(PTYS) }
}

/// Make a pair. Returns its index, with neither end referenced yet; `None`
/// with no room for it — as `reclaim` decides for whatever a program makes.
pub fn create(creator: usize) -> Option<usize> {
    if !crate::reclaim::may_make() {
        return None;
    }
    let uid = scheduler::task_uid_gid(creator).map_or(0, |(uid, _)| uid);
    let flags = irq_save();
    let out = ptys().lowest_free(0).filter(|&i| {
        ptys()
            .fill_from(i, &PTY_TEMPLATE, |p| {
                p.creator = creator;
                p.creator_uid = uid;
            })
            .is_ok()
    });
    irq_restore(flags);
    out
}

/// One more descriptor naming an end.
pub fn retain(pty: usize, end: u8) {
    if end > 1 {
        return;
    }
    let flags = irq_save();
    if let Some(p) = pty_at(pty) {
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
    let flags = irq_save();
    let ok = pty_at(pty).is_some_and(|p| p.refs[0] > 0);
    irq_restore(flags);
    ok
}

/// Whether task `tid` may use this terminal's slave: open it by its number,
/// read what is typed at it, write to it, change how it behaves.
///
/// A terminal is its session's. Once a session has claimed one, its slave
/// is for the members of that session; until one has, for the user who made
/// the pair, which is how a terminal emulator hands its shell a terminal.
/// And for whoever holds authority over every task: what may end a program
/// may look at what is typed to it. Not for user 0 as such — being user 0
/// opens nothing in this kernel, and the programs that keep a terminal
/// between sessions and take it for one are user 0 and hold nothing.
///
/// Holding a descriptor for it is not enough, and that is the point. A
/// descriptor is inherited by everything a session starts, and a program
/// that outlives its session — started in the background by somebody who
/// then logged out — went on holding the console's: the next person's
/// keystrokes were its to read, their password among them. Unix takes the
/// descriptor away (`vhangup`); here the question is asked each time, and
/// the answer changed when the session that was the program's ended. And a
/// slave could be opened by its number by anybody at all.
pub fn slave_is_for(pty: usize, tid: usize) -> bool {
    if crate::cap::task_has_task_mgmt(tid, 0) {
        return true;
    }
    let Ok((uid, _)) = scheduler::task_uid_gid(tid) else {
        return false;
    };
    let session = crate::job::sid_of(tid);
    let flags = irq_save();
    let ok = pty_at(pty).is_some_and(|p| if p.session != 0 { p.session == session } else { p.creator_uid == uid });
    irq_restore(flags);
    ok
}

/// One fewer. The last of either end wakes whoever was waiting on the other,
/// because a read with nobody left to write is an end of file rather than a
/// wait; the pair goes when both ends have gone.
pub fn release(pty: usize, end: u8) {
    if end > 1 {
        return;
    }
    let flags = irq_save();
    {
        let Some(p) = pty_at(pty) else {
            irq_restore(flags);
            return;
        };
        let e = end as usize;
        p.refs[e] = p.refs[e].saturating_sub(1);
        if p.refs[e] == 0 {
            // Readers, for whom this is the end of the file, and writers, for
            // whom there will never be room.
            unsafe {
                p.to_slave.wake_all();
                p.to_master.wake_all();
            }
        }
        if p.refs[0] == 0 && p.refs[1] == 0 {
            gone(pty);
        }
    }
    irq_restore(flags);
    // An end going is a hangup, which a set is waiting to hear about.
    crate::pollset::note_pty(pty);
}

/// Pair `pty` goes: whoever is still waiting on it looks again and finds
/// nothing. Interrupts are off.
fn gone(pty: usize) {
    if let Some(p) = pty_at(pty) {
        unsafe {
            p.to_slave.wake_all();
            p.to_master.wake_all();
        }
    }
    ptys().empty(pty);
}

/// Throw away pairs a task made and never wired to a descriptor.
pub fn cleanup_orphans(creator: usize) {
    let flags = irq_save();
    let mut at = 0;
    while let Some(i) = ptys().next_used(at) {
        at = i + 1;
        if pty_at(i).is_some_and(|p| p.creator == creator && p.refs[0] == 0 && p.refs[1] == 0) {
            gone(i);
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
    if end > 1 {
        return 0;
    }
    let flags = irq_save();
    let written = {
        let Some(p) = pty_at(pty) else {
            irq_restore(flags);
            return 0;
        };
        let done = if end == 0 { input(p, bytes) } else { output(p, bytes) };
        // Whoever was waiting for something to read now has it.
        let side = if end == 0 { &mut p.to_slave } else { &mut p.to_master };
        unsafe { waitlist::wake_all(&mut side.readers) };
        // Echo wakes a reader on the master as well.
        if end == 0 {
            unsafe { waitlist::wake_all(&mut p.to_master.readers) };
        }
        done
    };
    irq_restore(flags);
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
            p.signal = if is(VINTR) {
                SIGINT
            } else if is(VQUIT) {
                SIGQUIT
            } else {
                SIGTSTP
            };
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
    for &b in bytes {
        let takes = takes(p, b);
        if p.to_master.room() < takes {
            break;
        }
        if takes == 2 {
            p.to_master.push(b'\r');
        }
        p.to_master.push(b);
        done += 1;
    }
    done
}

/// How much room a byte a program prints takes in what goes to the master:
/// a newline that goes out as a return and a newline takes two.
///
/// One answer, for the write and for the wait for room ([`wait_writable`])
/// and for whether the end is writable at all: they have to agree about
/// what is being waited for.
fn takes(p: &Pty, b: u8) -> usize {
    let expand = p.termios.c_oflag & OPOST != 0 && p.termios.c_oflag & ONLCR != 0;
    if expand && b == b'\n' { 2 } else { 1 }
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
    if end > 1 {
        return Pending::Gone;
    }
    let flags = irq_save();
    let out = pty_at(pty).map_or(Pending::Gone, |p| pending(p, end));
    irq_restore(flags);
    out
}

/// Take up to `buf.len()` bytes. `Ok(0)` means end of file; `Err(())` means
/// there is nothing yet and the caller should wait.
pub fn read(pty: usize, end: u8, buf: &mut [u8]) -> Result<usize, ()> {
    if end > 1 {
        return Ok(0);
    }
    let mut woke = false;
    let flags = irq_save();
    let out = {
        if let Some(p) = pty_at(pty) {
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
                woke = !side.writers.is_empty();
                unsafe { waitlist::wake_all(&mut side.writers) };
                Ok(n)
            }
        } else {
            Ok(0)
        }
    };
    irq_restore(flags);
    if woke {
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
    /// There is no such terminal, or no use waiting on it.
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
    if end > 1 {
        return Waited::NoRoom;
    }
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let (out, parked) = match pty_at(pty) {
        None => (Waited::NoRoom, false),
        Some(p) if pending(p, end) != Pending::Bytes(0) => (Waited::Look, false),
        Some(_) if crate::signal::interrupted(tid) => (Waited::Interrupted, false),
        Some(p) => {
            // The master reads what the program printed, the slave what was
            // typed.
            let (side, which) = if end == 0 { (&mut p.to_master, MASTER_READS) } else { (&mut p.to_slave, SLAVE_READS) };
            unsafe { waitlist::add(&mut side.readers, tid, On::Pty(pty as u32, which)) };
            scheduler::block_task(tid);
            (Waited::Look, true)
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
/// `next` is the byte the write stopped at, and the wait is for room for
/// *that*. It asked whether there was any room, and a newline wants two
/// bytes: with one byte left the write took nothing, the wait found room
/// and did not wait, and the two went round for ever — in the kernel, with
/// the kernel's lock. On one processor a tick let the terminal's reader in
/// and it ended. On several the reader was at the kernel's door, waiting
/// for the lock, and so was everything else: the machine stopped, about
/// one run of a long test in several, with whatever it was printing cut
/// off in the middle of a line.
///
/// For the slave only. What the master writes is typing, which goes through
/// the line discipline and can be refused for want of room in *either*
/// direction — the echo comes back at the writer — so a terminal emulator
/// writes without waiting and keeps what did not fit.
///
/// A write that returned nothing instead of waiting is what this replaced:
/// `cat` of a file longer than the buffer reported "No space left on device",
/// because a write of nothing is what a full disk looks like.
pub fn wait_writable(pty: usize, next: u8) -> Waited {
    let tid = scheduler::current_tid();
    let flags = irq_save();
    let (ok, parked) = match pty_at(pty) {
        None => (Waited::NoRoom, false),
        Some(p) if peer_gone(p, 1) => (Waited::NoRoom, false),
        // Somebody read between the write and this: look again.
        Some(p) if p.to_master.room() >= takes(p, next) => (Waited::Look, false),
        Some(_) if crate::signal::ends_wait(tid) => (Waited::Interrupted, false),
        Some(p) => {
            unsafe { waitlist::add(&mut p.to_master.writers, tid, On::Pty(pty as u32, SLAVE_WRITES)) };
            scheduler::block_task(tid);
            (Waited::Look, true)
        }
    };
    irq_restore(flags);
    if parked {
        scheduler::yield_now();
    }
    ok
}

/// A signal has arrived for a task that may be parked reading a terminal:
/// if it is, it stops waiting and goes to see. Reading, and not writing: a
/// signal a program is told of ends a read of a terminal and not a write,
/// and one the kernel runs ends a write through what the writer holds
/// (`fdtable::interrupt`).
pub fn interrupt(tid: usize) -> bool {
    let flags = irq_save();
    let found = unsafe {
        matches!(waitlist::on(tid), On::Pty(_, MASTER_READS | SLAVE_READS)) && waitlist::forget(tid)
    };
    if found {
        scheduler::unblock_task(tid);
    }
    irq_restore(flags);
    found
}

/// Which of a pair's lists a task waits on, as `On::Pty` says it: reading
/// at the slave what was typed, reading at the master what was printed, and
/// waiting for room to print.
const SLAVE_READS: u8 = 0;
const MASTER_READS: u8 = 1;
const SLAVE_WRITES: u8 = 2;

/// Pair `pty`'s list `which`. For `waitlist::forget`.
///
/// # Safety
/// Interrupts off.
pub unsafe fn waiters(pty: usize, which: u8) -> Option<&'static mut Waiters> {
    pty_at(pty).map(|p| match which {
        SLAVE_READS => &mut p.to_slave.readers,
        MASTER_READS => &mut p.to_master.readers,
        _ => &mut p.to_master.writers,
    })
}

/// Is the other end of this one gone for good?
pub fn other_end_gone(pty: usize, end: u8) -> bool {
    if end > 1 {
        return true;
    }
    let flags = irq_save();
    let gone = pty_at(pty).is_none_or(|p| peer_gone(p, end));
    irq_restore(flags);
    gone
}

/// Whether a job behind is stopped for writing to the terminal (`TOSTOP`).
pub fn stops_writers(pty: usize) -> bool {
    get_termios(pty).is_some_and(|t| t.c_lflag & TOSTOP != 0)
}

/// Is this end writable? A pty's buffer is the only limit; when it is full a
/// writer waits, as it would on a pipe.
pub fn writable(pty: usize, end: u8) -> bool {
    if end > 1 {
        return false;
    }
    let flags = irq_save();
    // Room for whatever comes next, which for what a program prints may be
    // a newline: an end that said it could be written to and then took
    // nothing is a program that asks again at once, for ever.
    let out = pty_at(pty)
        .is_some_and(|p| if end == 0 { p.to_slave.room() > 0 } else { p.to_master.room() >= takes(p, b'\n') });
    irq_restore(flags);
    out
}

/// The signal a character typed at this terminal raised, if one has been and
/// nobody has collected it. Collected by whoever wrote the character: the
/// write is where it is noticed, and the caller decides who it is for.
pub fn take_signal(pty: usize) -> Option<u8> {
    let flags = irq_save();
    let sig = pty_at(pty).map_or(0, |p| core::mem::replace(&mut p.signal, 0));
    irq_restore(flags);
    (sig != 0).then_some(sig)
}

/// The session this terminal is the controlling terminal of, and the group
/// in front of it; `None` for no such terminal.
pub fn job(pty: usize) -> Option<(u64, u64)> {
    let flags = irq_save();
    let out = pty_at(pty).map(|p| (p.session, p.front));
    irq_restore(flags);
    out
}

/// Make this the controlling terminal of `session`, with `group` in front.
///
/// A terminal is one session's, and a session has one terminal: refused if
/// the terminal is another session's already, or the session has another
/// terminal. Asking for what is already so is not refused.
pub fn set_session(pty: usize, session: u64, group: u64) -> bool {
    if session == 0 {
        return false;
    }
    let flags = irq_save();
    let free = pty_at(pty).is_some_and(|p| p.session == 0 || p.session == session);
    let mut elsewhere = false;
    let mut at = 0;
    while let Some(i) = ptys().next_used(at) {
        at = i + 1;
        elsewhere |= i != pty && pty_at(i).is_some_and(|p| p.session == session);
    }
    let ok = free && !elsewhere;
    if let Some(p) = pty_at(pty).filter(|p| ok && p.session == 0) {
        p.session = session;
        p.front = group;
    }
    irq_restore(flags);
    ok
}

/// Put `group` in front of this terminal.
pub fn set_front(pty: usize, group: u64) -> bool {
    let flags = irq_save();
    let p = pty_at(pty);
    let ok = p.is_some();
    if let Some(p) = p {
        p.front = group;
    }
    irq_restore(flags);
    ok
}

/// A session's leader has gone, and with it the session's claim on its
/// terminal: the terminal is nobody's, and the next to ask may have it.
pub fn session_gone(session: u64) {
    if session == 0 {
        return;
    }
    let flags = irq_save();
    let mut at = 0;
    while let Some(i) = ptys().next_used(at) {
        at = i + 1;
        if let Some(p) = pty_at(i).filter(|p| p.session == session) {
            p.session = 0;
            p.front = 0;
        }
    }
    irq_restore(flags);
}

pub fn get_termios(pty: usize) -> Option<Termios> {
    let flags = irq_save();
    let out = pty_at(pty).map(|p| p.termios);
    irq_restore(flags);
    out
}

pub fn set_termios(pty: usize, t: &Termios) -> bool {
    let flags = irq_save();
    let found = pty_at(pty);
    let ok = found.is_some();
    if let Some(p) = found {
        p.termios = *t;
        // Leaving canonical mode releases what was gathered: a program that
        // turns it off is asking for the bytes it has already typed, not for
        // them to be held until a newline that will never come.
        if t.c_lflag & ICANON == 0 {
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
    let flags = irq_save();
    let out = pty_at(pty).map(|p| p.size);
    irq_restore(flags);
    out
}

pub fn set_winsize(pty: usize, size: &WinSize) -> bool {
    let flags = irq_save();
    let found = pty_at(pty);
    let ok = found.is_some();
    if let Some(p) = found {
        p.size = *size;
    }
    irq_restore(flags);
    ok
}

/// The pty and end a descriptor names, if it names one.
pub fn of_fd(tid: usize, fd: usize) -> Option<(usize, u8)> {
    if fd >= FD_MOST {
        return None;
    }
    match crate::fdtable::get(tid, fd) {
        FdKind::PtyEnd { pty, end } => Some((pty, end)),
        _ => None,
    }
}
