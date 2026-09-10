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
        // wl_display is object 1 and exists before anything is asked for.
        self.objects.insert(proto::DISPLAY_ID, Kind::Display);
    }

    pub fn close(&mut self) {
        if self.used {
            let _ = syscall::sys_fd_close(self.fd);
            surface::forget_client(self.slot);
            shm::forget_client(self.tid);
        }
        // Descriptors that arrived and were never claimed by a request. The
        // client is gone and nothing will ever ask for them, so they are ours
        // to drop — and not dropping them holds the memory behind them for the
        // life of the compositor.
        for i in 0..self.nfds {
            let _ = syscall::sys_fd_close(self.fds[i]);
        }
        self.nfds = 0;
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
        if self.wlen + wire::HEADER > WRITE_BUF {
            return None;
        }
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
        let size = (self.wlen - at) as u16;
        let lo = self.wbuf[at + 4];
        let hi = self.wbuf[at + 5];
        let opcode = u16::from_le_bytes([lo, hi]);
        let word = ((size as u32) << 16) | opcode as u32;
        self.wbuf[at + 4..at + 8].copy_from_slice(&word.to_le_bytes());
    }

    fn arg_u32(&mut self, v: u32) -> bool {
        if self.wlen + 4 > WRITE_BUF {
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
        let n = match syscall::sys_fd_send_nb(self.fd, &self.wbuf[..self.wlen], None) {
            Err(()) => {
                self.wlen = 0; // the client has gone; the bytes have nowhere to go
                return;
            }
            Ok(None) => 0,
            Ok(Some(n)) => n,
        };
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
            let Some(h) = wire::parse_header(&self.rbuf[at..self.rlen]) else {
                // A malformed header cannot be skipped past, because the size
                // that would say how far is the part that is wrong.
                return false;
            };
            let size = h.size as usize;
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
        let body = at + wire::HEADER;
        let Some(kind) = self.objects.get(h.object) else {
            // An object this client never made. libwayland does not do that,
            // so something is out of step and continuing would compound it.
            return false;
        };
        match kind {
            Kind::Display => self.display_request(h.opcode, body),
            Kind::Registry => self.registry_request(h.opcode, body),
            Kind::Shm => self.shm_request(h.object, h.opcode, body),
            Kind::ShmPool { pool } => self.pool_request(h.object, pool, h.opcode, body),
            Kind::Buffer { buffer } => self.buffer_request(h.object, buffer, h.opcode),
            Kind::Compositor => self.compositor_request(h.opcode, body),
            Kind::Surface { surface } => self.surface_request(h.object, surface, h.opcode, body),
            Kind::Region => {
                if h.opcode == proto::REGION_DESTROY {
                    self.objects.remove(h.object);
                }
                true
            }
            Kind::XdgWmBase => self.wm_base_request(h.object, h.opcode, body),
            Kind::XdgSurface { surface } => {
                self.xdg_surface_request(h.object, surface, h.opcode, body)
            }
            Kind::XdgToplevel { surface } => {
                self.toplevel_request(h.object, surface, h.opcode, body)
            }
            Kind::Output | Kind::Callback | Kind::None => true,
        }
    }

    fn compositor_request(&mut self, opcode: u16, body: usize) -> bool {
        let Some(id) = wire::get_u32(&self.rbuf, body) else {
            return false;
        };
        match opcode {
            proto::COMPOSITOR_CREATE_SURFACE => {
                let Some(idx) = surface::create(self.slot, self.tid) else {
                    return self.protocol_error(id, proto::ERR_NO_MEMORY, b"too many surfaces");
                };
                if !self.objects.insert(id, Kind::Surface { surface: idx }) {
                    surface::destroy(idx);
                    return false;
                }
                true
            }
            proto::COMPOSITOR_CREATE_REGION => self.objects.insert(id, Kind::Region),
            _ => true,
        }
    }

    fn surface_request(&mut self, object: u32, idx: usize, opcode: u16, body: usize) -> bool {
        match opcode {
            proto::SURFACE_DESTROY => {
                surface::destroy(idx);
                self.objects.remove(object);
                true
            }
            proto::SURFACE_ATTACH => {
                // attach(buffer, x, y). A null buffer is a real request: it
                // says there is nothing to show, which is not the same as
                // saying nothing about the buffer at all.
                let Some(buffer_id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                if buffer_id == 0 {
                    surface::attach(idx, surface::NONE);
                    return true;
                }
                let Some(Kind::Buffer { buffer }) = self.objects.get(buffer_id) else {
                    return self.protocol_error(
                        object,
                        proto::ERR_INVALID_OBJECT,
                        b"attach: not a buffer",
                    );
                };
                surface::attach(idx, buffer);
                true
            }
            proto::SURFACE_DAMAGE | proto::SURFACE_DAMAGE_BUFFER => {
                // Which pixels changed is not tracked. This compositor repaints
                // a whole window when it commits, so the only thing damage
                // decides here is whether to repaint at all — and the rectangle
                // would have to be believed to be worth more than that.
                surface::damage(idx);
                true
            }
            proto::SURFACE_FRAME => {
                let Some(id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                if !self.objects.insert(id, Kind::Callback) {
                    return false;
                }
                if !surface::want_frame(idx, id) {
                    return self.protocol_error(
                        object,
                        proto::ERR_NO_MEMORY,
                        b"too many frame callbacks outstanding",
                    );
                }
                true
            }
            proto::SURFACE_COMMIT => self.commit(object, idx),
            // Regions, transforms, scales and offsets: accepted and not acted
            // on. Each is a hint or a transform this compositor does not apply,
            // and refusing them would stop clients that set them by habit.
            proto::SURFACE_SET_OPAQUE_REGION
            | proto::SURFACE_SET_INPUT_REGION
            | proto::SURFACE_SET_BUFFER_TRANSFORM
            | proto::SURFACE_SET_BUFFER_SCALE
            | proto::SURFACE_OFFSET => true,
            _ => true,
        }
    }

    /// Apply everything the client has been accumulating, then tell it what
    /// that cost it: the buffer it may draw into again, and the callbacks that
    /// came due.
    fn commit(&mut self, object: u32, idx: usize) -> bool {
        let Some(s) = surface::get(idx) else {
            return false;
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
            return self.protocol_error(
                object,
                proto::XDG_ERR_UNCONFIGURED_BUFFER,
                b"buffer attached before ack_configure",
            );
        }
        let Some(applied) = surface::commit(idx) else {
            return false;
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
        true
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

    fn wm_base_request(&mut self, object: u32, opcode: u16, body: usize) -> bool {
        match opcode {
            proto::WM_BASE_GET_XDG_SURFACE => {
                let (Some(id), Some(surface_id)) = (
                    wire::get_u32(&self.rbuf, body),
                    wire::get_u32(&self.rbuf, body + 4),
                ) else {
                    return false;
                };
                let Some(Kind::Surface { surface: idx }) = self.objects.get(surface_id) else {
                    return self.protocol_error(
                        object,
                        proto::ERR_INVALID_OBJECT,
                        b"get_xdg_surface: not a surface",
                    );
                };
                self.objects.insert(id, Kind::XdgSurface { surface: idx })
            }
            proto::WM_BASE_DESTROY => {
                self.objects.remove(object);
                true
            }
            // A pong answers a ping this compositor does not send, and a
            // positioner belongs to popups, which it does not place.
            proto::WM_BASE_PONG => true,
            proto::WM_BASE_CREATE_POSITIONER => {
                self.protocol_error(object, proto::ERR_INVALID_METHOD, b"no popups")
            }
            _ => true,
        }
    }

    fn xdg_surface_request(
        &mut self,
        object: u32,
        idx: usize,
        opcode: u16,
        body: usize,
    ) -> bool {
        match opcode {
            proto::XDG_SURFACE_GET_TOPLEVEL => {
                let Some(id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                let Some(top) = shell::make_toplevel(idx) else {
                    return self.protocol_error(
                        object,
                        proto::XDG_ERR_ROLE,
                        b"that surface already has a role",
                    );
                };
                if !self.objects.insert(id, Kind::XdgToplevel { surface: idx }) {
                    return false;
                }
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
                true
            }
            proto::XDG_SURFACE_ACK_CONFIGURE => {
                let Some(serial) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                if !surface::ack(idx, serial) {
                    return self.protocol_error(
                        object,
                        proto::ERR_INVALID_METHOD,
                        b"ack_configure: no such serial",
                    );
                }
                true
            }
            proto::XDG_SURFACE_DESTROY => {
                self.objects.remove(object);
                true
            }
            // Window geometry says which part of the surface is the window
            // proper, excluding its own shadows. Nothing here draws client-side
            // decorations, so the whole surface is the window.
            proto::XDG_SURFACE_SET_GEOMETRY => true,
            proto::XDG_SURFACE_GET_POPUP => {
                self.protocol_error(object, proto::ERR_INVALID_METHOD, b"no popups")
            }
            _ => true,
        }
    }

    fn toplevel_request(&mut self, object: u32, idx: usize, opcode: u16, body: usize) -> bool {
        match opcode {
            proto::TOPLEVEL_SET_TITLE => {
                let Some((title, _)) = wire::get_str(&self.rbuf, body) else {
                    return false;
                };
                surface::set_title(idx, title);
                true
            }
            proto::TOPLEVEL_DESTROY => {
                surface::destroy(idx);
                self.objects.remove(object);
                true
            }
            // Maximise, fullscreen, minimise, move, resize: this compositor
            // decides where windows go and how big they are, and says so by
            // never sending a configure that offers the client a choice.
            _ => true,
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

    fn shm_request(&mut self, object: u32, opcode: u16, body: usize) -> bool {
        if opcode != proto::SHM_CREATE_POOL {
            return true;
        }
        // create_pool(new_id, fd, size). The descriptor is not in the message:
        // it came alongside it, and is claimed in the order requests ask.
        let (Some(id), Some(size)) =
            (wire::get_u32(&self.rbuf, body), wire::get_i32(&self.rbuf, body + 4))
        else {
            return false;
        };
        let Some(fd) = self.take_fd() else {
            return self.protocol_error(
                object,
                proto::SHM_ERR_INVALID_FD,
                b"create_pool without a descriptor",
            );
        };
        if size <= 0 {
            let _ = syscall::sys_fd_close(fd);
            return self.protocol_error(object, proto::SHM_ERR_INVALID_STRIDE, b"pool size");
        }
        // `create_pool` consumes the descriptor whether or not it works out.
        let Some(pool) = shm::create_pool(self.tid, fd, size as usize) else {
            return self.protocol_error(object, proto::ERR_NO_MEMORY, b"cannot map that pool");
        };
        if !self.objects.insert(id, Kind::ShmPool { pool }) {
            shm::destroy_pool(pool);
            return false;
        }
        true
    }

    fn pool_request(&mut self, object: u32, pool: usize, opcode: u16, body: usize) -> bool {
        match opcode {
            proto::SHM_POOL_CREATE_BUFFER => {
                // create_buffer(new_id, offset, width, height, stride, format)
                let mut args = [0i32; 5];
                for (i, a) in args.iter_mut().enumerate() {
                    match wire::get_i32(&self.rbuf, body + 4 + i * 4) {
                        Some(v) => *a = v,
                        None => return false,
                    }
                }
                let Some(id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                let [offset, width, height, stride, format] = args;
                if offset < 0 || width <= 0 || height <= 0 || stride <= 0 {
                    return self.protocol_error(
                        object,
                        proto::SHM_ERR_INVALID_STRIDE,
                        b"negative buffer geometry",
                    );
                }
                if !shm::format_supported(format as u32) {
                    return self.protocol_error(
                        object,
                        proto::SHM_ERR_INVALID_FORMAT,
                        b"unsupported pixel format",
                    );
                }
                let made = shm::create_buffer(
                    pool,
                    offset as usize,
                    width as usize,
                    height as usize,
                    stride as usize,
                    format as u32,
                );
                let Some(buffer) = made else {
                    // The arithmetic did not fit inside the pool. This is the
                    // check standing between a client's numbers and the
                    // compositor reading memory that is not there.
                    return self.protocol_error(
                        object,
                        proto::SHM_ERR_INVALID_STRIDE,
                        b"buffer runs past its pool",
                    );
                };
                if !self.objects.insert(id, Kind::Buffer { buffer }) {
                    shm::destroy_buffer(buffer);
                    return false;
                }
                true
            }
            proto::SHM_POOL_DESTROY => {
                shm::destroy_pool(pool);
                self.objects.remove(object);
                true
            }
            proto::SHM_POOL_RESIZE => {
                // A pool may only grow, and growing means new memory, which
                // means a new descriptor -- which resize does not carry. It is
                // refused rather than ignored: a client that resized and then
                // drew past the old end would fault the compositor.
                self.protocol_error(object, proto::ERR_INVALID_METHOD, b"resize is not supported")
            }
            _ => true,
        }
    }

    fn buffer_request(&mut self, object: u32, buffer: usize, opcode: u16) -> bool {
        if opcode == proto::BUFFER_DESTROY {
            shm::destroy_buffer(buffer);
            self.objects.remove(object);
        }
        true
    }

    fn display_request(&mut self, opcode: u16, body: usize) -> bool {
        match opcode {
            proto::DISPLAY_GET_REGISTRY => {
                let Some(id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
                if !self.objects.insert(id, Kind::Registry) {
                    return false;
                }
                self.send_globals(id);
                true
            }
            proto::DISPLAY_SYNC => {
                let Some(id) = wire::get_u32(&self.rbuf, body) else {
                    return false;
                };
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
                true
            }
            _ => true,
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

    fn registry_request(&mut self, opcode: u16, body: usize) -> bool {
        if opcode != proto::REGISTRY_BIND {
            return true;
        }
        // bind(name, interface, version, new_id)
        let Some(name) = wire::get_u32(&self.rbuf, body) else {
            return false;
        };
        let Some((_iface, used)) = wire::get_str(&self.rbuf, body + 4) else {
            return false;
        };
        let Some(_version) = wire::get_u32(&self.rbuf, body + 4 + used) else {
            return false;
        };
        let Some(id) = wire::get_u32(&self.rbuf, body + 8 + used) else {
            return false;
        };
        let kind = match name {
            1 => Kind::Compositor,
            2 => Kind::Shm,
            3 => Kind::Output,
            4 => Kind::XdgWmBase,
            _ => return false,
        };
        if !self.objects.insert(id, kind) {
            return false;
        }
        if kind == Kind::Output {
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
            if let Some(a) = self.begin(id, proto::OUTPUT_SCALE) {
                self.arg_u32(1);
                self.end(a);
            }
            if let Some(a) = self.begin(id, proto::OUTPUT_DONE) {
                self.end(a);
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
        true
    }
}
