//! TCP sockets as file descriptors.
//!
//! The net server has had working TCP for a while, but it was reached
//! procedurally — `tcp_connect(net_tid, …)` handing back a handle, then
//! `tcp_send(net_tid, handle, phys_addr, len)` with a physical page the caller
//! allocated and the server mapped. Every program that wanted a socket had to
//! know the server's task ID, allocate pages, and drive that protocol.
//!
//! Ported software expects none of that. It expects to connect, get back
//! something it can `read` and `write`, and pass that thing to code that has
//! no idea a network is involved. So a connection is bound to a descriptor in
//! the task's fd table, and the kernel routes reads and writes on it to the
//! net server the way it already routes them to a console or a pipe.
//!
//! What it costs: the fd path carries 40 bytes per message, so bulk transfer
//! is a round trip per 40 bytes rather than per page. That is the price of
//! going through the same path as everything else, and it is a data-path
//! change to fix, not an interface one — the older page-based calls in
//! [`crate::net`] remain for a caller that needs the throughput.

use crate::nameserver;
use crate::net;
use crate::syscall;

/// Anything that went wrong. Deliberately coarse: the net server reports a
/// small integer, and inventing a taxonomy here would be inventing detail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No net server is registered.
    NoService,
    /// The name could not be resolved.
    NoHost,
    /// The connection attempt failed or was refused.
    ConnectFailed,
    /// The fd table is full.
    NoDescriptor,
    /// The connection is gone.
    Closed,
    /// The transfer failed.
    Io,
}

/// How long to wait for the net server to register itself.
const SERVICE_ATTEMPTS: usize = 50;

fn net_tid() -> Result<usize, Error> {
    nameserver::lookup_retry(b"net", SERVICE_ATTEMPTS).ok_or(Error::NoService)
}

/// A connected TCP stream.
///
/// Closes the connection and releases the descriptor when dropped, so the
/// ordinary path needs no explicit close.
pub struct TcpStream {
    fd: usize,
    net_tid: usize,
    handle: usize,
}

impl TcpStream {
    /// Connect to `ip:port`.
    pub fn connect(ip: [u8; 4], port: u16) -> Result<TcpStream, Error> {
        let net_tid = net_tid()?;
        Self::connect_via(net_tid, u32::from_be_bytes(ip), port)
    }

    /// Connect to `host:port`, resolving `host` through the net server's DNS.
    ///
    /// A dotted quad is parsed directly rather than being sent to the
    /// resolver, so an address works with no DNS server configured.
    pub fn connect_host(host: &[u8], port: u16) -> Result<TcpStream, Error> {
        let net_tid = net_tid()?;
        let ip = match parse_ipv4(host) {
            Some(ip) => u32::from_be_bytes(ip),
            None => net::dns_resolve(net_tid, host).map_err(|_| Error::NoHost)?,
        };
        Self::connect_via(net_tid, ip, port)
    }

    fn connect_via(net_tid: usize, ip: u32, port: u16) -> Result<TcpStream, Error> {
        let handle = net::tcp_connect(net_tid, ip, port, 0).map_err(|_| Error::ConnectFailed)?;
        match syscall::sys_sock_fd(net_tid, handle) {
            Ok(fd) => Ok(TcpStream { fd, net_tid, handle }),
            Err(()) => {
                // The connection is open but unreachable; do not leak it.
                let _ = net::tcp_close(net_tid, handle);
                Err(Error::NoDescriptor)
            }
        }
    }

    /// Read into `buf`, returning 0 at end of stream.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Error> {
        let n = syscall::sys_fd_read(self.fd, buf);
        if n == u64::MAX { Err(Error::Io) } else { Ok(n as usize) }
    }

    /// Write `buf`, blocking until all of it is queued.
    pub fn write(&self, buf: &[u8]) -> Result<usize, Error> {
        let n = syscall::sys_fd_write(self.fd, buf);
        if n == u64::MAX { Err(Error::Io) } else { Ok(n as usize) }
    }

    /// Keep writing until everything is sent.
    pub fn write_all(&self, buf: &[u8]) -> Result<(), Error> {
        let mut sent = 0;
        while sent < buf.len() {
            match self.write(&buf[sent..])? {
                0 => return Err(Error::Closed),
                n => sent += n,
            }
        }
        Ok(())
    }

    /// The underlying descriptor, for code that takes an fd.
    pub fn as_fd(&self) -> usize {
        self.fd
    }

    /// Give up ownership of the descriptor without closing the connection.
    pub fn into_fd(self) -> usize {
        let fd = self.fd;
        core::mem::forget(self);
        fd
    }

    /// Close the connection now rather than on drop.
    ///
    /// Idempotent: the server stops recognising the handle as ours after the
    /// first close, so the one that runs on drop does nothing.
    pub fn close(&self) {
        let _ = net::tcp_close(self.net_tid, self.handle);
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let _ = net::tcp_close(self.net_tid, self.handle);
    }
}

/// A socket listening for connections on one port.
pub struct TcpListener {
    net_tid: usize,
    port: u16,
}

impl TcpListener {
    /// Listen on `port`.
    ///
    /// The net server allocates a connection for the listener when a client
    /// arrives, so nothing is held here until then.
    pub fn bind(port: u16) -> Result<TcpListener, Error> {
        Ok(TcpListener { net_tid: net_tid()?, port })
    }

    /// Wait for a client, returning the stream and its address.
    pub fn accept(&self) -> Result<(TcpStream, [u8; 4], u16), Error> {
        let (handle, ip, remote_port) =
            net::tcp_listen(self.net_tid, self.port).map_err(|_| Error::ConnectFailed)?;
        match syscall::sys_sock_fd(self.net_tid, handle) {
            Ok(fd) => Ok((
                TcpStream { fd, net_tid: self.net_tid, handle },
                ip.to_be_bytes(),
                remote_port,
            )),
            Err(()) => {
                let _ = net::tcp_close(self.net_tid, handle);
                Err(Error::NoDescriptor)
            }
        }
    }

    /// The port being listened on.
    pub fn port(&self) -> u16 {
        self.port
    }
}

/// Resolve `host` to an address: a dotted quad directly, anything else
/// through the net server's resolver.
pub fn resolve(host: &[u8]) -> Result<[u8; 4], Error> {
    if let Some(ip) = parse_ipv4(host) {
        return Ok(ip);
    }
    let net_tid = net_tid()?;
    net::dns_resolve(net_tid, host)
        .map(|ip| ip.to_be_bytes())
        .map_err(|_| Error::NoHost)
}

/// Parse a dotted quad. Returns None for anything else, including a hostname.
pub fn parse_ipv4(s: &[u8]) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0;
    let mut value: u32 = 0;
    let mut digits = 0;

    for &b in s {
        match b {
            b'0'..=b'9' => {
                value = value * 10 + (b - b'0') as u32;
                if value > 255 {
                    return None;
                }
                digits += 1;
            }
            b'.' => {
                if digits == 0 || idx >= 3 {
                    return None;
                }
                octets[idx] = value as u8;
                idx += 1;
                value = 0;
                digits = 0;
            }
            _ => return None,
        }
    }

    if idx != 3 || digits == 0 {
        return None;
    }
    octets[3] = value as u8;
    Some(octets)
}
