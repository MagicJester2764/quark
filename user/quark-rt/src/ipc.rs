/// IPC message type — mirrors the kernel's Message struct.

pub const TID_ANY: usize = usize::MAX;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Message {
    pub sender: usize,
    pub tag: u64,
    pub data: [u64; 6],
}

impl Message {
    pub const fn empty() -> Self {
        Message {
            sender: 0,
            tag: 0,
            data: [0; 6],
        }
    }
}

/// Conventional no-op request that every IPC server answers.
///
/// A ping has to measure a round trip to the service itself, but there is no
/// tag common to the protocols, and sending a real one would make the service
/// do work. This value sits far from the small integers the protocols use, so
/// any server can answer it without colliding with its own tags. The reply
/// carries no payload — the round trip is the whole point.
///
/// Note that not every service is an IPC server: the console is driven by a
/// pipe on fd 0 and has no dispatch to answer from, so it never replies.
pub const TAG_PING: u64 = 0xFFFF_FFFF_FFFF_FF01;
