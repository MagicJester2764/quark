#![no_std]
#![no_main]

//! The i8042's other device, on its own.
//!
//! This exists because a mouse and a keyboard share one controller and one data
//! port, and the way that goes wrong is a keyboard that types rubbish or stops.
//! Coupling "does the demultiplex work" to "does `wl_pointer` work" would make
//! one failure look like the other, so this asks the driver directly and prints
//! what it gets: no compositor, no protocol, nothing between.
//!
//! Type while moving. The point is not that the pointer moves — it is that the
//! keys still arrive while it does.

use quark_rt::ipc::Message;
use quark_rt::manifest::CapReq;
use quark_rt::{nameserver, println, syscall};

// Nothing to ask for: the endpoint that reaches the input server is inherited
// from the shell, the same way every other program here gets one.
quark_rt::manifest!([CapReq::priority(quark_rt::syscall::PRIO_NORMAL)]);

const TAG_INPUT_CLAIM: u64 = 0x200;
const TAG_INPUT_RELEASE: u64 = 0x201;
const TAG_INPUT_POLL: u64 = 0x202;
const TAG_INPUT_KEY: u64 = 0x203;
const TAG_INPUT_POLL_MOUSE: u64 = 0x205;
const TAG_INPUT_MOUSE: u64 = 0x206;

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    let Some(input) = nameserver::lookup_retry(b"input", 20) else {
        println!("mousetest: no input server");
        syscall::sys_exit_code(1);
    };
    // Raw input, the way a compositor takes it. Without the claim the poll is
    // refused, which is the same rule that stops a background program reading
    // keys out from under the shell.
    let claim = Message { sender: 0, tag: TAG_INPUT_CLAIM, data: [0; 6] };
    let mut reply = Message::empty();
    if syscall::sys_call(input, &claim, &mut reply).is_err() {
        println!("mousetest: could not claim the input");
        syscall::sys_exit_code(1);
    }

    println!("mousetest: move the mouse and type; ten seconds.");
    let mut x = 0i64;
    let mut y = 0i64;
    let mut moves = 0u32;
    let mut clicks = 0u32;
    let mut keys = 0u32;
    let mut wheel = 0i64;
    let mut last_buttons = 0u64;

    let deadline = syscall::sys_ticks() + 1000;
    while syscall::sys_ticks() < deadline {
        // Keys first, and counted: the whole point of testing the controller
        // on its own is that the mouse must not cost the keyboard.
        let ask_key = Message { sender: 0, tag: TAG_INPUT_POLL, data: [0; 6] };
        let mut key = Message::empty();
        if syscall::sys_call(input, &ask_key, &mut key).is_ok() && key.tag == TAG_INPUT_KEY {
            if key.data[0] != 0 {
                keys += 1;
            }
        }

        let msg = Message { sender: 0, tag: TAG_INPUT_POLL_MOUSE, data: [0; 6] };
        let mut reply = Message::empty();
        if syscall::sys_call(input, &msg, &mut reply).is_err() {
            println!("mousetest: the input server stopped answering");
            break;
        }
        if reply.tag != TAG_INPUT_MOUSE {
            syscall::sleep_ticks(1);
            continue;
        }
        x += reply.data[0] as i64;
        y += reply.data[1] as i64;
        // Detents since the last poll, signed: a mouse without a wheel — or a
        // driver that never asked for one — reports nothing here at all.
        let detents = reply.data[3] as i64;
        if detents != 0 {
            wheel += detents;
            println!("  wheel {}", detents);
        }
        moves += 1;
        if reply.data[2] != last_buttons {
            last_buttons = reply.data[2];
            if last_buttons != 0 {
                clicks += 1;
            }
            println!("  buttons {}", last_buttons);
        }
        // Every so often rather than every packet: a mouse reports a hundred
        // times a second and the console cannot keep up with that.
        if moves % 40 == 0 {
            println!("  {} moves, at {} {}", moves, x, y);
        }
    }
    println!(
        "mousetest: {} moves, {} presses, {} keys, wheel: {}",
        moves, clicks, keys, wheel
    );
    let release = Message { sender: 0, tag: TAG_INPUT_RELEASE, data: [0; 6] };
    let mut done = Message::empty();
    let _ = syscall::sys_call(input, &release, &mut done);
    syscall::sys_exit_code(0);
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("mousetest: PANIC: {}", info);
    syscall::sys_exit_code(255);
}
