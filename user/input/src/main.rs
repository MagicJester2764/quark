#![no_std]
#![no_main]

use libquark::ipc::{Message, TID_ANY};
use libquark::{print, println, syscall};

const NAMESERVER_TID: usize = 2;

// Nameserver protocol
const TAG_NS_REGISTER: u64 = 1;
const TAG_NS_LOOKUP: u64 = 2;

// Keyboard protocol (client side)
const TAG_GET_KEY: u64 = 1;
const TAG_KEY_EVENT: u64 = 2;

// Input server protocol (serving readers)
const TAG_READ: u64 = 1;
const TAG_SET_FOREGROUND: u64 = 2;

// Keyboard registration
const TAG_REGISTER_SIGINT: u64 = 4;

// Notification tag from kernel
const TAG_NOTIFICATION: u64 = 0xFFFF_0002;

const KEY_PRESS: u64 = 1;

const LINE_BUF_SIZE: usize = 256;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[input] Started.");

    // Discover keyboard service
    let kbd_tid = lookup_service(b"keyboard");
    if kbd_tid == 0 {
        println!("[input] Keyboard service not found!");
        syscall::sys_exit();
    }
    println!("[input] Found keyboard at TID {}", kbd_tid);

    // Register as "input" with nameserver
    register_with_nameserver();

    // Register with keyboard driver for Ctrl+C notifications
    register_sigint(kbd_tid);

    println!("[input] Ready.");

    let mut line_buf = [0u8; LINE_BUF_SIZE];
    let mut line_len: usize = 0;
    let mut foreground_tid: usize = 0;

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }

        match msg.tag {
            TAG_SET_FOREGROUND => {
                foreground_tid = msg.data[0] as usize;
                let reply = Message { sender: 0, tag: 0, data: [0; 6] };
                let _ = syscall::sys_reply(msg.sender, &reply);
            }
            TAG_NOTIFICATION => {
                // Ctrl+C notification from keyboard driver (while no one is reading)
                handle_ctrl_c(&mut foreground_tid, &mut line_len);
            }
            TAG_READ => {
                let max_bytes = (msg.data[0] as usize).min(40);
                let reader_tid = msg.sender;

                // Read keys until we have a newline
                loop {
                    let ascii = get_key_blocking(kbd_tid);
                    if ascii == 0 {
                        continue;
                    }

                    match ascii {
                        0x03 => {
                            // Ctrl+C while reading
                            print!("^C\n");
                            line_len = 0;
                            if foreground_tid != 0 {
                                let _ = syscall::sys_signal(foreground_tid, syscall::SIG_INT);
                                foreground_tid = 0;
                            }
                            // Reply with 0 bytes to unblock the reader
                            let reply = pack_read_reply(&line_buf, 0);
                            let _ = syscall::sys_reply(reader_tid, &reply);
                            break;
                        }
                        b'\n' | 13 => {
                            // Newline — echo and deliver
                            print!("\n");
                            if line_len < LINE_BUF_SIZE {
                                line_buf[line_len] = b'\n';
                                line_len += 1;
                            }
                            let deliver_len = line_len.min(max_bytes);
                            let reply = pack_read_reply(&line_buf, deliver_len);
                            let _ = syscall::sys_reply(reader_tid, &reply);
                            line_len = 0;
                            break;
                        }
                        8 | 127 => {
                            // Backspace
                            if line_len > 0 {
                                line_len -= 1;
                                print!("\x08 \x08");
                            }
                        }
                        c if c >= 0x20 => {
                            // Printable character
                            if line_len < LINE_BUF_SIZE - 1 {
                                line_buf[line_len] = c;
                                line_len += 1;
                                let ch = [c];
                                if let Ok(s) = core::str::from_utf8(&ch) {
                                    print!("{}", s);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// Get one key press from the keyboard driver (blocking).
/// Returns ASCII code, or 0 for non-printable/release events.
fn get_key_blocking(kbd_tid: usize) -> u8 {
    let msg = Message {
        sender: 0,
        tag: TAG_GET_KEY,
        data: [0; 6],
    };
    let mut reply = Message::empty();
    if syscall::sys_call(kbd_tid, &msg, &mut reply).is_err() {
        return 0;
    }
    if reply.tag != TAG_KEY_EVENT {
        return 0;
    }
    // Only handle key presses
    if reply.data[0] != KEY_PRESS {
        return 0;
    }
    reply.data[1] as u8
}

/// Pack a read reply: data[0] = byte count, data[1..6] = bytes
fn pack_read_reply(buf: &[u8], len: usize) -> Message {
    let mut data = [0u64; 6];
    data[0] = len as u64;
    for i in 0..5 {
        let base = i * 8;
        let mut w = [0u8; 8];
        for j in 0..8 {
            if base + j < len {
                w[j] = buf[base + j];
            }
        }
        data[i + 1] = u64::from_le_bytes(w);
    }
    Message {
        sender: 0,
        tag: TAG_READ,
        data,
    }
}

fn register_sigint(kbd_tid: usize) {
    let msg = Message {
        sender: 0,
        tag: TAG_REGISTER_SIGINT,
        data: [0; 6],
    };
    let mut reply = Message::empty();
    let _ = syscall::sys_call(kbd_tid, &msg, &mut reply);
}

fn handle_ctrl_c(foreground_tid: &mut usize, line_len: &mut usize) {
    print!("^C\n");
    *line_len = 0;
    if *foreground_tid != 0 {
        let _ = syscall::sys_task_kill(*foreground_tid);
        *foreground_tid = 0;
    }
}

fn lookup_service(name: &[u8]) -> usize {
    let mut buf = [0u8; 24];
    let copy_len = name.len().min(24);
    buf[..copy_len].copy_from_slice(&name[..copy_len]);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_LOOKUP,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() && reply.tag != u64::MAX {
        reply.tag as usize
    } else {
        0
    }
}

fn register_with_nameserver() {
    let name = b"input";
    let mut buf = [0u8; 24];
    buf[..name.len()].copy_from_slice(name);
    let w0 = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let w1 = u64::from_le_bytes(buf[8..16].try_into().unwrap());
    let w2 = u64::from_le_bytes(buf[16..24].try_into().unwrap());

    let msg = Message {
        sender: 0,
        tag: TAG_NS_REGISTER,
        data: [w0, w1, w2, 0, 0, 0],
    };

    let mut reply = Message::empty();
    if syscall::sys_call(NAMESERVER_TID, &msg, &mut reply).is_ok() {
        println!("[input] Registered with nameserver.");
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[input] PANIC: {}", info);
    loop {
        core::hint::spin_loop();
    }
}
