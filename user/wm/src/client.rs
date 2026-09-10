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
use crate::shm;

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

pub struct Client {
    pub used: bool,
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
}

pub const NO_CLIENT: Client = Client {
    used: false,
    fd: 0,
    tid: 0,
    objects: objects::EMPTY,
    rbuf: [0; READ_BUF],
    rlen: 0,
    wbuf: [0; WRITE_BUF],
    wlen: 0,
    fds: [0; MAX_PENDING_FDS],
    nfds: 0,
};

impl Client {
    pub fn open(&mut self, fd: usize, tid: usize) {
        self.used = true;
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
            _ => true, // interfaces that arrive in later tasks
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
