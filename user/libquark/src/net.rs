/// Network client helpers — wraps net service IPC protocol for user-space callers.

use crate::ipc::Message;
use crate::syscall;

const TAG_UDP_SEND: u64 = 1;
const TAG_UDP_RECV: u64 = 2;
const TAG_NET_CONFIG: u64 = 3;
const TAG_NET_INFO: u64 = 4;
const TAG_ICMP_PING: u64 = 5;
const TAG_NET_DHCP: u64 = 6;
const TAG_DNS_RESOLVE: u64 = 7;
const TAG_TCP_CONNECT: u64 = 10;
const TAG_TCP_LISTEN: u64 = 11;
const TAG_TCP_SEND: u64 = 13;
const TAG_TCP_RECV: u64 = 14;
const TAG_TCP_CLOSE: u64 = 15;
const TAG_ERROR: u64 = u64::MAX;

/// Send a UDP datagram. `phys_addr` must point to a page with the payload.
/// `dst_ip` is packed big-endian (e.g., 10.0.2.2 = 0x0A000202).
pub fn udp_send(
    net_tid: usize,
    phys_addr: usize,
    len: usize,
    dst_ip: u32,
    dst_port: u16,
    src_port: u16,
) -> Result<(), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_UDP_SEND,
        data: [
            phys_addr as u64,
            len as u64,
            dst_ip as u64,
            ((dst_port as u64) << 16) | (src_port as u64),
            0, 0,
        ],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(()) }
}

/// Receive a UDP datagram. Blocks until data arrives on `listen_port` (0 = any).
/// `phys_addr` must point to a page for the received payload.
/// Returns (bytes_read, src_ip, src_port, dst_port).
pub fn udp_recv(
    net_tid: usize,
    phys_addr: usize,
    max_len: usize,
    listen_port: u16,
) -> Result<(usize, u32, u16, u16), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_UDP_RECV,
        data: [phys_addr as u64, max_len as u64, listen_port as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
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

/// Configure IP address, netmask, and gateway (all packed big-endian u32).
pub fn configure(net_tid: usize, ip: u32, netmask: u32, gateway: u32) -> Result<(), u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_NET_CONFIG,
        data: [ip as u64, netmask as u64, gateway as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(()) }
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

/// Trigger DHCP renewal. Returns the new IP address (packed big-endian) on success.
pub fn dhcp_renew(net_tid: usize) -> Result<[u8; 4], u64> {
    let msg = Message { sender: 0, tag: TAG_NET_DHCP, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { return Err(reply.data[0]); }
    let ip_packed = reply.data[0] as u32;
    Ok(ip_packed.to_be_bytes())
}

// ---------------------------------------------------------------------------
// TCP
// ---------------------------------------------------------------------------

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

/// Send data over a TCP connection. `phys_addr` points to a page with the payload.
/// Returns the number of bytes actually queued.
pub fn tcp_send(
    net_tid: usize,
    handle: usize,
    phys_addr: usize,
    len: usize,
) -> Result<usize, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_SEND,
        data: [handle as u64, phys_addr as u64, len as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
        return Err(1);
    }
    if reply.tag == TAG_ERROR { Err(reply.data[0]) } else { Ok(reply.data[0] as usize) }
}

/// Receive data from a TCP connection. Blocks until data is available.
/// `phys_addr` points to a page for the received data. Returns bytes read (0 = EOF).
pub fn tcp_recv(
    net_tid: usize,
    handle: usize,
    phys_addr: usize,
    max_len: usize,
) -> Result<usize, u64> {
    let msg = Message {
        sender: 0,
        tag: TAG_TCP_RECV,
        data: [handle as u64, phys_addr as u64, max_len as u64, 0, 0, 0],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(net_tid, &msg, &mut reply).is_err() {
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
