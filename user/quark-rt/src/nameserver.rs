//! Finding a service by name.
//!
//! Task IDs are assigned in start order, so nothing may hardcode one and
//! expect it to keep meaning the same service. The nameserver is the one
//! exception: it is started first, deliberately, so that there is a fixed
//! place to ask about everything else.
//!
//! This was written out separately in eleven programs, each with its own copy
//! of the same name packing and the same retry loop, which is how `ping` ended
//! up calling the nameserver's TID as though it were the service it wanted.

use crate::ipc::Message;
use crate::syscall;

/// The nameserver, started first by `init` so that this number is stable.
pub const NAMESERVER_TID: usize = 2;

const TAG_NS_REGISTER: u64 = 1;
const TAG_NS_LOOKUP: u64 = 2;

/// Longest service name, fixed by the three data words it travels in.
pub const MAX_NAME: usize = 24;

fn pack_name(name: &[u8]) -> [u64; 6] {
    let mut buf = [0u8; MAX_NAME];
    let len = name.len().min(MAX_NAME);
    buf[..len].copy_from_slice(&name[..len]);
    [
        u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]),
        u64::from_le_bytes([buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15]]),
        u64::from_le_bytes([buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23]]),
        0,
        0,
        0,
    ]
}

/// The TID registered under `name`, or `None` if nothing has registered it.
pub fn lookup(name: &[u8]) -> Option<usize> {
    let msg = Message { sender: 0, tag: TAG_NS_LOOKUP, data: pack_name(name) };
    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        Some(reply.tag as usize)
    } else {
        None
    }
}

/// As [`lookup`], retrying while the service starts up.
///
/// Services come up concurrently, so a client started at the same moment as
/// the service it needs will often ask before the answer exists.
pub fn lookup_retry(name: &[u8], attempts: usize) -> Option<usize> {
    for _ in 0..attempts {
        if let Some(tid) = lookup(name) {
            return Some(tid);
        }
        for _ in 0..100 {
            syscall::sys_yield();
        }
    }
    None
}

/// Register the calling task under `name`.
pub fn register(name: &[u8]) -> Result<(), ()> {
    let msg = Message { sender: 0, tag: TAG_NS_REGISTER, data: pack_name(name) };
    let mut reply = Message::empty();
    match syscall::sys_call(NAMESERVER_TID, &msg, &mut reply) {
        Ok(()) if reply.tag != u64::MAX => Ok(()),
        _ => Err(()),
    }
}
