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
//!
//! Telling them takes the right to call them, which nothing else gives this
//! server: a claim is made with a capability to the claimant on offer, and a
//! claim without one is refused. It is kept for as long as the claimant is in
//! line for the display.
//!
//! The line is a stack. Each claim goes on top and displaces the one below,
//! which gets the display back when everything above it has let go — a
//! console under a compositor under another compositor unwinds in that order.
//! A claimant below the top that lets go or dies just leaves the line.

use quark_rt::ipc::{death_notice, Message, TID_ANY};
use quark_rt::{nameserver, println, syscall};

// No capabilities here. The framebuffer's address is whatever mode the
// bootloader set, which nothing knowable at build time can name, so `init`
// mints the range and grants it directly — the one capability a manifest
// cannot express. The band it runs in is expressible, and is asked for like
// anything else: programs wait on this server, so it goes ahead of them and
// behind the drivers.
quark_rt::manifest!([
    quark_rt::manifest::CapReq::priority(quark_rt::syscall::PRIO_SERVER),
]);

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

/// How many programs can be in line for the display. A ninth claim is
/// refused: deeper nesting than this is a program claiming in a loop.
const MAX_CLAIMANTS: usize = 8;

/// A program in line for the display, and the slot holding the capability to
/// call it.
#[derive(Clone, Copy)]
struct Claimant {
    tid: usize,
    slot: usize,
}

/// Oldest first: the last one is drawing.
static mut LINE: [Claimant; MAX_CLAIMANTS] = [Claimant { tid: 0, slot: 0 }; MAX_CLAIMANTS];
static mut DEPTH: usize = 0;

fn line() -> &'static [Claimant] {
    unsafe {
        let line: &'static [Claimant; MAX_CLAIMANTS] = &*core::ptr::addr_of!(LINE);
        &line[..DEPTH]
    }
}

/// Who is drawing, or 0.
fn owner() -> usize {
    line().last().map_or(0, |c| c.tid)
}

fn push(tid: usize, slot: usize) {
    unsafe {
        LINE[DEPTH] = Claimant { tid, slot };
        DEPTH += 1;
    }
}

/// Take `tid` out of line, wherever it is, and let go of the capability to
/// call it. Returns whether it was drawing, or `None` if it was not in line.
fn remove(tid: usize) -> Option<bool> {
    let i = line().iter().position(|c| c.tid == tid)?;
    unsafe {
        let top = i + 1 == DEPTH;
        let _ = syscall::sys_cap_delete(LINE[i].slot);
        LINE.copy_within(i + 1..DEPTH, i);
        DEPTH -= 1;
        Some(top)
    }
}

/// How long a claimant has to answer being told the display is changing
/// hands. One that does not is passed over: a wedged program must not keep
/// the screen from everybody else.
const HANDOVER_TICKS: u64 = 100;

fn call_claimant(tid: usize, msg: &Message) {
    let mut reply = Message::empty();
    let _ = syscall::sys_call_timeout(tid, msg, &mut reply, HANDOVER_TICKS);
}

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

/// Give the display to whoever is on top of the line now, and tell it the
/// mode. One that cannot be given it leaves the line, and the next is tried.
fn hand_back() {
    while owner() != 0 {
        let tid = owner();
        if lease_to(tid) {
            let msg = mode_reply();
            call_claimant(tid, &Message { sender: 0, tag: TAG_FB_GAINED, data: msg.data });
            return;
        }
        let _ = remove(tid);
    }
}

/// `sender` wants the display.
fn claim(sender: usize) -> Message {
    if owner() == sender {
        return mode_reply(); // already theirs
    }
    let again = line().iter().any(|c| c.tid == sender);
    if !again && line().len() == MAX_CLAIMANTS {
        return error();
    }
    // Nothing to tell it with when somebody else claims, without this.
    let Ok(slot) = syscall::sys_cap_take_any(sender) else {
        return error();
    };
    // Claiming again from further down the line is asking to be on top, not
    // to be in line twice.
    if again {
        let _ = remove(sender);
    }
    take_back();
    if !lease_to(sender) {
        let _ = syscall::sys_cap_delete(slot);
        hand_back();
        return error();
    }
    push(sender, slot);
    // A program that dies still holding the display would otherwise keep it
    // for good: revocation stops it mapping the framebuffer again, but
    // nothing gives the screen back to whoever is next in line.
    let _ = syscall::sys_task_watch(sender);
    mode_reply()
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
    let owner = owner();
    if owner != 0 {
        // A call, not a send: the point is to know they have stopped before
        // anybody else starts. If they cannot answer, go ahead anyway — a
        // wedged client must not make the screen unusable for everything else.
        call_claimant(owner, &Message { sender: 0, tag: TAG_FB_LOST, data: [0; 6] });
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

        // A claimant has died. The kernel is not waiting for an answer; the
        // same tag from anybody else is an unknown request.
        if let Some(dead) = death_notice(&msg) {
            if remove(dead) == Some(true) {
                println!("[fb] tid {} died holding the display", dead);
                let _ = syscall::sys_cap_revoke(LEASE_SLOT);
                hand_back();
            }
            continue;
        }

        let reply = match msg.tag {
            TAG_FB_INFO => mode_reply(),

            TAG_FB_CLAIM => claim(sender),

            TAG_FB_RELEASE => match remove(sender) {
                None => error(),
                // Further down the line: whoever is drawing carries on.
                Some(false) => ok(),
                Some(true) => {
                    let _ = syscall::sys_cap_revoke(LEASE_SLOT);
                    // Answer the releaser before telling the next owner: they
                    // are waiting on this reply, and handing the display over
                    // is a call of its own.
                    let _ = syscall::sys_reply(sender, &ok());
                    hand_back();
                    continue; // already replied
                }
            },

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
