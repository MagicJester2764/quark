/// Network client helpers — wraps net service IPC protocol for user-space callers.

use crate::ipc::Message;
use crate::syscall;

const TAG_UDP_SEND: u64 = 1;
const TAG_UDP_RECV: u64 = 2;
const TAG_NET_INFO: u64 = 4;
const TAG_ICMP_PING: u64 = 5;
const TAG_DNS_RESOLVE: u64 = 7;
const TAG_TCP_CONNECT: u64 = 10;
const TAG_TCP_LISTEN: u64 = 11;
const TAG_TCP_SEND: u64 = 13;
const TAG_TCP_RECV: u64 = 14;
const TAG_TCP_CLOSE: u64 = 15;
const TAG_ERROR: u64 = u64::MAX;

/// The most a datagram carries.
pub const MAX_DATAGRAM: usize = 1472;
/// The most one [`tcp_send`] or [`tcp_recv`] carries.
pub const MAX_SEGMENT_IO: usize = 4096;

/// Send `payload` (at most [`MAX_DATAGRAM`] bytes) as a UDP datagram, lending
/// it to the server for the call. `dst_ip` is packed big-endian (e.g., 10.0.2.2
/// = 0x0A000202).
pub fn udp_send(
    net_tid: usize,
    payload: &[u8],
    dst_ip: u32,
    dst_port: u16,
    src_port: u16,
) -> Result<(), u64> {
    let payload = &payload[..payload.len().min(MAX_DATAGRAM)];
    let msg = Message {
        sender: 0,
        tag: TAG_UDP_SEND,
        data: [
            0,
            payload.len() as u64,
            dst_ip as u64,
            ((dst_port as u64) << 16) | (src_port as u64),
            0, 0,
        ],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(net_tid, &msg, &mut reply, payload).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(()) }
}

/// Receive a UDP datagram into `buf`, which the server is lent until one
/// arrives on `listen_port` (0 = any). Blocks until then.
/// Returns (bytes_read, src_ip, src_port, dst_port).
pub fn udp_recv(
    net_tid: usize,
    buf: &mut [u8],
    listen_port: u16,
) -> Result<(usize, u32, u16, u16), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_UDP_RECV,
        data: [0, buf.len() as u64, listen_port as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(net_tid, &msg, &mut reply, buf).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    let bytes = reply.data[0] as usize;
    let src_ip = reply.data[1] as u32;
    let ports = reply.data[2];
    let src_port = (ports >> 16) as u16;
    let dst_port = (ports & 0xFFFF) as u16;
    Ok((bytes, src_ip, src_port, dst_port))
}

/// Get network info. Returns (mac_packed_le, ip_packed_be).
pub fn info(net_tid: usize) -> Result<(u64, u32), u64> {
    let msg = Message { sender: 0, tag: TAG_NET_INFO, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { return Err(reply.data[0]); }
    Ok((reply.data[0], reply.data[1] as u32))
}

/// Send an ICMP echo request and wait for the reply.
/// `dst_ip` is packed big-endian. Returns (rtt_ticks, ttl, reply_bytes) on success.
pub fn icmp_ping(net_tid: usize, dst_ip: u32, id: u16, seq: u16) -> Result<(u64, u8, usize), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_ICMP_PING,
        data: [dst_ip as u64, id as u64, seq as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    let rtt = reply.data[0];
    let ttl = reply.data[1] as u8;
    let size = reply.data[2] as usize;
    Ok((rtt, ttl, size))
}

/// Resolve a hostname to an IPv4 address via DNS.
/// Returns the IP as a packed big-endian u32 on success.
/// Hostname must be <= 48 bytes.
pub fn dns_resolve(net_tid: usize, hostname: &[u8]) -> Result<u32, u64> {
    if hostname.is_empty() || hostname.len() > 48 {
        return Err(1);
    }
    let mut name_buf = [0u8; 48];
    name_buf[..hostname.len()].copy_from_slice(hostname);
    let msg = Message {
        sender: 0,
        tag: TAG_DNS_RESOLVE,
        data: [
            u64::from_le_bytes(name_buf[0..8].try_into().unwrap()),
            u64::from_le_bytes(name_buf[8..16].try_into().unwrap()),
            u64::from_le_bytes(name_buf[16..24].try_into().unwrap()),
            u64::from_le_bytes(name_buf[24..32].try_into().unwrap()),
            u64::from_le_bytes(name_buf[32..40].try_into().unwrap()),
            u64::from_le_bytes(name_buf[40..48].try_into().unwrap()),
        ],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { return Err(reply.data[0]); }
    Ok(reply.data[0] as u32)
}

/// Open a TCP connection to `dst_ip:dst_port`. Blocks until established or timeout.
/// `src_port` of 0 uses an ephemeral port. Returns connection handle on success.
pub fn tcp_connect(
    net_tid: usize,
    dst_ip: u32,
    dst_port: u16,
    src_port: u16,
) -> Result<usize, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_CONNECT,
        data: [
            dst_ip as u64,
            ((dst_port as u64) << 16) | (src_port as u64),
            0, 0, 0, 0,
        ],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(reply.data[0] as usize) }
}

/// Listen for an incoming TCP connection on `port`. Blocks until a client connects.
/// Returns (handle, remote_ip, remote_port).
pub fn tcp_listen(
    net_tid: usize,
    port: u16,
) -> Result<(usize, u32, u16), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_LISTEN,
        data: [port as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR {
        return Err(reply.data[0]);
    }
    Ok((reply.data[0] as usize, reply.data[1] as u32, reply.data[2] as u16))
}

/// Send up to [`MAX_SEGMENT_IO`] bytes of `data` over a TCP connection,
/// lending them to the server for the call. Returns how many were queued.
pub fn tcp_send(net_tid: usize, handle: usize, data: &[u8]) -> Result<usize, u64> {
    let data = &data[..data.len().min(MAX_SEGMENT_IO)];
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_SEND,
        data: [handle as u64, 0, data.len() as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend(net_tid, &msg, &mut reply, data).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(reply.data[0] as usize) }
}

/// Receive into `buf` (at most [`MAX_SEGMENT_IO`] bytes of it) from a TCP
/// connection, lending it to the server until data arrives. Blocks until then.
/// Returns bytes read (0 = the other end closed).
pub fn tcp_recv(net_tid: usize, handle: usize, buf: &mut [u8]) -> Result<usize, u64> {
    let len = buf.len().min(MAX_SEGMENT_IO);
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_RECV,
        data: [handle as u64, 0, len as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call_lend_mut(net_tid, &msg, &mut reply, &mut buf[..len]).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(reply.data[0] as usize) }
}

/// Close a TCP connection gracefully.
pub fn tcp_close(net_tid: usize, handle: usize) -> Result<(), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_CLOSE,
        data: [handle as u64, 0, 0, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(()) }
}
