/// Nameserver — service discovery via IPC.
///
/// Tasks register services by name and look them up by name.
/// The nameserver runs as a kernel task (TID assigned at spawn time)
/// and communicates via the IPC subsystem.
///
/// IPC protocol:
///   tag=1 (REGISTER): data[0..3] = name (up to 24 bytes, packed in u64s)
///     → reply tag=0 (OK) or tag=u64::MAX (ERROR)
///   tag=2 (LOOKUP):   data[0..3] = name
///     → reply tag=TID or tag=u64::MAX (NOT_FOUND)

use crate::ipc::{self, Message, TID_ANY};
use crate::scheduler;

const TAG_REGISTER: u64 = 1;
const TAG_LOOKUP: u64 = 2;
const TAG_OK: u64 = 0;
const TAG_NOT_FOUND: u64 = u64::MAX;

const MAX_SERVICES: usize = 32;
const NAME_LEN: usize = 24; // 3 × u64

struct ServiceEntry {
    name: [u8; NAME_LEN],
    name_len: usize,
    tid: usize,
}

static mut NAMESERVER_TID: usize = 0;

/// Get the nameserver's TID (set after init).
pub fn tid() -> usize {
    unsafe { NAMESERVER_TID }
}

/// Spawn the nameserver as a kernel task.
pub fn init() {
    let tid = scheduler::spawn(nameserver_main);
    unsafe { NAMESERVER_TID = tid };
}

fn nameserver_main() {
    let mut services: [Option<ServiceEntry>; MAX_SERVICES] = {
        const NONE: Option<ServiceEntry> = None;
        [NONE; MAX_SERVICES]
    };

    loop {
        // Wait for any message
        let msg = match ipc::sys_recv(TID_ANY) {
            Ok(m) => m,
            Err(_) => continue,
        };

        match msg.tag {
            TAG_REGISTER => {
                let name = extract_name(&msg);
                let sender = msg.sender;

                // Find empty slot
                let mut registered = false;
                for slot in services.iter_mut() {
                    if slot.is_none() {
                        *slot = Some(ServiceEntry {
                            name: name.0,
                            name_len: name.1,
                            tid: sender,
                        });
                        registered = true;
                        break;
                    }
                }

                let reply = Message {
                    sender: 0,
                    tag: if registered { TAG_OK } else { TAG_NOT_FOUND },
                    data: [0; 6],
                };
                let _ = ipc::sys_reply(sender, &reply);
            }
            TAG_LOOKUP => {
                let name = extract_name(&msg);
                let sender = msg.sender;

                let mut found_tid = None;
                for slot in services.iter() {
                    if let Some(entry) = slot {
                        if entry.name_len == name.1
                            && entry.name[..entry.name_len] == name.0[..name.1]
                        {
                            found_tid = Some(entry.tid);
                            break;
                        }
                    }
                }

                let reply = Message {
                    sender: 0,
                    tag: found_tid.map_or(TAG_NOT_FOUND, |t| t as u64),
                    data: [0; 6],
                };
                let _ = ipc::sys_reply(sender, &reply);
            }
            _ => {
                // Unknown tag — reply with error
                let reply = Message {
                    sender: 0,
                    tag: TAG_NOT_FOUND,
                    data: [0; 6],
                };
                let _ = ipc::sys_reply(msg.sender, &reply);
            }
        }
    }
}

/// Extract a service name from message data words.
/// Name is packed into data[0], data[1], data[2] (up to 24 bytes).
fn extract_name(msg: &Message) -> ([u8; NAME_LEN], usize) {
    let mut name = [0u8; NAME_LEN];
    let bytes0 = msg.data[0].to_le_bytes();
    let bytes1 = msg.data[1].to_le_bytes();
    let bytes2 = msg.data[2].to_le_bytes();
    name[0..8].copy_from_slice(&bytes0);
    name[8..16].copy_from_slice(&bytes1);
    name[16..24].copy_from_slice(&bytes2);

    // Find actual length (null-terminated)
    let len = name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
    (name, len)
}

/// Pack a service name into message data words for sending.
pub fn pack_name(name: &[u8]) -> [u64; 3] {
    let mut padded = [0u8; NAME_LEN];
    let copy_len = name.len().min(NAME_LEN);
    padded[..copy_len].copy_from_slice(&name[..copy_len]);

    let w0 = u64::from_le_bytes(padded[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(padded[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(padded[16..24].try_into().unwrap());
    [w0, w1, w2]
}

/// Register a service name for the current task.
pub fn register(name: &[u8]) -> bool {
    let ns_tid = tid();
    if ns_tid == 0 {
        return false;
    }

    let words = pack_name(name);
    let msg = Message {
        sender: 0,
        tag: TAG_REGISTER,
        data: [words[0], words[1], words[2], 0, 0, 0],
    };

    match ipc::sys_call(ns_tid, &msg) {
        Ok(reply) => reply.tag == TAG_OK,
        Err(_) => false,
    }
}

/// Look up a service by name. Returns the TID if found.
pub fn lookup(name: &[u8]) -> Option<usize> {
    let ns_tid = tid();
    if ns_tid == 0 {
        return None;
    }

    let words = pack_name(name);
    let msg = Message {
        sender: 0,
        tag: TAG_LOOKUP,
        data: [words[0], words[1], words[2], 0, 0, 0],
    };

    match ipc::sys_call(ns_tid, &msg) {
        Ok(reply) => {
            if reply.tag == TAG_NOT_FOUND {
                None
            } else {
                Some(reply.tag as usize)
            }
        }
        Err(_) => None,
    }
}
