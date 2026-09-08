#![no_std]
#![no_main]
#![allow(static_mut_refs)]

//! The framebuffer, as a device.
//!
//! This owns the screen the way `/dev/fb0` does: it knows the mode the
//! bootloader set, and it decides who may draw. It does not draw anything
//! itself, and it has no opinion about windows — a text console and a
//! compositor are both just programs that ask it for the display.
//!
//! Handing the display over is a capability transfer, not a copy of a pointer.
//! The `PhysRange` covering the framebuffer arrives from `init` and stays put;
//! each claimant is given a *derived* capability minted into [`LEASE_SLOT`],
//! and taking the display back is revoking that slot. A revoked capability
//! cannot be used to map the framebuffer again, which is what makes "the
//! display belongs to one program at a time" true rather than merely agreed.
//!
//! Pages already mapped stay mapped, though — revocation governs the right to
//! map, not existing mappings. So the outgoing owner is told, and told before
//! the new one is let in:
//!
//! ```text
//!     wm ---- CLAIM -----> fb
//!                          fb ---- LOST ----> console   "stop drawing"
//!                          fb <--- ack -----
//!                          revoke, mint, grant
//!     wm <--- mode -------
//! ```

use quark_rt::ipc::{Message, TAG_TASK_DIED, TID_ANY};
use quark_rt::{nameserver, println, syscall};

// No manifest. The framebuffer's address is whatever mode the bootloader set,
// which nothing knowable at build time can name, so `init` mints the range and
// grants it directly — the one capability a manifest cannot express.

/// From `init`: the mode, alongside the capability in [`MASTER_SLOT`].
const TAG_FB_INIT: u64 = 100;

/// What is on the other end of this? Anyone may ask.
///
/// Replies `data[0] = (width << 32) | height`,
/// `data[1] = (pitch << 32) | bpp`,
/// `data[2] = (red << 16) | (green << 8) | blue` bit positions,
/// `data[3] = physical address`.
const TAG_FB_INFO: u64 = 1;
/// Take the display. Replies as [`TAG_FB_INFO`] does, and the caller may now
/// map the framebuffer.
const TAG_FB_CLAIM: u64 = 2;
/// Give it back to whoever had it before.
const TAG_FB_RELEASE: u64 = 3;
/// Sent *to* the current owner when somebody else claims: stop drawing and
/// unmap. Answer when you have.
///
/// These two are numbered well clear of anything a client is likely to define
/// for its own protocol: they arrive at whatever receive loop that client
/// already has, and a collision would be silent.
const TAG_FB_LOST: u64 = 0x100;
/// Sent to the previous owner when the display comes back, with the mode.
const TAG_FB_GAINED: u64 = 0x101;

const TAG_OK: u64 = 0;
const TAG_ERROR: u64 = u64::MAX;

/// The capability handed to whoever holds the display. Revoking this slot is
/// how the display is taken back.
const LEASE_SLOT: usize = 1;
/// Where a claimant receives it.
///
/// Any free slot would do — `sys_map_phys` scans the whole capability space —
/// but a fixed one keeps the client simple. A claimant must empty it again
/// when it gives the display up: granting into an occupied slot fails, so a
/// client that keeps its revoked lease can never be handed the display twice.
const CLIENT_SLOT: usize = 2;

struct Mode {
    phys: u64,
    width: u64,
    height: u64,
    pitch: u64,
    bpp: u64,
    r_pos: u64,
    g_pos: u64,
    b_pos: u64,
}

static mut MODE: Mode = Mode {
    phys: 0,
    width: 0,
    height: 0,
    pitch: 0,
    bpp: 32,
    r_pos: 16,
    g_pos: 8,
    b_pos: 0,
};

/// Who is drawing, and who gets it back when they let go. One deep: a console
/// that the compositor displaces and returns to is the case that exists, and a
/// stack would be inventing a policy nothing has asked for.
static mut OWNER: usize = 0;
static mut PREVIOUS: usize = 0;

fn mode_reply() -> Message {
    let m = unsafe { &MODE };
    Message {
        sender: 0,
        tag: TAG_OK,
        data: [
            (m.width << 32) | m.height,
            (m.pitch << 32) | m.bpp,
            (m.r_pos << 16) | (m.g_pos << 8) | m.b_pos,
            m.phys,
            0,
            0,
        ],
    }
}

fn error() -> Message {
    Message { sender: 0, tag: TAG_ERROR, data: [0; 6] }
}

fn ok() -> Message {
    Message { sender: 0, tag: TAG_OK, data: [0; 6] }
}

/// Give the display to `back`, which had it before, and tell it the mode.
fn hand_back(back: usize) {
    if back == 0 || !lease_to(back) {
        return;
    }
    unsafe { OWNER = back };
    let _ = syscall::sys_task_watch(back);
    println!("[fb] display returned to tid {}", back);
    let msg = mode_reply();
    let handover = Message { sender: 0, tag: TAG_FB_GAINED, data: msg.data };
    let mut ack = Message::empty();
    let _ = syscall::sys_call(back, &handover, &mut ack);
}

/// Hand the right to map the framebuffer to `tid`.
///
/// Revoke first, always: the previous lease is derived from this slot, so
/// bumping its generation is what stops the last holder mapping it again.
fn lease_to(tid: usize) -> bool {
    let m = unsafe { &MODE };
    let end = m.phys + m.pitch * m.height;

    let _ = syscall::sys_cap_revoke(LEASE_SLOT);
    let _ = syscall::sys_cap_delete(LEASE_SLOT);
    if syscall::sys_cap_mint(LEASE_SLOT, syscall::CAP_TYPE_PHYS_RANGE, m.phys, end).is_err() {
        println!("[fb] could not mint a lease");
        return false;
    }
    if syscall::sys_cap_grant(tid, LEASE_SLOT, CLIENT_SLOT).is_err() {
        println!("[fb] could not grant the display to tid {}", tid);
        return false;
    }
    true
}

/// Take the display away from whoever has it, telling them first.
fn take_back() {
    let owner = unsafe { OWNER };
    if owner != 0 {
        // A call, not a send: the point is to know they have stopped before
        // anybody else starts. If they cannot answer, go ahead anyway — a
        // wedged client must not make the screen unusable for everything else.
        let msg = Message { sender: 0, tag: TAG_FB_LOST, data: [0; 6] };
        let mut reply = Message::empty();
        let _ = syscall::sys_call(owner, &msg, &mut reply);
    }
    let _ = syscall::sys_cap_revoke(LEASE_SLOT);
}

#[unsafe(no_mangle)]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    println!("[fb] Started.");

    // The mode, from init. Nothing can be answered before it arrives.
    let mut msg = Message::empty();
    loop {
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }
        if msg.tag == TAG_FB_INIT {
            break;
        }
        let _ = syscall::sys_reply(msg.sender, &error());
    }
    unsafe {
        MODE = Mode {
            phys: msg.data[0],
            width: msg.data[1] >> 32,
            height: msg.data[1] & 0xFFFF_FFFF,
            pitch: msg.data[2] >> 32,
            bpp: msg.data[2] & 0xFF,
            r_pos: (msg.data[3] >> 16) & 0xFF,
            g_pos: (msg.data[3] >> 8) & 0xFF,
            b_pos: msg.data[3] & 0xFF,
        };
    }
    let _ = syscall::sys_reply(msg.sender, &ok());

    unsafe {
        println!("[fb] {}x{} at {} bpp.", MODE.width, MODE.height, MODE.bpp);
    }
    if nameserver::register(b"fb").is_ok() {
        println!("[fb] Registered with nameserver.");
    }

    loop {
        let mut msg = Message::empty();
        if syscall::sys_recv(TID_ANY, &mut msg).is_err() {
            continue;
        }
        let sender = msg.sender;

        let reply = match msg.tag {
            TAG_FB_INFO => mode_reply(),

            TAG_FB_CLAIM => {
                let owner = unsafe { OWNER };
                if owner == sender {
                    mode_reply() // already theirs
                } else {
                    if owner != 0 {
                        take_back();
                        unsafe { PREVIOUS = owner };
                    }
                    if lease_to(sender) {
                        unsafe { OWNER = sender };
                        // A program that dies still holding the display would
                        // otherwise keep it for good: revocation stops it
                        // mapping the framebuffer again, but nothing gives the
                        // screen back, and there is no console to return to.
                        let _ = syscall::sys_task_watch(sender);
                        println!("[fb] display claimed by tid {}", sender);
                        mode_reply()
                    } else {
                        unsafe { OWNER = 0 };
                        error()
                    }
                }
            }

            TAG_FB_RELEASE => {
                if unsafe { OWNER } != sender {
                    error()
                } else {
                    let _ = syscall::sys_cap_revoke(LEASE_SLOT);
                    let back = unsafe { PREVIOUS };
                    unsafe {
                        OWNER = 0;
                        PREVIOUS = 0;
                    }
                    // Answer the releaser before telling the next owner: they
                    // are waiting on this reply, and handing the display over
                    // is a call of its own.
                    let _ = syscall::sys_reply(sender, &ok());

                    hand_back(back);
                    continue; // already replied
                }
            }

            // Whoever had the display has died. Nobody is waiting on an
            // answer to this, so take it back and give it to whoever was
            // displaced — the text console, on the path that matters.
            TAG_TASK_DIED => {
                let dead = msg.data[0] as usize;
                unsafe {
                    if PREVIOUS == dead {
                        PREVIOUS = 0;
                    }
                    if OWNER != dead {
                        continue;
                    }
                }
                println!("[fb] tid {} died holding the display", dead);
                let _ = syscall::sys_cap_revoke(LEASE_SLOT);
                let back = unsafe { PREVIOUS };
                unsafe {
                    OWNER = 0;
                    PREVIOUS = 0;
                }
                hand_back(back);
                continue; // the kernel is not waiting for a reply
            }

            _ => error(),
        };

        let _ = syscall::sys_reply(sender, &reply);
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("[fb] PANIC: {}", info);
    syscall::sys_exit_code(255);
}
