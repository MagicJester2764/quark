//! A connected Wayland client.
//!
//! Its socket, what it has asked for, and the bytes that have arrived but do
//! not yet make a message.
//!
//! That last part is the whole reason this is a struct rather than a function.
//! A stream is not a message: a read delivers whatever happened to be in the
//! buffer, which may be half of one request or three and a bit. A compositor
//! that treats a read as a message works until a client sends two things
//! quickly, and then fails somewhere unrelated.

use quark_rt::wl::wire;
use quark_rt::syscall;

use crate::clipboard::{self, Mimes, NO_MIMES};
use crate::objects::{self, Kind, Table};
use crate::protocol as proto;
use crate::shell;
use crate::shm;
use crate::surface;

pub const MAX_CLIENTS: usize = 4;
const READ_BUF: usize = 4096;
const WRITE_BUF: usize = 4096;

/// Descriptors received but not yet claimed by a request.
///
/// A descriptor travels beside the byte stream rather than in it, so it cannot
/// be found by parsing: it is matched to a request by *order*, which is how
/// `SCM_RIGHTS` works on Unix and what libwayland's own demarshaller assumes.
/// Four is more outstanding descriptors than any client here sends — the only
/// request that carries one is `wl_shm.create_pool`.
const MAX_PENDING_FDS: usize = 4;

/// Frame callbacks one client may have waiting to be answered. More than one
/// surface may commit between passes, and each may have asked for a frame.
const MAX_DUE: usize = 16;

/// Room to leave before starting an event. Comfortably larger than the biggest
/// this compositor sends, which is `wl_output.geometry` with two strings.
const EVENT_SLACK: usize = 256;

/// A request that breaks the protocol: which object it was on, which of
/// `wl_display.error`'s codes it earns, and what to say about it.
///
/// A protocol error is fatal to the connection by design — the client's idea
/// of the object graph and the compositor's have diverged, and everything
/// after this point would be read against the wrong one — but it is *said*
/// first. A connection that simply stops leaves a client author guessing.
struct Fault {
    object: u32,
    code: u32,
    message: &'static [u8],
}

impl Fault {
    /// An object that is not there, or is not what the request needs.
    fn object(object: u32, message: &'static [u8]) -> Fault {
        Fault { object, code: proto::ERR_INVALID_OBJECT, message }
    }

    /// A request this interface does not have, or cannot be honoured.
    fn method(object: u32, message: &'static [u8]) -> Fault {
        Fault { object, code: proto::ERR_INVALID_METHOD, message }
    }

    /// Nothing left to make it out of.
    fn memory(object: u32, message: &'static [u8]) -> Fault {
        Fault { object, code: proto::ERR_NO_MEMORY, message }
    }
}

type Handled = Result<(), Fault>;

/// One request's arguments: where they are, and where they end.
///
/// Every argument is read through this, so none is read past the size the
/// message's own header gave. Reading straight from the buffer took the next
/// message's bytes — or bytes from the read before — as arguments a client
/// had not sent, which is a client choosing what the compositor parses.
#[derive(Clone, Copy)]
struct Args {
    object: u32,
    at: usize,
    end: usize,
}

/// The next word, or a fault: a request too short for what it says it is.
fn take_u32(buf: &[u8], a: &mut Args) -> Result<u32, Fault> {
    if a.at + 4 > a.end {
        return Err(Fault::method(a.object, b"a request shorter than its arguments"));
    }
    let v = u32::from_le_bytes([buf[a.at], buf[a.at + 1], buf[a.at + 2], buf[a.at + 3]]);
    a.at += 4;
    Ok(v)
}

fn take_i32(buf: &[u8], a: &mut Args) -> Result<i32, Fault> {
    take_u32(buf, a).map(|v| v as i32)
}

/// An id for an object the client is making. Zero is not one.
fn take_new_id(buf: &[u8], a: &mut Args) -> Result<u32, Fault> {
    match take_u32(buf, a)? {
        0 => Err(Fault::object(a.object, b"a new object with no id")),
        id => Ok(id),
    }
}

/// An id for an object the client already has. Zero means "none", and is a
/// fault where the request does not allow it.
fn take_object(buf: &[u8], a: &mut Args, nullable: bool) -> Result<u32, Fault> {
    match take_u32(buf, a)? {
        0 if !nullable => Err(Fault::object(a.object, b"a null object where one is needed")),
        id => Ok(id),
    }
}

/// The string at the cursor, without its NUL.
///
/// The length is the client's and is checked against the message rather than
/// believed: it is the one argument whose size the client chooses.
fn take_str<'a>(buf: &'a [u8], a: &mut Args, nullable: bool) -> Result<&'a [u8], Fault> {
    let len = take_u32(buf, a)? as usize;
    if len == 0 {
        return if nullable {
            Ok(&buf[..0])
        } else {
            Err(Fault::method(a.object, b"a null string where one is needed"))
        };
    }
    let padded = wire::pad4(len);
    if a.at + padded > a.end {
        return Err(Fault::method(a.object, b"a string longer than the request holding it"));
    }
    let start = a.at;
    a.at += padded;
    if buf[start + len - 1] != 0 {
        return Err(Fault::method(a.object, b"a string that does not end in a nul"));
    }
    Ok(&buf[start..start + len - 1])
}

/// How many requests an interface has, so that an opcode beyond them is
/// refused rather than quietly doing nothing. A client sending one has
/// mistaken the object for something else, and the requests after it will be
/// read against the wrong interface too.
fn request_count(kind: Kind) -> u16 {
    match kind {
        Kind::Display => 2,
        Kind::Registry => 1,
        Kind::Compositor => 2,
        Kind::Shm => 2,
        Kind::ShmPool { .. } => 3,
        Kind::Buffer { .. } => 1,
        Kind::Surface { .. } => 11,
        Kind::Region => 3,
        Kind::Output => 1,
        Kind::XdgWmBase => 4,
        Kind::XdgSurface { .. } => 5,
        Kind::XdgToplevel { .. } => 14,
        Kind::Seat => 4,
        Kind::Keyboard => 1,
        Kind::Pointer => 2,
        Kind::Decoration => 2,
        Kind::ToplevelDecoration => 3,
        // The clipboard's manager has two requests and the primary's has
        // three, and the rest come from the table that says what each
        // protocol's numbers are.
        Kind::DataDeviceManager { which } => {
            if which as usize == clipboard::PRIMARY { 3 } else { 2 }
        }
        Kind::DataSource { which } => proto::SELECTION_WIRE[which as usize].source_requests,
        Kind::DataDevice { which } => proto::SELECTION_WIRE[which as usize].device_requests,
        Kind::DataOffer { which } => proto::SELECTION_WIRE[which as usize].offer_requests,
        // A callback answers and is gone; `None` is the touch device this
        // compositor records so that destroying it names something.
        Kind::Callback => 0,
        Kind::None => 1,
    }
}

pub struct Client {
    pub used: bool,
    /// Which entry of the compositor's client table this is. Surfaces are kept
    /// in a table of their own and remember it, so that a disconnection can
    /// take everything the client had with it.
    pub slot: usize,
    /// Our end of the socketpair. The client holds the other.
    pub fd: usize,
    pub tid: usize,
    pub objects: Table,
    rbuf: [u8; READ_BUF],
    rlen: usize,
    wbuf: [u8; WRITE_BUF],
    wlen: usize,
    fds: [usize; MAX_PENDING_FDS],
    nfds: usize,
    /// Frame callbacks that have come due but not yet been sent, and the tick
    /// the last batch went out on.
    ///
    /// A frame callback means "the frame you committed is on the screen; draw
    /// the next one", so a client that gets one immediately draws again — and
    /// answering inside `commit` means the compositor never gets back to its
    /// own loop between frames. Holding them until the next pass is what turns
    /// "as fast as the client can loop" into "as fast as this compositor
    /// presents", which is what the callback is supposed to mean.
    due: [u32; MAX_DUE],
    ndue: usize,
    fired_at: u64,
    /// The `wl_output` this client bound, if it did. A surface's `enter` names
    /// it, so a client that never bound one cannot be told where its surfaces
    /// are — there is no object to say it with.
    pub output: u32,
    /// The `wl_keyboard` this client asked for, if it did. Events go to
    /// objects, and a client that never took a keyboard has none to send to.
    keyboard: u32,
    /// The `wl_pointer`, likewise.
    pointer: u32,
    /// The `wl_data_device` and its primary-selection twin, which is where
    /// each selection is announced to this client. Indexed by
    /// `clipboard::CLIPBOARD` and `PRIMARY`, as every pair below is.
    data_device: [u32; clipboard::KINDS],
    /// The source it is building for each, and what it has offered on it.
    ///
    /// One at a time per selection: a client offers types and then sets the
    /// selection, and nothing here needs two sources part-built at once.
    building: [u32; clipboard::KINDS],
    building_mimes: [Mimes; clipboard::KINDS],
    /// The offer this client was last given of each, so that a `receive` on it
    /// can be matched to the selection it names.
    offer: [u32; clipboard::KINDS],
    /// A descriptor to attach to the next flush.
    ///
    /// `wl_keyboard.keymap` carries one, and the kernel queues a descriptor
    /// ahead of the bytes of the write it rode on — so the event it belongs to
    /// has to be the only thing in the buffer when that write happens.
    /// libwayland pops descriptors in message order, and one outstanding at a
    /// time is what keeps the two orders the same.
    pending_fd: usize,
    /// An argument did not fit in the write buffer, so the event being built
    /// is not the event it claims to be.
    ///
    /// A half-written event is worse than a missing one: its header says a
    /// size, the arguments after it are somebody else's, and the client parses
    /// the rest of the connection out of step. `end` rolls the whole event back
    /// when this is set, so the stream stays valid whatever else is lost.
    wfail: bool,
}

pub const NO_CLIENT: Client = Client {
    used: false,
    slot: 0,
    fd: 0,
    tid: 0,
    objects: objects::EMPTY,
    rbuf: [0; READ_BUF],
    rlen: 0,
    wbuf: [0; WRITE_BUF],
    wlen: 0,
    fds: [0; MAX_PENDING_FDS],
    nfds: 0,
    due: [0; MAX_DUE],
    ndue: 0,
    fired_at: 0,
    output: 0,
    keyboard: 0,
    pointer: 0,
    data_device: [0; clipboard::KINDS],
    building: [0; clipboard::KINDS],
    building_mimes: [NO_MIMES; clipboard::KINDS],
    offer: [0; clipboard::KINDS],
    pending_fd: usize::MAX,
    wfail: false,
};

impl Client {
    pub fn open(&mut self, slot: usize, fd: usize, tid: usize) {
        self.used = true;
        self.slot = slot;
        self.fd = fd;
        self.tid = tid;
        self.objects.clear();
        self.rlen = 0;
        self.wlen = 0;
        self.nfds = 0;
        self.keyboard = 0;
        self.pointer = 0;
        self.output = 0;
        self.data_device = [0; clipboard::KINDS];
        self.building = [0; clipboard::KINDS];
        self.building_mimes = [NO_MIMES; clipboard::KINDS];
        self.offer = [0; clipboard::KINDS];
        self.pending_fd = usize::MAX;
        // wl_display is object 1 and exists before anything is asked for.
        self.objects.insert(proto::DISPLAY_ID, Kind::Display);
    }

    pub fn close(&mut self) {
        if self.used {
            let _ = syscall::sys_fd_close(self.fd);
            surface::forget_client(self.slot);
            shm::forget_client(self.tid);
            clipboard::forget_client(self.slot);
        }
        // Descriptors that arrived and were never claimed by a request. The
        // client is gone and nothing will ever ask for them, so they are ours
        // to drop — and not dropping them holds the memory behind them for the
        // life of the compositor.
        for i in 0..self.nfds {
            let _ = syscall::sys_fd_close(self.fds[i]);
        }
        self.nfds = 0;
        if self.pending_fd != usize::MAX {
            let _ = syscall::sys_fd_close(self.pending_fd);
            self.pending_fd = usize::MAX;
        }
        self.keyboard = 0;
        self.pointer = 0;
        self.output = 0;
        self.data_device = [0; clipboard::KINDS];
        self.offer = [0; clipboard::KINDS];
        self.used = false;
        self.fd = 0;
        self.tid = 0;
        self.objects.clear();
        self.rlen = 0;
        self.wlen = 0;
    }

    /// Take the oldest descriptor a client has sent, if any.
    fn take_fd(&mut self) -> Option<usize> {
        if self.nfds == 0 {
            return None;
        }
        let fd = self.fds[0];
        self.fds.copy_within(1..self.nfds, 0);
        self.nfds -= 1;
        Some(fd)
    }

    fn push_fd(&mut self, fd: usize) {
        if self.nfds < MAX_PENDING_FDS {
            self.fds[self.nfds] = fd;
            self.nfds += 1;
        } else {
            // More outstanding descriptors than any request here can want.
            // Dropping is better than growing a queue on a client's say-so.
            let _ = syscall::sys_fd_close(fd);
        }
    }

    /// Start an event. Returns where its arguments go, or `None` if the write
    /// buffer is full.
    fn begin(&mut self, object: u32, opcode: u16) -> Option<usize> {
        // Make room before starting rather than discovering halfway through.
        // The send is non-blocking, so this may not free anything — but when it
        // does, an event that would have been rolled back goes out instead.
        if WRITE_BUF - self.wlen < EVENT_SLACK {
            self.flush();
        }
        if self.wlen + wire::HEADER > WRITE_BUF {
            return None;
        }
        self.wfail = false;
        let at = self.wlen;
        wire::put_header(
            &mut self.wbuf[at..],
            wire::Header { object, opcode, size: 0 },
        );
        self.wlen = at + wire::HEADER;
        Some(at)
    }

    /// Finish the event started at `at`, filling in the size the header could
    /// not know when it was written.
    ///
    /// The size is patched in place rather than the header rebuilt. Rebuilding
    /// it meant reading the opcode back out with `parse_header`, which refuses
    /// a size smaller than a header — and the placeholder `begin` wrote is
    /// zero. Every event went out with opcode 0, which for `delete_id` is
    /// `wl_display.error` and reads to a client as the compositor giving up.
    fn end(&mut self, at: usize) {
        if self.wfail {
            self.wlen = at;
            self.wfail = false;
            return;
        }
        let size = (self.wlen - at) as u16;
        let lo = self.wbuf[at + 4];
        let hi = self.wbuf[at + 5];
        let opcode = u16::from_le_bytes([lo, hi]);
        let word = ((size as u32) << 16) | opcode as u32;
        self.wbuf[at + 4..at + 8].copy_from_slice(&word.to_le_bytes());
    }

    fn arg_u32(&mut self, v: u32) -> bool {
        if self.wlen + 4 > WRITE_BUF {
            self.wfail = true;
            return false;
        }
        wire::put_u32(&mut self.wbuf, self.wlen, v);
        self.wlen += 4;
        true
    }

    /// An array argument: a length and that many bytes, padded to four.
    ///
    /// Unlike a string it carries no NUL, which is the whole difference between
    /// the two on the wire and the reason they are separate functions.
    fn arg_array(&mut self, bytes: &[u8]) -> bool {
        let need = 4 + wire::pad4(bytes.len());
        if self.wlen + need > WRITE_BUF {
            self.wfail = true;
            return false;
        }
        wire::put_u32(&mut self.wbuf, self.wlen, bytes.len() as u32);
        self.wbuf[self.wlen + 4..self.wlen + 4 + bytes.len()].copy_from_slice(bytes);
        for i in bytes.len()..wire::pad4(bytes.len()) {
            self.wbuf[self.wlen + 4 + i] = 0;
        }
        self.wlen += need;
        true
    }

    fn arg_str(&mut self, s: &[u8]) -> bool {
        let need = 4 + wire::pad4(s.len() + 1);
        if self.wlen + need > WRITE_BUF {
            self.wfail = true;
            return false;
        }
        let n = wire::put_str(&mut self.wbuf, self.wlen, s);
        self.wlen += n;
        true
    }

    /// Push everything queued down the socket.
    pub fn flush(&mut self) {
        if self.wlen == 0 {
            return;
        }
        // Non-blocking on purpose. A client that stops reading must not be
        // able to park the compositor, which is serving everybody else.
        let pass = if self.pending_fd == usize::MAX { None } else { Some(self.pending_fd) };
        let n = match syscall::sys_fd_send_nb(self.fd, &self.wbuf[..self.wlen], pass) {
            Err(()) => {
                self.wlen = 0; // the client has gone; the bytes have nowhere to go
                if self.pending_fd != usize::MAX {
                    let _ = syscall::sys_fd_close(self.pending_fd);
                    self.pending_fd = usize::MAX;
                }
                return;
            }
            Ok(None) => 0,
            Ok(Some(n)) => n,
        };
        if self.pending_fd != usize::MAX {
            // Sent, and ours to let go of: the peer took a reference when the
            // kernel queued it, so this only drops our own.
            let _ = syscall::sys_fd_close(self.pending_fd);
            self.pending_fd = usize::MAX;
        }
        // A short write means the stream is full. Dropping the remainder would
        // desynchronise it, so keep it and try again next time.
        if n < self.wlen {
            self.wbuf.copy_within(n..self.wlen, 0);
            self.wlen -= n;
        } else {
            self.wlen = 0;
        }
    }

    /// Read whatever has arrived and act on every whole message in it.
    ///
    /// Returns false when the client has gone.
    pub fn dispatch(&mut self) -> bool {
        if self.rlen < READ_BUF {
            let mut scratch = [0u8; READ_BUF];
            let want = READ_BUF - self.rlen;
            let at = if self.nfds < MAX_PENDING_FDS { Some(syscall::ANY_FD) } else { None };
            match syscall::sys_fd_recv_nb(self.fd, &mut scratch[..want], at) {
                Err(()) => return false,
                Ok(Some((0, None))) => return false, // end of file: the client has gone
                Ok(Some((got, fd))) => {
                    if let Some(fd) = fd {
                        self.push_fd(fd);
                    }
                    self.rbuf[self.rlen..self.rlen + got].copy_from_slice(&scratch[..got]);
                    self.rlen += got;
                }
                Ok(None) => {} // nothing new; act on what is already buffered
            }
            // One receive collects at most one descriptor, so a client that
            // sent two requests carrying one each would have the second's
            // arrive a whole read late — and be matched to the wrong request.
            // A zero-length receive asks for a descriptor and nothing else.
            while self.nfds < MAX_PENDING_FDS {
                match syscall::sys_fd_recv_nb(self.fd, &mut [], Some(syscall::ANY_FD)) {
                    Ok(Some((_, Some(fd)))) => self.push_fd(fd),
                    _ => break,
                }
            }
        }

        let mut at = 0usize;
        while at + wire::HEADER <= self.rlen {
            // A malformed header cannot be skipped past, because the size that
            // would say how far is the part that is wrong. Say so and stop.
            let Some(h) = wire::parse_header(&self.rbuf[at..self.rlen]) else {
                return self.protocol_error(
                    proto::DISPLAY_ID,
                    proto::ERR_INVALID_METHOD,
                    b"a message shorter than its own header",
                );
            };
            let size = h.size as usize;
            if size > READ_BUF {
                // Longer than this can ever hold, so waiting for the rest of
                // it would be waiting for ever — with the buffer full, the
                // connection would go quiet rather than wrong.
                return self.protocol_error(
                    proto::DISPLAY_ID,
                    proto::ERR_INVALID_METHOD,
                    b"a message longer than the buffer that reads it",
                );
            }
            if size % 4 != 0 {
                return self.protocol_error(
                    proto::DISPLAY_ID,
                    proto::ERR_INVALID_METHOD,
                    b"a message whose size is not a multiple of four",
                );
            }
            if at + size > self.rlen {
                break; // the rest of it has not arrived
            }
            if !self.handle(h, at) {
                return false;
            }
            at += size;
        }
        if at > 0 {
            self.rbuf.copy_within(at..self.rlen, 0);
            self.rlen -= at;
        }
        self.flush();
        true
    }

    fn handle(&mut self, h: wire::Header, at: usize) -> bool {
        let mut args =
            Args { object: h.object, at: at + wire::HEADER, end: at + h.size as usize };
        match self.request(h, &mut args) {
            Ok(()) => true,
            Err(f) => self.protocol_error(f.object, f.code, f.message),
        }
    }

    fn request(&mut self, h: wire::Header, args: &mut Args) -> Handled {
        let Some(kind) = self.objects.get(h.object) else {
            // An object this client never made, or one it has destroyed. The
            // display is what the error names, since there is no object to
            // name it on.
            return Err(Fault::object(
                proto::DISPLAY_ID,
                b"a request for an object that is not there",
            ));
        };
        if h.opcode >= request_count(kind) {
            return Err(Fault::method(h.object, b"a request this interface does not have"));
        }
        match kind {
            Kind::Display => self.display_request(h.opcode, args),
            Kind::Registry => self.registry_request(h.opcode, args),
            Kind::Shm => self.shm_request(h.object, h.opcode, args),
            Kind::ShmPool { pool } => self.pool_request(h.object, pool, h.opcode, args),
            Kind::Buffer { buffer } => {
                if h.opcode == proto::BUFFER_DESTROY {
                    shm::destroy_buffer(buffer);
                    self.objects.remove(h.object);
                }
                Ok(())
            }
            Kind::Compositor => self.compositor_request(h.opcode, args),
            Kind::Surface { surface } => self.surface_request(h.object, surface, h.opcode, args),
            Kind::Region => {
                if h.opcode == proto::REGION_DESTROY {
                    self.objects.remove(h.object);
                } else {
                    // add and subtract, each a rectangle: read to check the
                    // request is as long as it says, and then let go. What is
                    // opaque or takes input changes nothing here.
                    for _ in 0..4 {
                        take_i32(&self.rbuf, args)?;
                    }
                }
                Ok(())
            }
            Kind::XdgWmBase => self.wm_base_request(h.object, h.opcode, args),
            Kind::XdgSurface { surface } => {
                self.xdg_surface_request(h.object, surface, h.opcode, args)
            }
            Kind::XdgToplevel { surface } => {
                self.toplevel_request(h.object, surface, h.opcode, args)
            }
            Kind::Seat => self.seat_request(h.object, h.opcode, args),
            Kind::Keyboard => {
                if h.opcode == proto::KEYBOARD_RELEASE {
                    self.objects.remove(h.object);
                    if self.keyboard == h.object {
                        self.keyboard = 0;
                    }
                }
                Ok(())
            }
            Kind::Pointer => {
                if h.opcode == proto::POINTER_SET_CURSOR {
                    // set_cursor(serial, surface, hotspot_x, hotspot_y). The
                    // compositor draws its own pointer, so this is read to
                    // check it and then let go.
                    take_u32(&self.rbuf, args)?;
                    let surface = take_object(&self.rbuf, args, true)?;
                    take_i32(&self.rbuf, args)?;
                    take_i32(&self.rbuf, args)?;
                    if surface != 0
                        && !matches!(self.objects.get(surface), Some(Kind::Surface { .. }))
                    {
                        return Err(Fault::object(h.object, b"set_cursor: not a surface"));
                    }
                    return Ok(());
                }
                if h.opcode == proto::POINTER_RELEASE {
                    self.objects.remove(h.object);
                    if self.pointer == h.object {
                        self.pointer = 0;
                    }
                }
                Ok(())
            }
            Kind::Decoration => match h.opcode {
                proto::DECORATION_GET_TOPLEVEL => {
                    // get_toplevel_decoration(new_id, toplevel)
                    let id = take_new_id(&self.rbuf, args)?;
                    let toplevel = take_object(&self.rbuf, args, false)?;
                    if !matches!(self.objects.get(toplevel), Some(Kind::XdgToplevel { .. })) {
                        return Err(Fault::object(
                            h.object,
                            b"get_toplevel_decoration: not a toplevel",
                        ));
                    }
                    if !self.objects.insert(id, Kind::ToplevelDecoration) {
                        return Err(self.no_room(id));
                    }
                    self.decoration_configure(id);
                    Ok(())
                }
                proto::DECORATION_DESTROY => {
                    self.objects.remove(h.object);
                    Ok(())
                }
                _ => Ok(()),
            },
            Kind::ToplevelDecoration => {
                match h.opcode {
                    // A client may ask for either mode and is told which it
                    // gets. The answer does not depend on the question: the
                    // frame is drawn before the client's pixels are, and there
                    // is no arrangement here in which it is not drawn.
                    proto::TOPLEVEL_DECORATION_SET_MODE
                    | proto::TOPLEVEL_DECORATION_UNSET_MODE => {
                        self.decoration_configure(h.object);
                        Ok(())
                    }
                    proto::TOPLEVEL_DECORATION_DESTROY => {
                        self.objects.remove(h.object);
                        Ok(())
                    }
                    _ => Ok(()),
                }
            }
            Kind::DataDeviceManager { which } => {
                self.ddm_request(which as usize, h.opcode, args)
            }
            Kind::DataSource { which } => {
                self.data_source_request(which as usize, h.object, h.opcode, args)
            }
            Kind::DataDevice { which } => {
                self.data_device_request(which as usize, h.object, h.opcode, args)
            }
            Kind::DataOffer { which } => {
                self.data_offer_request(which as usize, h.object, h.opcode, args)
            }
            // An output's one request, and the touch device this compositor
            // records but never speaks to, are both destructors.
            Kind::Output | Kind::None => {
                self.objects.remove(h.object);
                Ok(())
            }
            // Nothing is a request on a callback; `request_count` said so.
            Kind::Callback => Ok(()),
        }
    }

    /// Take away every object of this client's that named surface `idx`,
    /// which has just been freed.
    ///
    /// The protocol says a role object is destroyed before the surface under
    /// it, and a client that does it the other way round would otherwise be
    /// left holding names for a slot the next client's surface takes.
    fn forget_surface(&mut self, idx: usize) {
        while let Some(id) = self.objects.find(|k| {
            matches!(
                k,
                Kind::Surface { surface }
                    | Kind::XdgSurface { surface }
                    | Kind::XdgToplevel { surface } if *surface == idx
            )
        }) {
            self.objects.remove(id);
        }
    }

    /// An id a client cannot have: in use already, the compositor's to give,
    /// or one more than its table holds.
    fn no_room(&self, id: u32) -> Fault {
        if self.objects.get(id).is_some() || id >= objects::CLIENT_ID_MAX {
            Fault::object(proto::DISPLAY_ID, b"an id that is taken, or not the client's to give")
        } else {
            Fault::memory(proto::DISPLAY_ID, b"too many objects")
        }
    }

    fn decoration_configure(&mut self, id: u32) {
        if let Some(a) = self.begin(id, proto::TOPLEVEL_DECORATION_CONFIGURE) {
            self.arg_u32(proto::DECORATION_MODE_SERVER_SIDE);
            self.end(a);
        }
        self.flush();
    }

    /// Either manager. `destroy` is the primary's third request and takes no
    /// arguments, so it is answered before an id is read for it.
    fn ddm_request(&mut self, which: usize, opcode: u16, args: &mut Args) -> Handled {
        if which == clipboard::PRIMARY && opcode == 2 {
            self.objects.remove(args.object);
            return Ok(());
        }
        let id = take_new_id(&self.rbuf, args)?;
        match opcode {
            proto::SELECTION_CREATE_SOURCE => {
                if !self.objects.insert(id, Kind::DataSource { which: which as u8 }) {
                    return Err(self.no_room(id));
                }
                self.building[which] = id;
                self.building_mimes[which] = NO_MIMES;
                Ok(())
            }
            proto::SELECTION_GET_DEVICE => {
                let seat = take_object(&self.rbuf, args, false)?;
                if !matches!(self.objects.get(seat), Some(Kind::Seat)) {
                    return Err(Fault::object(args.object, b"get_data_device: not a seat"));
                }
                if !self.objects.insert(id, Kind::DataDevice { which: which as u8 }) {
                    return Err(self.no_room(id));
                }
                self.data_device[which] = id;
                // A device made while this client already has focus should hear
                // about the selection now, not the next time focus moves.
                if crate::seat::focus_is_mine(self.slot) {
                    self.announce_selection(which);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn data_source_request(
        &mut self,
        which: usize,
        object: u32,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        let wire = &proto::SELECTION_WIRE[which];
        match opcode {
            op if op == wire.source_offer => {
                let pushed = {
                    let mime = take_str(&self.rbuf, args, false)?;
                    if object != self.building[which] {
                        // A type offered on a source that is not the one being
                        // built. Nothing here can hold two part-built sources,
                        // and silently dropping it would leave a client
                        // believing it had offered something it had not.
                        return Err(Fault::memory(object, b"one data source at a time"));
                    }
                    self.building_mimes[which].push(mime)
                };
                if !pushed {
                    return Err(Fault::memory(object, b"too many mime types, or one too long"));
                }
                Ok(())
            }
            op if op == wire.source_destroy => {
                if clipboard::release(which, self.slot, object) {
                    crate::announce_selection_to_focus(which);
                }
                if self.building[which] == object {
                    self.building[which] = 0;
                    self.building_mimes[which] = NO_MIMES;
                }
                self.objects.remove(object);
                Ok(())
            }
            // set_actions belongs to drag and drop, which this manager does
            // not advertise a version with.
            _ => Ok(()),
        }
    }

    fn data_device_request(
        &mut self,
        which: usize,
        object: u32,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        let wire = &proto::SELECTION_WIRE[which];
        match opcode {
            op if op == wire.device_set_selection => {
                // set_selection(source, serial). A null source clears it.
                let source = take_object(&self.rbuf, args, true)?;
                let _serial = take_u32(&self.rbuf, args)?;
                if source == 0 {
                    if let Some((slot, id)) = clipboard::owner(which) {
                        if slot == self.slot {
                            clipboard::release(which, slot, id);
                            crate::announce_selection_to_focus(which);
                        }
                    }
                    return Ok(());
                }
                // A source of the *other* protocol is not this protocol's
                // source, however much it looks like one: setting the
                // clipboard from a primary source is a client that has
                // confused its two selections.
                if !matches!(self.objects.get(source),
                             Some(Kind::DataSource { which: w }) if w as usize == which)
                {
                    return Err(Fault::object(object, b"set_selection: not a data source"));
                }
                let mimes = self.building_mimes[which];
                let previous = clipboard::take(which, self.slot, source, mimes);
                self.building[which] = 0;
                self.building_mimes[which] = NO_MIMES;
                // The client that had it is told, because a source still
                // offering something nobody can reach is a program waiting for
                // a request that will never come.
                if let Some((slot, id)) = previous {
                    crate::cancel_source(slot, id, which);
                }
                crate::announce_selection_to_focus(which);
                Ok(())
            }
            op if op == wire.device_destroy => {
                self.objects.remove(object);
                if self.data_device[which] == object {
                    self.data_device[which] = 0;
                }
                Ok(())
            }
            proto::DATA_DEVICE_START_DRAG if which == clipboard::CLIPBOARD => {
                // Drag and drop is version 2 and up, and this manager is
                // version 1: the request exists so that its arguments are
                // read and refused rather than ignored.
                Err(Fault::method(object, b"no drag and drop"))
            }
            _ => Ok(()),
        }
    }

    fn data_offer_request(
        &mut self,
        which: usize,
        object: u32,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        let wire = &proto::SELECTION_WIRE[which];
        match opcode {
            op if op == wire.offer_receive => {
                // receive(mime_type, fd). The descriptor is the point: it is a
                // pipe this client made, and the compositor's whole part in the
                // transfer is handing it to the other end.
                // Copied out of the read buffer before anything else touches
                // it: the name has to outlive the borrow, and taking the
                // descriptor needs the buffer released.
                let mut name = [0u8; clipboard::MIME_LEN];
                let len = {
                    let m = take_str(&self.rbuf, args, false)?;
                    let len = m.len().min(clipboard::MIME_LEN);
                    name[..len].copy_from_slice(&m[..len]);
                    m.len()
                };
                let Some(fd) = self.take_fd() else {
                    return Err(Fault::method(object, b"receive without a descriptor"));
                };
                // A name longer than any that can be offered matches nothing,
                // which is the same answer as a name nobody offered.
                let mime = if len <= clipboard::MIME_LEN { &name[..len] } else { &[][..] };
                if object != self.offer[which] || !clipboard::mimes(which).has(mime) {
                    // A stale offer, or a type nobody promised. Closing the
                    // descriptor is what tells the client to stop reading:
                    // leaving it open would hang it on a pipe with no writer.
                    let _ = syscall::sys_fd_close(fd);
                    return Ok(());
                }
                let Some((slot, source)) = clipboard::owner(which) else {
                    let _ = syscall::sys_fd_close(fd);
                    return Ok(());
                };
                crate::send_to_source(slot, source, mime, fd, which);
                Ok(())
            }
            op if op == wire.offer_destroy => {
                if self.offer[which] == object {
                    self.offer[which] = 0;
                }
                self.objects.remove(object);
                Ok(())
            }
            // `accept` says which type a drag would take, and there are no
            // drags here; `finish` and `set_actions` are version 3. The
            // primary selection has neither, and nothing but receive and
            // destroy to mistake them for.
            proto::DATA_OFFER_ACCEPT if which == clipboard::CLIPBOARD => {
                take_u32(&self.rbuf, args)?;
                take_str(&self.rbuf, args, true)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Tell this client what is on the clipboard, if it has a device to hear it.
    ///
    /// The offer object is named by the *compositor*: the client did not ask
    /// for it and has nothing to name it with, which is what the server half of
    /// the id space is for.
    pub fn announce_selection(&mut self, which: usize) {
        let device = self.data_device[which];
        if device == 0 {
            return;
        }
        let wire = &proto::SELECTION_WIRE[which];
        // The previous offer is finished with, but it is the client's object
        // to destroy: it stays in the table until it does, and `receive` on
        // it is refused meanwhile because it is no longer the offer. Taking
        // it away here would make a client that destroys its old offer — as a
        // client is supposed to — name an object that is not there.
        self.offer[which] = 0;
        if !clipboard::is_held(which) {
            // A null offer means "there is nothing", which is a real thing to
            // say: a client that is never told stops trusting what it has.
            if let Some(a) = self.begin(device, wire.device_selection) {
                self.arg_u32(0);
                self.end(a);
            }
            self.flush();
            return;
        }
        let kind = Kind::DataOffer { which: which as u8 };
        let id = match self.objects.allocate(kind) {
            Some(id) => id,
            None => {
                // Full of offers the client never destroyed. One of them goes:
                // there is nothing left to do with an offer that is not the
                // selection.
                if let Some(stale) = self.objects.find(|k| matches!(k, Kind::DataOffer { .. })) {
                    self.objects.remove(stale);
                }
                match self.objects.allocate(kind) {
                    Some(id) => id,
                    None => return,
                }
            }
        };
        self.offer[which] = id;
        if let Some(a) = self.begin(device, wire.device_data_offer) {
            self.arg_u32(id);
            self.end(a);
        }
        let mimes = clipboard::mimes(which);
        for i in 0..mimes.len() {
            let Some(name) = mimes.get(i) else { break };
            if let Some(a) = self.begin(id, wire.offer_offer) {
                self.arg_str(name);
                self.end(a);
            }
        }
        if let Some(a) = self.begin(device, wire.device_selection) {
            self.arg_u32(id);
            self.end(a);
        }
        self.flush();
    }

    /// Ask this client for the bytes, handing it the receiver's pipe.
    pub fn source_send(&mut self, source: u32, mime: &[u8], fd: usize, which: usize) {
        // Alone in the buffer: a descriptor rides with the write it was
        // attached to, and libwayland pops descriptors in message order.
        self.flush();
        if let Some(a) = self.begin(source, proto::SELECTION_WIRE[which].source_send) {
            self.arg_str(mime);
            self.end(a);
        }
        self.pending_fd = fd;
        self.flush();
    }

    /// Tell this client its source is no longer the selection.
    pub fn source_cancelled(&mut self, source: u32, which: usize) {
        if let Some(a) = self.begin(source, proto::SELECTION_WIRE[which].source_cancelled) {
            self.end(a);
        }
        self.flush();
    }

    /// The keyboard object this client asked for, if it did.
    pub fn keyboard_id(&self) -> Option<u32> {
        if self.keyboard == 0 { None } else { Some(self.keyboard) }
    }

    /// The pointer object this client asked for, if it did.
    pub fn pointer_id(&self) -> Option<u32> {
        if self.pointer == 0 { None } else { Some(self.pointer) }
    }

    /// Ask this client's toplevel to be a size, with the states that go with
    /// the asking.
    ///
    /// The pair is the round trip the shell is built on: the toplevel's
    /// configure carries the size and the state array, and the xdg_surface's
    /// carries the serial that has to come back. Sending one without the other
    /// leaves a client waiting for a serial that never arrives.
    pub fn configure_toplevel(&mut self, surface_idx: usize, w: u32, h: u32, states: &[u32]) {
        let Some(s) = surface::get(surface_idx) else {
            return;
        };
        if s.toplevel == 0 || s.xdg_surface == 0 {
            return; // a surface that is not a toplevel has nothing to configure
        }
        let (toplevel, xdg_surface) = (s.toplevel, s.xdg_surface);
        if let Some(a) = self.begin(toplevel, proto::TOPLEVEL_CONFIGURE) {
            self.arg_u32(w);
            self.arg_u32(h);
            // The state array is an array of four-byte values, not of bytes:
            // what goes on the wire is a length in bytes and that many, so the
            // numbers are laid out here and the length follows from them.
            let mut bytes = [0u8; 4 * proto::MAX_STATES];
            let n = states.len().min(proto::MAX_STATES);
            for (i, st) in states.iter().take(n).enumerate() {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&st.to_le_bytes());
            }
            self.arg_array(&bytes[..n * 4]);
            self.end(a);
        }
        let serial = shell::begin_configure(surface_idx);
        if let Some(a) = self.begin(xdg_surface, proto::XDG_SURFACE_CONFIGURE) {
            self.arg_u32(serial);
            self.end(a);
        }
        self.flush();
    }

    /// Tell this client which of its surfaces are on the output, and which
    /// have stopped being.
    ///
    /// A client that never bound a `wl_output` is told nothing — there is no
    /// object to name — and hears about every surface it has the moment it
    /// binds one, which is the ordinary order for a toolkit that asks for the
    /// registry, makes a window, and only then looks at the outputs.
    pub fn reconcile_outputs(&mut self) {
        if self.output == 0 {
            return;
        }
        let output = self.output;
        while let Some((idx, visible)) = surface::output_change(self.slot) {
            let Some(id) = self.surface_id(idx) else {
                // No object for it: nothing to address, and nothing to say.
                surface::set_entered(idx, visible);
                continue;
            };
            let opcode = if visible { proto::SURFACE_ENTER } else { proto::SURFACE_LEAVE };
            if let Some(a) = self.begin(id, opcode) {
                self.arg_u32(output);
                self.end(a);
            }
            surface::set_entered(idx, visible);
        }
        self.flush();
    }

    /// Ask a client to close.
    ///
    /// `xdg_toplevel.close` has no reply and no force behind it: the client
    /// decides. A compositor that killed the connection instead would be one
    /// where clicking the box loses whatever the program was holding.
    pub fn send_close(&mut self, surface_idx: usize) {
        let Some(s) = surface::get(surface_idx) else {
            return;
        };
        if s.toplevel == 0 {
            return;
        }
        if let Some(a) = self.begin(s.toplevel, proto::TOPLEVEL_CLOSE) {
            self.end(a);
        }
        self.flush();
    }

    /// End a group of pointer events.
    ///
    /// Version 5 and up only. Below it there is no such event, and a client
    /// treats each event as its own group — which is exactly what the events
    /// mean when nothing says otherwise, so nothing is lost by saying nothing.
    pub fn pointer_frame(&mut self, id: u32) {
        if self.objects.version_of(id) < proto::POINTER_FRAME_SINCE {
            return;
        }
        if let Some(a) = self.begin(id, proto::POINTER_FRAME) {
            self.end(a);
        }
    }

    pub fn pointer_enter(&mut self, id: u32, surface_idx: usize, x: i32, y: i32) {
        let Some(surface_id) = self.surface_id(surface_idx) else {
            return;
        };
        if let Some(a) = self.begin(id, proto::POINTER_ENTER) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(surface_id);
            self.arg_u32(crate::seat::fixed(x));
            self.arg_u32(crate::seat::fixed(y));
            self.end(a);
        }
        self.pointer_frame(id);
        self.flush();
    }

    /// As with the keyboard, the surface is passed rather than read back: by
    /// the time a leave is sent the pointer is already somewhere else.
    pub fn pointer_leave(&mut self, id: u32, surface_idx: usize) {
        let Some(surface_id) = self.surface_id(surface_idx) else {
            return;
        };
        if let Some(a) = self.begin(id, proto::POINTER_LEAVE) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(surface_id);
            self.end(a);
        }
        self.pointer_frame(id);
        self.flush();
    }

    pub fn pointer_motion(&mut self, id: u32, x: i32, y: i32) {
        if let Some(a) = self.begin(id, proto::POINTER_MOTION) {
            self.arg_u32(crate::now_ms());
            self.arg_u32(crate::seat::fixed(x));
            self.arg_u32(crate::seat::fixed(y));
            self.end(a);
        }
        self.pointer_frame(id);
        self.flush();
    }

    pub fn pointer_button(&mut self, id: u32, code: u32, press: bool) {
        if let Some(a) = self.begin(id, proto::POINTER_BUTTON) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(crate::now_ms());
            self.arg_u32(code);
            self.arg_u32(if press { proto::BUTTON_PRESSED } else { proto::BUTTON_RELEASED });
            self.end(a);
        }
        self.pointer_frame(id);
        self.flush();
    }

    /// One turn of the wheel, as one group.
    ///
    /// The order is the protocol's, not a preference: `axis_source` describes
    /// the group before it says anything, and `axis_discrete` is defined as
    /// coming *before* the `axis` it belongs to, so that a client reading
    /// events in order knows the click count by the time it has the distance.
    ///
    /// A client below version 5 gets the `axis` alone, which is all version 1
    /// ever had — the wheel still scrolls, it simply has no click count.
    pub fn pointer_axis(&mut self, id: u32, axis: u32, detents: i32) {
        if detents == 0 {
            return;
        }
        let modern = self.objects.version_of(id) >= proto::POINTER_FRAME_SINCE;
        if modern {
            if let Some(a) = self.begin(id, proto::POINTER_AXIS_SOURCE) {
                self.arg_u32(proto::AXIS_SOURCE_WHEEL);
                self.end(a);
            }
            if let Some(a) = self.begin(id, proto::POINTER_AXIS_DISCRETE) {
                self.arg_u32(axis);
                self.arg_u32(detents as u32);
                self.end(a);
            }
        }
        if let Some(a) = self.begin(id, proto::POINTER_AXIS) {
            self.arg_u32(crate::now_ms());
            self.arg_u32(axis);
            self.arg_u32(crate::seat::fixed(detents * proto::AXIS_STEP));
            self.end(a);
        }
        self.pointer_frame(id);
        self.flush();
    }

    fn seat_request(&mut self, object: u32, opcode: u16, args: &mut Args) -> Handled {
        match opcode {
            proto::SEAT_GET_KEYBOARD => {
                let id = take_new_id(&self.rbuf, args)?;
                // An object made by a request inherits the version of the
                // object it was made from — that is how a client that bound
                // wl_seat at 1 gets a wl_keyboard at 1, with five events and
                // not six.
                let version = self.objects.version_of(object);
                if !self.objects.insert_at(id, Kind::Keyboard, version) {
                    return Err(self.no_room(id));
                }
                self.keyboard = id;
                self.send_keymap(id);
                if version >= 4 {
                    if let Some(a) = self.begin(id, proto::KEYBOARD_REPEAT_INFO) {
                        self.arg_u32(crate::seat::REPEAT_RATE as u32);
                        self.arg_u32(crate::seat::REPEAT_DELAY as u32);
                        self.end(a);
                    }
                }
                // A client may take its keyboard while already focused --
                // `wlprobe` does, because it waits for a configure first -- and
                // would otherwise hear nothing until focus moved away and back.
                let focus = crate::seat::focus();
                if let Some(s) = surface::get(focus) {
                    if s.client == self.slot {
                        self.keyboard_enter(id, focus, crate::seat::mods());
                    }
                }
                self.flush();
                Ok(())
            }
            proto::SEAT_GET_POINTER => {
                let id = take_new_id(&self.rbuf, args)?;
                let version = self.objects.version_of(object);
                if !self.objects.insert_at(id, Kind::Pointer, version) {
                    return Err(self.no_room(id));
                }
                self.pointer = id;
                // A pointer already over this client's surface would otherwise
                // hear nothing until it left and came back.
                let focus = crate::seat::pointer_focus();
                if let Some(s) = surface::get(focus) {
                    if s.client == self.slot {
                        self.pointer_enter(id, focus, 0, 0);
                    }
                }
                self.flush();
                Ok(())
            }
            // Touch is not among the advertised capabilities, so asking for one
            // is a client ignoring what it was told. The object is recorded so
            // that destroying it does not look like a reference to nothing; it
            // simply never hears anything.
            proto::SEAT_GET_TOUCH => {
                let id = take_new_id(&self.rbuf, args)?;
                if !self.objects.insert(id, Kind::None) {
                    return Err(self.no_room(id));
                }
                Ok(())
            }
            proto::SEAT_RELEASE => {
                self.objects.remove(object);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `wl_keyboard.keymap`, which carries a descriptor whatever its format.
    ///
    /// The client maps it and hands it to xkbcommon; the compositor never reads
    /// it back. Saying `NO_KEYMAP` instead would leave every client to guess
    /// what the key codes mean, and the guesses do not agree.
    fn send_keymap(&mut self, id: u32) {
        // Alone in the buffer: see `pending_fd`.
        self.flush();
        // Without a keymap the event still has to go out, and still has to
        // carry a descriptor: a client waits for it before it will believe the
        // keyboard exists at all.
        let (format, size, fd) = match crate::keymap::descriptor() {
            Some(fd) if crate::keymap::ready() => {
                (proto::KEYMAP_FORMAT_XKB_V1, crate::keymap::size(), fd)
            }
            other => {
                if let Some(fd) = other {
                    let _ = syscall::sys_fd_close(fd);
                }
                match syscall::sys_memfd_create(1) {
                    Ok(fd) => (proto::KEYMAP_FORMAT_NO_KEYMAP, 0, fd),
                    Err(()) => return,
                }
            }
        };
        if let Some(a) = self.begin(id, proto::KEYBOARD_KEYMAP) {
            self.arg_u32(format);
            self.arg_u32(size);
            self.end(a);
        }
        self.pending_fd = fd;
        self.flush();
    }

    pub fn keyboard_enter(&mut self, id: u32, surface_idx: usize, mods: u32) {
        let Some(surface_id) = self.surface_id(surface_idx) else {
            return;
        };
        if let Some(a) = self.begin(id, proto::KEYBOARD_ENTER) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(surface_id);
            // The keys already held down. Empty: the compositor does not track
            // which are held, and telling a client a key is down when it is not
            // leaves it waiting for a release that never comes.
            self.arg_array(&[]);
            self.end(a);
        }
        if let Some(a) = self.begin(id, proto::KEYBOARD_MODIFIERS) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(mods); // depressed
            self.arg_u32(0); // latched
            self.arg_u32(0); // locked
            self.arg_u32(0); // group
            self.end(a);
        }
        self.flush();
    }

    /// `surface_idx` is passed rather than read from the seat because by the
    /// time a leave is sent the focus has already moved: reading it would name
    /// the surface that just *gained* focus, or — when the two belong to
    /// different clients, which is the only case a leave matters in — name
    /// nothing this client knows and send no leave at all.
    pub fn keyboard_leave(&mut self, id: u32, surface_idx: usize) {
        let Some(surface_id) = self.surface_id(surface_idx) else {
            return;
        };
        if let Some(a) = self.begin(id, proto::KEYBOARD_LEAVE) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(surface_id);
            self.end(a);
        }
        self.flush();
    }

    pub fn keyboard_key(&mut self, id: u32, keycode: u32, press: bool) {
        if let Some(a) = self.begin(id, proto::KEYBOARD_KEY) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(crate::now_ms());
            self.arg_u32(keycode);
            self.arg_u32(if press { proto::KEY_PRESSED } else { proto::KEY_RELEASED });
            self.end(a);
        }
        self.flush();
    }

    pub fn keyboard_modifiers(&mut self, id: u32, mods: u32) {
        if let Some(a) = self.begin(id, proto::KEYBOARD_MODIFIERS) {
            self.arg_u32(proto::next_serial());
            self.arg_u32(mods);
            self.arg_u32(0);
            self.arg_u32(0);
            self.arg_u32(0);
            self.end(a);
        }
        self.flush();
    }

    /// The object id this client knows a surface by.
    fn surface_id(&self, surface_idx: usize) -> Option<u32> {
        self.objects
            .find(|k| matches!(k, Kind::Surface { surface: s } if *s == surface_idx))
    }

    fn compositor_request(&mut self, opcode: u16, args: &mut Args) -> Handled {
        let id = take_new_id(&self.rbuf, args)?;
        match opcode {
            proto::COMPOSITOR_CREATE_SURFACE => {
                let Some(idx) = surface::create(self.slot, self.tid) else {
                    return Err(Fault::memory(id, b"too many surfaces"));
                };
                if !self.objects.insert(id, Kind::Surface { surface: idx }) {
                    surface::destroy(idx);
                    return Err(self.no_room(id));
                }
                Ok(())
            }
            proto::COMPOSITOR_CREATE_REGION => {
                if !self.objects.insert(id, Kind::Region) {
                    return Err(self.no_room(id));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn surface_request(
        &mut self,
        object: u32,
        idx: usize,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        match opcode {
            proto::SURFACE_DESTROY => {
                surface::destroy(idx);
                self.forget_surface(idx);
                Ok(())
            }
            proto::SURFACE_ATTACH => {
                // attach(buffer, x, y). A null buffer is a real request: it
                // says there is nothing to show, which is not the same as
                // saying nothing about the buffer at all.
                let buffer_id = take_object(&self.rbuf, args, true)?;
                let _x = take_i32(&self.rbuf, args)?;
                let _y = take_i32(&self.rbuf, args)?;
                if buffer_id == 0 {
                    surface::attach(idx, surface::NONE);
                    return Ok(());
                }
                let Some(Kind::Buffer { buffer }) = self.objects.get(buffer_id) else {
                    return Err(Fault::object(object, b"attach: not a buffer"));
                };
                surface::attach(idx, buffer);
                Ok(())
            }
            proto::SURFACE_DAMAGE | proto::SURFACE_DAMAGE_BUFFER => {
                // Which pixels changed is not tracked. This compositor repaints
                // a whole window when it commits, so the only thing damage
                // decides here is whether to repaint at all — and the rectangle
                // would have to be believed to be worth more than that.
                for _ in 0..4 {
                    take_i32(&self.rbuf, args)?;
                }
                surface::damage(idx);
                Ok(())
            }
            proto::SURFACE_FRAME => {
                let id = take_new_id(&self.rbuf, args)?;
                if !self.objects.insert(id, Kind::Callback) {
                    return Err(self.no_room(id));
                }
                if !surface::want_frame(idx, id) {
                    return Err(Fault::memory(object, b"too many frame callbacks outstanding"));
                }
                Ok(())
            }
            proto::SURFACE_COMMIT => self.commit(object, idx),
            proto::SURFACE_SET_OPAQUE_REGION | proto::SURFACE_SET_INPUT_REGION => {
                // A region is a hint this compositor does not act on, and a
                // null one is how a client takes the hint back.
                let region = take_object(&self.rbuf, args, true)?;
                if region != 0 && !matches!(self.objects.get(region), Some(Kind::Region)) {
                    return Err(Fault::object(object, b"that is not a region"));
                }
                Ok(())
            }
            // Transforms, scales and offsets are read and checked, and not
            // acted on: each is a transform this compositor does not apply,
            // and refusing the ones that are meant would stop clients that
            // set them by habit.
            proto::SURFACE_SET_BUFFER_TRANSFORM => {
                match take_i32(&self.rbuf, args)? {
                    0..=7 => Ok(()),
                    _ => Err(Fault {
                        object,
                        code: proto::SURFACE_ERR_INVALID_TRANSFORM,
                        message: b"that is not a transform",
                    }),
                }
            }
            proto::SURFACE_SET_BUFFER_SCALE => {
                match take_i32(&self.rbuf, args)? {
                    n if n > 0 => Ok(()),
                    _ => Err(Fault {
                        object,
                        code: proto::SURFACE_ERR_INVALID_SCALE,
                        message: b"a scale of zero or less",
                    }),
                }
            }
            proto::SURFACE_OFFSET => {
                take_i32(&self.rbuf, args)?;
                take_i32(&self.rbuf, args)?;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Apply everything the client has been accumulating, then tell it what
    /// that cost it: the buffer it may draw into again, and the callbacks that
    /// came due.
    fn commit(&mut self, object: u32, idx: usize) -> Handled {
        let Some(s) = surface::get(idx) else {
            return Err(Fault::object(object, b"commit: no such surface"));
        };
        // A buffer attached to a surface that has not agreed to a size is the
        // one thing xdg_shell makes an error rather than a no-op: the client is
        // showing pixels it has not been told the shape of. A surface with no
        // role at all is exempt — it is not a window yet, so there is nothing
        // it could have agreed to.
        if s.role != surface::Role::None
            && !s.configured
            && s.pending.attached
            && s.pending.buffer != surface::NONE
        {
            return Err(Fault {
                object,
                code: proto::XDG_ERR_UNCONFIGURED_BUFFER,
                message: b"buffer attached before ack_configure",
            });
        }
        let Some(applied) = surface::commit(idx) else {
            return Err(Fault::object(object, b"commit: no such surface"));
        };
        if applied.repaint {
            if let Some(w) = surface::window_of(idx) {
                crate::refresh_window(w);
            }
        }
        if applied.release != surface::NONE {
            if let Some(id) = self.id_of_buffer(applied.release) {
                if let Some(a) = self.begin(id, proto::BUFFER_RELEASE) {
                    self.end(a);
                }
            }
        }
        // The frame callbacks come due here and go out on the next pass. See
        // `due` for why the delay is the point rather than an omission.
        for i in 0..applied.nframes {
            if self.ndue < MAX_DUE {
                self.due[self.ndue] = applied.frames[i];
                self.ndue += 1;
            } else {
                // Answer immediately rather than lose it: a frame callback
                // that is never sent is a client that never draws again.
                self.send_frame(applied.frames[i]);
            }
        }
        Ok(())
    }

    fn send_frame(&mut self, id: u32) {
        if let Some(a) = self.begin(id, proto::CALLBACK_DONE) {
            self.arg_u32(crate::now_ms());
            self.end(a);
        }
        self.delete_id(id);
        self.objects.remove(id);
    }

    /// Answer the frame callbacks that came due, at most one batch per tick.
    ///
    /// The tick is the clock this compositor has: there is no vertical blank to
    /// wait for, so "presented" means "drawn, and the compositor has been round
    /// its loop since". A client is thereby held to a hundred frames a second
    /// rather than to however fast it can fill a buffer, and the compositor
    /// keeps the time in between for the keyboard.
    pub fn fire_frames(&mut self, now: u64) {
        if self.ndue == 0 || now == self.fired_at {
            return;
        }
        self.fired_at = now;
        let n = self.ndue;
        self.ndue = 0;
        let due = self.due;
        for i in 0..n {
            self.send_frame(due[i]);
        }
        self.flush();
    }

    /// The object id a client knows a buffer by.
    fn id_of_buffer(&self, buffer: usize) -> Option<u32> {
        self.objects.find(|k| matches!(k, Kind::Buffer { buffer: b } if *b == buffer))
    }

    /// Tell the client an id it allocated is free again.
    fn delete_id(&mut self, id: u32) {
        if let Some(a) = self.begin(proto::DISPLAY_ID, proto::DISPLAY_DELETE_ID) {
            self.arg_u32(id);
            self.end(a);
        }
    }

    fn wm_base_request(&mut self, object: u32, opcode: u16, args: &mut Args) -> Handled {
        match opcode {
            proto::WM_BASE_GET_XDG_SURFACE => {
                let id = take_new_id(&self.rbuf, args)?;
                let surface_id = take_object(&self.rbuf, args, false)?;
                let Some(Kind::Surface { surface: idx }) = self.objects.get(surface_id) else {
                    return Err(Fault::object(object, b"get_xdg_surface: not a surface"));
                };
                if !self.objects.insert(id, Kind::XdgSurface { surface: idx }) {
                    return Err(self.no_room(id));
                }
                surface::set_xdg_surface(idx, id);
                Ok(())
            }
            proto::WM_BASE_DESTROY => {
                self.objects.remove(object);
                Ok(())
            }
            // A pong answers a ping this compositor does not send, and a
            // positioner belongs to popups, which it does not place.
            proto::WM_BASE_PONG => Ok(()),
            proto::WM_BASE_CREATE_POSITIONER => Err(Fault::method(object, b"no popups")),
            _ => Ok(()),
        }
    }

    fn xdg_surface_request(
        &mut self,
        object: u32,
        idx: usize,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        match opcode {
            proto::XDG_SURFACE_GET_TOPLEVEL => {
                let id = take_new_id(&self.rbuf, args)?;
                let Some(top) = shell::make_toplevel(idx) else {
                    return Err(Fault {
                        object,
                        code: proto::XDG_ERR_ROLE,
                        message: b"that surface already has a role",
                    });
                };
                if !self.objects.insert(id, Kind::XdgToplevel { surface: idx }) {
                    return Err(self.no_room(id));
                }
                surface::set_toplevel(idx, id);
                // The size, then the state (none of them), then the configure
                // that says "answer this". A client waits for all three before
                // it draws anything at all.
                if let Some(a) = self.begin(id, proto::TOPLEVEL_CONFIGURE) {
                    self.arg_u32(top.width);
                    self.arg_u32(top.height);
                    self.arg_array(&[]);
                    self.end(a);
                }
                let serial = shell::begin_configure(idx);
                if let Some(a) = self.begin(object, proto::XDG_SURFACE_CONFIGURE) {
                    self.arg_u32(serial);
                    self.end(a);
                }
                self.flush();
                Ok(())
            }
            proto::XDG_SURFACE_ACK_CONFIGURE => {
                let serial = take_u32(&self.rbuf, args)?;
                if !surface::ack(idx, serial) {
                    return Err(Fault::method(object, b"ack_configure: no such serial"));
                }
                Ok(())
            }
            proto::XDG_SURFACE_DESTROY => {
                // As with the toplevel: an id the client has destroyed must
                // not be one a configure is later addressed to.
                surface::set_xdg_surface(idx, 0);
                self.objects.remove(object);
                Ok(())
            }
            // Window geometry says which part of the surface is the window
            // proper, excluding its own shadows. Nothing here draws client-side
            // decorations, so the whole surface is the window.
            proto::XDG_SURFACE_SET_GEOMETRY => Ok(()),
            proto::XDG_SURFACE_GET_POPUP => Err(Fault::method(object, b"no popups")),
            _ => Ok(()),
        }
    }

    fn toplevel_request(
        &mut self,
        object: u32,
        idx: usize,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        match opcode {
            proto::TOPLEVEL_SET_TITLE => {
                let mut title = [0u8; surface::MAX_TITLE];
                let len = {
                    let text = take_str(&self.rbuf, args, false)?;
                    let len = text.len().min(surface::MAX_TITLE);
                    title[..len].copy_from_slice(&text[..len]);
                    len
                };
                surface::set_title(idx, &title[..len]);
                Ok(())
            }
            proto::TOPLEVEL_DESTROY => {
                // The window goes; the surface stays until the client destroys
                // that too. It is the client's `wl_surface` that names it.
                // `clear_role` forgets both shell ids, so nothing configures an
                // object the client has just destroyed.
                surface::clear_role(idx);
                self.objects.remove(object);
                Ok(())
            }
            // move(seat, serial): the same grab a press on the title bar
            // starts, asked for by a client that draws its own decorations.
            //
            // The serial is read and not checked. A compositor is meant to
            // refuse a grab whose serial was not a recent press, which needs a
            // history of serials this one does not keep; refusing on a serial
            // it cannot verify would be pretending to a check rather than
            // making one, and it is written down as a gap instead.
            proto::TOPLEVEL_MOVE => {
                let _seat = take_object(&self.rbuf, args, false)?;
                let _serial = take_u32(&self.rbuf, args)?;
                // Only while the button is actually down. The serial is read
                // and not checked — this compositor keeps no history of
                // serials, and refusing on one it cannot verify would be
                // pretending to a check rather than making one — but a grab
                // with nothing held would end at the next release or never,
                // so a client could take the pointer away from the person
                // using the machine by asking at the wrong moment.
                if !crate::pointer_held() {
                    return Ok(());
                }
                if let Some(s) = surface::get(idx) {
                    if s.window != surface::NONE {
                        let (x, y) = crate::cursor::position();
                        crate::grab::start_move(s.window, x, y);
                    }
                }
                Ok(())
            }
            // resize(seat, serial, edges): the same grab a press on a corner
            // starts. The edges are the client's to choose — it knows where
            // the pointer went down inside its own decorations — and an
            // `edges` of zero is "none", which starts nothing.
            proto::TOPLEVEL_RESIZE => {
                let _seat = take_object(&self.rbuf, args, false)?;
                let _serial = take_u32(&self.rbuf, args)?;
                let edges = take_u32(&self.rbuf, args)?;
                if !crate::pointer_held() {
                    return Ok(());
                }
                if let Some(s) = surface::get(idx) {
                    if s.window != surface::NONE {
                        let (x, y) = crate::cursor::position();
                        crate::grab::start_resize(s.window, edges & 0xF, x, y);
                    }
                }
                Ok(())
            }
            // The client asking for what a double click on its title bar
            // does. Both are a request rather than a statement: the answer is
            // a configure with a size and the state, which the client is free
            // to ignore like any other.
            proto::TOPLEVEL_SET_MAXIMIZED | proto::TOPLEVEL_UNSET_MAXIMIZED => {
                let want = opcode == proto::TOPLEVEL_SET_MAXIMIZED;
                if let Some(s) = surface::get(idx) {
                    if s.window != surface::NONE && crate::window_maximized(s.window) != want {
                        crate::toggle_maximized(s.window);
                    }
                }
                Ok(())
            }
            // Fullscreen and minimise: this compositor has one screen and no
            // place to put a window that is not on it.
            _ => Ok(()),
        }
    }

    /// Tell the client its request cannot be honoured, and which one.
    ///
    /// A protocol error is fatal to the connection by design: the client's idea
    /// of the object graph and the compositor's have diverged, and everything
    /// after this point would be read against the wrong one. Saying so is still
    /// worth the bytes — libwayland prints it, and a client author reads the
    /// object id and the reason rather than guessing why a socket closed.
    fn protocol_error(&mut self, object: u32, code: u32, message: &[u8]) -> bool {
        if let Some(a) = self.begin(proto::DISPLAY_ID, proto::DISPLAY_ERROR) {
            self.arg_u32(object);
            self.arg_u32(code);
            self.arg_str(message);
            self.end(a);
        }
        self.flush();
        false
    }

    fn shm_request(&mut self, object: u32, opcode: u16, args: &mut Args) -> Handled {
        if opcode != proto::SHM_CREATE_POOL {
            self.objects.remove(object);
            return Ok(());
        }
        // create_pool(new_id, fd, size). The descriptor is not in the message:
        // it came alongside it, and is claimed in the order requests ask.
        let id = take_new_id(&self.rbuf, args)?;
        let size = take_i32(&self.rbuf, args)?;
        let Some(fd) = self.take_fd() else {
            return Err(Fault {
                object,
                code: proto::SHM_ERR_INVALID_FD,
                message: b"create_pool without a descriptor",
            });
        };
        if size <= 0 {
            let _ = syscall::sys_fd_close(fd);
            return Err(Fault {
                object,
                code: proto::SHM_ERR_INVALID_STRIDE,
                message: b"pool size",
            });
        }
        if shm::pools_of(self.tid) >= shm::MAX_POOLS_PER_CLIENT {
            // One client's pools are its own share of them: without this,
            // a client asking for pools in a loop takes the memory every
            // other client draws through.
            let _ = syscall::sys_fd_close(fd);
            return Err(Fault::memory(object, b"too many pools"));
        }
        // `create_pool` consumes the descriptor whether or not it works out.
        let Some(pool) = shm::create_pool(self.tid, fd, size as usize) else {
            return Err(Fault::memory(object, b"cannot map that pool"));
        };
        if !self.objects.insert(id, Kind::ShmPool { pool }) {
            shm::destroy_pool(pool);
            return Err(self.no_room(id));
        }
        Ok(())
    }

    fn pool_request(
        &mut self,
        object: u32,
        pool: usize,
        opcode: u16,
        args: &mut Args,
    ) -> Handled {
        match opcode {
            proto::SHM_POOL_CREATE_BUFFER => {
                // create_buffer(new_id, offset, width, height, stride, format)
                let id = take_new_id(&self.rbuf, args)?;
                let offset = take_i32(&self.rbuf, args)?;
                let width = take_i32(&self.rbuf, args)?;
                let height = take_i32(&self.rbuf, args)?;
                let stride = take_i32(&self.rbuf, args)?;
                let format = take_u32(&self.rbuf, args)?;
                if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
                    return Err(Fault {
                        object,
                        code: proto::SHM_ERR_INVALID_STRIDE,
                        message: b"negative buffer geometry",
                    });
                }
                if !shm::format_supported(format) {
                    return Err(Fault {
                        object,
                        code: proto::SHM_ERR_INVALID_FORMAT,
                        message: b"unsupported pixel format",
                    });
                }
                if shm::buffers_of(self.tid) >= shm::MAX_BUFFERS_PER_CLIENT {
                    return Err(Fault::memory(object, b"too many buffers"));
                }
                let made = shm::create_buffer(
                    pool,
                    offset as usize,
                    width as usize,
                    height as usize,
                    stride as usize,
                    format,
                );
                let Some(buffer) = made else {
                    // The arithmetic did not fit inside the pool. This is the
                    // check standing between a client's numbers and the
                    // compositor reading memory that is not there.
                    return Err(Fault {
                        object,
                        code: proto::SHM_ERR_INVALID_STRIDE,
                        message: b"buffer runs past its pool",
                    });
                };
                if !self.objects.insert(id, Kind::Buffer { buffer }) {
                    shm::destroy_buffer(buffer);
                    return Err(self.no_room(id));
                }
                Ok(())
            }
            proto::SHM_POOL_DESTROY => {
                shm::destroy_pool(pool);
                self.objects.remove(object);
                Ok(())
            }
            // A pool may only grow, and growing means new memory, which means
            // a new descriptor -- which resize does not carry. It is refused
            // rather than ignored: a client that resized and then drew past
            // the old end would fault the compositor.
            proto::SHM_POOL_RESIZE => Err(Fault::method(object, b"resize is not supported")),
            _ => Ok(()),
        }
    }

    fn display_request(&mut self, opcode: u16, args: &mut Args) -> Handled {
        let id = take_new_id(&self.rbuf, args)?;
        match opcode {
            proto::DISPLAY_GET_REGISTRY => {
                if !self.objects.insert(id, Kind::Registry) {
                    return Err(self.no_room(id));
                }
                self.send_globals(id);
                Ok(())
            }
            proto::DISPLAY_SYNC => {
                // A sync is a barrier: the callback fires after everything
                // queued before it, which here means after the globals.
                if let Some(a) = self.begin(id, proto::CALLBACK_DONE) {
                    self.arg_u32(0);
                    self.end(a);
                }
                // The callback is gone once it has fired, and the client is
                // told so it may reuse the id.
                if let Some(a) = self.begin(proto::DISPLAY_ID, proto::DISPLAY_DELETE_ID) {
                    self.arg_u32(id);
                    self.end(a);
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn send_globals(&mut self, registry: u32) {
        for (i, g) in proto::GLOBALS.iter().enumerate() {
            let Some(a) = self.begin(registry, proto::REGISTRY_GLOBAL) else {
                return;
            };
            self.arg_u32(i as u32 + 1);
            self.arg_str(g.name);
            self.arg_u32(g.version);
            self.end(a);
        }
    }

    fn registry_request(&mut self, opcode: u16, args: &mut Args) -> Handled {
        if opcode != proto::REGISTRY_BIND {
            return Ok(());
        }
        // bind(name, interface, version, new_id)
        let name = take_u32(&self.rbuf, args)?;
        let global = proto::GLOBALS.get(name.wrapping_sub(1) as usize);
        // The interface named has to be the one advertised under that number.
        // A client that sends another has counted the globals wrongly, and
        // would be answered with events for something it is not.
        let named = {
            let iface = take_str(&self.rbuf, args, false)?;
            global.is_some_and(|g| iface == g.name)
        };
        let version = take_u32(&self.rbuf, args)?;
        let id = take_new_id(&self.rbuf, args)?;
        let Some(global) = global else {
            return Err(Fault::object(args.object, b"bind: no such global"));
        };
        if !named {
            return Err(Fault::object(args.object, b"bind: not that global's interface"));
        }
        // What was advertised is what there is. Honouring a higher number
        // would have the compositor promising events it has no code for, and
        // libwayland indexes a client's listener struct by opcode without a
        // bounds check.
        if version == 0 || version > global.version {
            return Err(Fault::object(args.object, b"bind: a version that was not offered"));
        }
        let kind = match name {
            1 => Kind::Compositor,
            2 => Kind::Shm,
            3 => Kind::Output,
            4 => Kind::XdgWmBase,
            5 => Kind::Seat,
            6 => Kind::Decoration,
            7 => Kind::DataDeviceManager { which: clipboard::CLIPBOARD as u8 },
            _ => Kind::DataDeviceManager { which: clipboard::PRIMARY as u8 },
        };
        if !self.objects.insert_at(id, kind, version) {
            return Err(self.no_room(id));
        }
        if kind == Kind::Seat {
            // What this seat has. A client reads it to decide what to ask for,
            // so a capability advertised is a request that must be answered.
            if let Some(a) = self.begin(id, proto::SEAT_CAPABILITIES) {
                self.arg_u32(proto::SEAT_CAP_KEYBOARD | proto::SEAT_CAP_POINTER);
                self.end(a);
            }
            if version >= 2 {
                if let Some(a) = self.begin(id, proto::SEAT_NAME) {
                    self.arg_str(b"seat0");
                    self.end(a);
                }
            }
        }
        if kind == Kind::Output {
            // The object a surface's `enter` will name. A client may bind more
            // than one; the latest is the one used, because a compositor with
            // one output has nothing to choose between them.
            self.output = id;
            // A client that binds an output waits for `done` before it
            // believes any of it, so all four go out together.
            let (w, h) = crate::screen_size();
            if let Some(a) = self.begin(id, proto::OUTPUT_GEOMETRY) {
                self.arg_u32(0); // x
                self.arg_u32(0); // y
                self.arg_u32(0); // physical width, unknown
                self.arg_u32(0); // physical height, unknown
                self.arg_u32(0); // subpixel: unknown
                self.arg_str(b"Quark");
                self.arg_str(b"framebuffer");
                self.arg_u32(0); // transform: normal
                self.end(a);
            }
            if let Some(a) = self.begin(id, proto::OUTPUT_MODE) {
                self.arg_u32(proto::OUTPUT_MODE_CURRENT | proto::OUTPUT_MODE_PREFERRED);
                self.arg_u32(w as u32);
                self.arg_u32(h as u32);
                self.arg_u32(0); // refresh in mHz: the framebuffer does not say
                self.end(a);
            }
            if version >= 2 {
                if let Some(a) = self.begin(id, proto::OUTPUT_SCALE) {
                    self.arg_u32(1);
                    self.end(a);
                }
                if let Some(a) = self.begin(id, proto::OUTPUT_DONE) {
                    self.end(a);
                }
            }
        }
        if kind == Kind::Shm {
            // wl_shm announces its formats the moment it is bound, and a client
            // that supports none of them is expected to find that out here
            // rather than by having a buffer refused later.
            for f in [shm::FORMAT_ARGB8888, shm::FORMAT_XRGB8888] {
                if let Some(a) = self.begin(id, proto::SHM_FORMAT) {
                    self.arg_u32(f);
                    self.end(a);
                }
            }
        }
        Ok(())
    }
}
