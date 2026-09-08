#![no_std]
#![no_main]

use quark_rt::ipc::{Message, TID_ANY};
use quark_rt::{println, syscall};

const TAG_REGISTER: u64 = 1;
const TAG_LOOKUP: u64 = 2;
/// Reverse lookup: given a TID in data[0], reply with that service's name.
/// Diagnostics work from TIDs — `ps` output, a kernel log line — and had no way
/// to turn one back into the name it registered under.
const TAG_LOOKUP_TID: u64 = 3;
const TAG_OK: u64 = 0;
const TAG_NOT_FOUND: u64 = u64::MAX;

const MAX_SERVICES: usize = 32;
const NAME_LEN: usize = 24; // 3 x u64

struct ServiceEntry {
    name: [u8; NAME_LEN],
    name_len: usize,
    tid: usize,
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[nameserver] Started.");

    let mut services: [Option<ServiceEntry>; MAX_SERVICES] = {
        const NONE: Option<ServiceEntry> = None;
        [NONE; MAX_SERVICES]
    };

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
        });
    }

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        match msg.tag {
            TAG_REGISTER => {
                let name = extract_name(&msg);
                let sender = msg.sender;

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
                let _ = syscall::sys_reply(sender, &reply);
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
