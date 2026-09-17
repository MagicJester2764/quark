#![no_std]
#![no_main]

//! Names for services, and the right to call them.
//!
//! A service registers by calling here with a capability to itself on offer —
//! `nameserver::register` does that — and this server takes it and grants a
//! copy to every program that looks the name up. For most programs that is the
//! only way to reach a service at all: nothing else hands them the capability.
//! A registration without one is refused, and so is one for a name a live task
//! already holds. Registrants are watched, and their names go when they do.

use quark_rt::ipc::{death_notice, Message, TID_ANY};
use quark_rt::{println, syscall};

// A server: programs are usually blocked waiting on this, so it runs
// ahead of them and behind the drivers. No capabilities — everything
// this needs it is given directly or asks another server for.
quark_rt::manifest!([
    quark_rt::manifest::CapReq::priority(quark_rt::syscall::PRIO_SERVER),
]);

const TAG_REGISTER: u64 = 1;
const TAG_LOOKUP: u64 = 2;
/// Reverse lookup: given a TID in data[0], reply with that service's name.
/// Diagnostics work from TIDs — `ps` output, a kernel log line — and had no way
/// to turn one back into the name it registered under.
const TAG_LOOKUP_TID: u64 = 3;
const TAG_OK: u64 = 0;
const TAG_NOT_FOUND: u64 = u64::MAX;

const MAX_SERVICES: usize = 32;
/// Names one task may hold, so that one task cannot fill the table.
const MAX_NAMES_PER_TASK: usize = 4;
const NAME_LEN: usize = 24; // 3 x u64
/// Where a capability is minted only to be compared, and deleted at once.
const CHECK_SLOT: usize = syscall::SLOT_SCRATCH;

#[derive(Clone, Copy)]
struct ServiceEntry {
    name: [u8; NAME_LEN],
    name_len: usize,
    tid: usize,
    /// The capability to call it, which a lookup copies. 0 for this server's
    /// own entry: a program asking here can call it already.
    slot: usize,
}

type Services = [Option<ServiceEntry>; MAX_SERVICES];

fn find(services: &Services, name: &[u8; NAME_LEN], len: usize) -> Option<usize> {
    services
        .iter()
        .position(|s| s.is_some_and(|e| e.name_len == len && e.name[..len] == name[..len]))
}

/// Does the capability in `slot` name `tid`, as `tid` is now?
///
/// Minting a capability by TID succeeds only for a task this server already
/// holds one to, and records that task's current number. So comparing the two
/// answers both "is what was offered the registrant's own?" and "is that task
/// still the one it was?" — a TID can have been given to somebody else since.
fn names(slot: usize, tid: usize) -> bool {
    let me = syscall::sys_getpid() as usize;
    let current = match syscall::sys_cap_mint(CHECK_SLOT, syscall::CAP_TYPE_ENDPOINT, tid as u64, 0)
    {
        Ok(()) => syscall::sys_cap_read(me, CHECK_SLOT).ok(),
        Err(()) => None,
    };
    let _ = syscall::sys_cap_delete(CHECK_SLOT);
    match (current, syscall::sys_cap_read(me, slot)) {
        (Some(now), Ok(held)) => {
            held.cap_type == syscall::CAP_TYPE_ENDPOINT && held.valid && held.param0 == now.param0
        }
        _ => false,
    }
}

/// Let go of `slot` unless an entry still uses it.
fn release(services: &Services, slot: usize) {
    if slot != 0 && !services.iter().any(|s| s.is_some_and(|e| e.slot == slot)) {
        let _ = syscall::sys_cap_delete(slot);
    }
}

/// Record `sender` under `name`, with the capability its call offered.
fn register(services: &mut Services, sender: usize, name: [u8; NAME_LEN], len: usize) -> bool {
    // The capability has to be the caller's own. Anything else would be handed
    // out under a name that is not its.
    let Ok(slot) = syscall::sys_cap_take_any(sender) else {
        return false;
    };
    if !names(slot, sender) {
        release(services, slot);
        return false;
    }
    if let Some(i) = find(services, &name, len) {
        let Some(held) = services[i] else {
            return false;
        };
        if held.slot == slot {
            return true; // the holder, asking again
        }
        // A live holder keeps its name, and nobody takes this server's own.
        if held.slot == 0 || names(held.slot, held.tid) {
            release(services, slot);
            return false;
        }
        // Its holder is gone and the news has not arrived yet.
        services[i] = None;
        release(services, held.slot);
    }
    let held = services.iter().flatten().filter(|e| e.tid == sender).count();
    let free = services.iter().position(|s| s.is_none());
    let Some(free) = free.filter(|_| held < MAX_NAMES_PER_TASK) else {
        release(services, slot);
        return false;
    };
    services[free] = Some(ServiceEntry { name, name_len: len, tid: sender, slot });
    let _ = syscall::sys_task_watch(sender);
    true
}

/// `dead` has exited: its names go, and the capability with them.
fn forget(services: &mut Services, dead: usize) {
    let mut slots = [0usize; MAX_SERVICES];
    for (i, s) in services.iter_mut().enumerate() {
        if let Some(e) = s {
            if e.tid == dead && e.slot != 0 {
                slots[i] = e.slot;
                *s = None;
            }
        }
    }
    for slot in slots {
        release(services, slot);
    }
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[nameserver] Started.");

    let mut services: Services = [None; MAX_SERVICES];

    // Register ourselves, so a reverse lookup of the nameserver's own TID
    // resolves like any other service. Nothing looks the nameserver up by
    // name — its TID is well known — but diagnostics that go the other way
    // would otherwise show a bare number for it alone.
    {
        let mut name = [0u8; NAME_LEN];
        name[..b"nameserver".len()].copy_from_slice(b"nameserver");
        services[0] = Some(ServiceEntry {
            name,
            name_len: b"nameserver".len(),
            tid: syscall::sys_getpid() as usize,
            slot: 0,
        });
    }

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        // From the kernel, which is not waiting for an answer. The same tag
        // from anybody else falls through to the unknown request it is.
        if let Some(dead) = death_notice(&msg) {
            forget(&mut services, dead);
            continue;
        }

        match msg.tag {
            TAG_REGISTER => {
                let (name, len) = extract_name(&msg);
                let sender = msg.sender;
                let registered = register(&mut services, sender, name, len);

                let reply = Message {
                    sender: 0,
                    tag: if registered { TAG_OK } else { TAG_NOT_FOUND },
                    data: [0; 6],
                };
                let _ = syscall::sys_reply(sender, &reply);
            }
            TAG_LOOKUP => {
                let (name, len) = extract_name(&msg);
                let sender = msg.sender;

                // A TID is only useful with the right to call it, so the
                // answer is the TID and a copy of the capability, or nothing.
                let tag = match find(&services, &name, len).and_then(|i| services[i]) {
                    Some(entry) if entry.slot == 0 => entry.tid as u64,
                    Some(entry) => match syscall::sys_cap_grant_any(sender, entry.slot) {
                        Ok(_) => entry.tid as u64,
                        Err(()) => TAG_NOT_FOUND,
                    },
                    None => TAG_NOT_FOUND,
                };
                let reply = Message { sender: 0, tag, data: [0; 6] };
                let _ = syscall::sys_reply(sender, &reply);
            }
            TAG_LOOKUP_TID => {
                let sender = msg.sender;
                let want = msg.data[0] as usize;

                let mut reply = Message {
                    sender: 0,
                    tag: TAG_NOT_FOUND,
                    data: [0; 6],
                };
                for slot in services.iter() {
                    if let Some(entry) = slot {
                        if entry.tid == want {
                            let mut buf = [0u8; NAME_LEN];
                            buf[..entry.name_len].copy_from_slice(&entry.name[..entry.name_len]);
                            reply.tag = TAG_OK;
                            reply.data[0] = u64::from_le_bytes(buf[0..8].try_into().unwrap());
                            reply.data[1] = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                            reply.data[2] = u64::from_le_bytes(buf[16..24].try_into().unwrap());
                            reply.data[3] = entry.name_len as u64;
                            break;
                        }
                    }
                }
                let _ = syscall::sys_reply(sender, &reply);
            }
            quark_rt::ipc::TAG_PING => {
                // Liveness probe: reply immediately, do nothing else.
                let reply = Message {
                    sender: 0,
                    tag: quark_rt::ipc::TAG_PING,
                    data: [0; 6],
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            _ => {
                let reply = Message {
                    sender: 0,
                    tag: TAG_NOT_FOUND,
                    data: [0; 6],
                };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
        }
    }
}

fn extract_name(msg: &Message) -> ([u8; NAME_LEN], usize) {
    let mut name = [0u8; NAME_LEN];
    let bytes0 = msg.data[0].to_le_bytes();
    let bytes1 = msg.data[1].to_le_bytes();
    let bytes2 = msg.data[2].to_le_bytes();
    name[0..8].copy_from_slice(&bytes0);
    name[8..16].copy_from_slice(&bytes1);
    name[16..24].copy_from_slice(&bytes2);

    let len = name.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
    (name, len)
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[nameserver] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
