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

pub const MAX_CLIENTS: usize = 4;
const READ_BUF: usize = 4096;
const WRITE_BUF: usize = 4096;

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
};

impl Client {
    pub fn open(&mut self, fd: usize, tid: usize) {
        self.used = true;
        self.fd = fd;
        self.tid = tid;
        self.objects.clear();
        self.rlen = 0;
        self.wlen = 0;
        // wl_display is object 1 and exists before anything is asked for.
        self.objects.insert(proto::DISPLAY_ID, Kind::Display);
    }

    pub fn close(&mut self) {
        if self.used {
            let _ = syscall::sys_fd_close(self.fd);
        }
        self.used = false;
        self.fd = 0;
        self.tid = 0;
        self.objects.clear();
        self.rlen = 0;
        self.wlen = 0;
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
            match syscall::sys_fd_recv_nb(self.fd, &mut scratch[..want], None) {
                Err(()) => return false,
                Ok(Some((0, _))) => return false, // end of file: the client has gone
                Ok(Some((got, _))) => {
                    self.rbuf[self.rlen..self.rlen + got].copy_from_slice(&scratch[..got]);
                    self.rlen += got;
                }
                Ok(None) => {} // nothing new; act on what is already buffered
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
            _ => true, // interfaces that arrive in later tasks
        }
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
        self.objects.insert(id, kind)
    }
}
