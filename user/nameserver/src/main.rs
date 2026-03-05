#![no_std]
#![no_main]

use libquark::ipc::{Message, TID_ANY};
use libquark::{println, syscall};

const TAG_REGISTER: u64 = 1;
const TAG_LOOKUP: u64 = 2;
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
