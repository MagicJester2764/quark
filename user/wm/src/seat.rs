//! The seat: one keyboard, and later one pointer.
//!
//! Wayland keeps keyboard focus and pointer focus apart, and this is the first
//! of the two. What the compositor calls focus is a *window*; what the protocol
//! needs is a surface and the client that owns it, so the hop between them
//! happens here rather than in the routing that already works.
//!
//! Nothing here decides focus. `main.rs` still does that — Tab cycles, a new
//! window takes it — and this is told afterwards. Keeping the decision and the
//! announcement apart is what lets the compositor's own bindings work on a
//! window whose client has no seat bound at all.

use crate::client::{Client, MAX_CLIENTS};
use crate::surface;

/// The driver's modifier byte, from `user/keyboard`. Repeated rather than
/// shared because the two do not otherwise know about each other, and this is
/// the boundary where a wire format meets a driver's private encoding.
const MOD_SHIFT: u8 = 1 << 0;
const MOD_CTRL: u8 = 1 << 1;
const MOD_ALT: u8 = 1 << 2;
const MOD_CAPSLOCK: u8 = 1 << 3;

/// What a client is told about key repeat.
///
/// Described rather than done: the protocol has the compositor state the rate
/// and the client generate the repeats, because only the client knows whether
/// the key it is holding means "another character" or "scroll faster".
pub const REPEAT_RATE: i32 = 25;
pub const REPEAT_DELAY: i32 = 400;

/// The surface with keyboard focus, or `surface::NONE`.
static mut FOCUS: usize = surface::NONE;
/// The modifier state as the driver last reported it.
static mut MODS: u8 = 0;

/// The surface the pointer is over, or `surface::NONE`.
///
/// Kept apart from `FOCUS` because Wayland keeps them apart: typing goes to the
/// window you last chose, and clicking goes to the window under the pointer,
/// and a compositor that conflates them is one where moving the mouse steals
/// what you were typing into.
static mut POINTER_FOCUS: usize = surface::NONE;
/// Which buttons are held, in the driver's bit order.
static mut BUTTONS: u8 = 0;

/// A coordinate as the protocol carries it: 24.8 fixed point.
///
/// Wayland's `fixed` is not a convenience — a surface can be scaled, and a
/// pointer position that could only be a whole pixel would be unable to say
/// where it is inside one.
pub fn fixed(v: i32) -> u32 {
    ((v as i64) << 8) as u32
}

/// The driver's modifier byte as an XKB modifier mask.
///
/// The indices are XKB's conventional ones — Shift 0, Lock 1, Control 2, Mod1
/// 3 for Alt — which is what every keymap derived from the standard set uses
/// and therefore what a client will read them back as. With `NO_KEYMAP` a
/// client has nothing to resolve them against and will mostly ignore them;
/// sending the right numbers anyway is what makes a real keymap a change of
/// one constant later.
fn xkb_mods(driver: u8) -> u32 {
    let mut m = 0u32;
    if driver & MOD_SHIFT != 0 {
        m |= 1 << 0;
    }
    if driver & MOD_CAPSLOCK != 0 {
        m |= 1 << 1;
    }
    if driver & MOD_CTRL != 0 {
        m |= 1 << 2;
    }
    if driver & MOD_ALT != 0 {
        m |= 1 << 3;
    }
    m
}

/// The surface that currently has keyboard focus.
pub fn focus() -> usize {
    unsafe { FOCUS }
}

/// The client slot with keyboard focus, if any.
pub fn focused_client() -> Option<usize> {
    surface::get(unsafe { FOCUS }).map(|s| s.client)
}

/// Is this client the focused one? Asked by a client setting something up that
/// only the focused client hears about.
pub fn focus_is_mine(slot: usize) -> bool {
    focused_client() == Some(slot)
}

/// The modifiers held right now, as an XKB mask.
pub fn mods() -> u32 {
    xkb_mods(unsafe { MODS })
}

/// The compositor moved focus to `window`, which may be `usize::MAX` for none.
///
/// Leave goes out before enter, and to a different client than the one that
/// gets the enter — which is the whole reason this is one function rather than
/// two calls at the two sites that change focus.
pub fn focus_changed(clients: &mut [Client; MAX_CLIENTS], window: usize) {
    let next = surface::by_window(window).unwrap_or(surface::NONE);
    let prev = unsafe { FOCUS };
    if prev == next {
        return;
    }
    unsafe { FOCUS = next };

    if prev != surface::NONE {
        if let Some((slot, id)) = locate(clients, prev) {
            clients[slot].keyboard_leave(id, prev);
        }
    }
    if next != surface::NONE {
        if let Some((slot, id)) = locate(clients, next) {
            let mods = unsafe { MODS };
            clients[slot].keyboard_enter(id, next, xkb_mods(mods));
        }
    }
}

/// A key happened. `scancode` is a PS/2 set-1 code with the break bit already
/// stripped, which for everything in the non-extended set *is* the Linux evdev
/// keycode `wl_keyboard.key` wants — escape is 1 in both, F12 is 88 in both.
/// The absence of a translation table here is deliberate, not an omission.
pub fn key(clients: &mut [Client; MAX_CLIENTS], press: bool, scancode: u8, modifiers: u8) {
    let changed = unsafe { MODS != modifiers };
    unsafe { MODS = modifiers };

    let focus = unsafe { FOCUS };
    if focus == surface::NONE {
        return;
    }
    let Some((slot, id)) = locate(clients, focus) else {
        return;
    };
    if changed {
        clients[slot].keyboard_modifiers(id, xkb_mods(modifiers));
    }
    if scancode != 0 {
        clients[slot].keyboard_key(id, scancode as u32, press);
    }
}

/// A surface is going away, so nothing has focus until something takes it.
pub fn surface_gone(idx: usize) {
    unsafe {
        if FOCUS == idx {
            FOCUS = surface::NONE;
        }
        if POINTER_FOCUS == idx {
            POINTER_FOCUS = surface::NONE;
        }
    }
}

/// The pointer moved to a screen position, and may be over a different window
/// than it was.
///
/// `surface_x`/`surface_y` are relative to the surface, which is what the
/// protocol carries: a client knows where its own pixels are and nothing about
/// where the compositor put them.
pub fn motion(
    clients: &mut [Client; MAX_CLIENTS],
    window: usize,
    surface_x: i32,
    surface_y: i32,
) {
    let next = surface::by_window(window).unwrap_or(surface::NONE);
    let prev = unsafe { POINTER_FOCUS };

    if prev != next {
        unsafe { POINTER_FOCUS = next };
        if prev != surface::NONE {
            if let Some((slot, id)) = locate_pointer(clients, prev) {
                clients[slot].pointer_leave(id, prev);
            }
        }
        if next != surface::NONE {
            if let Some((slot, id)) = locate_pointer(clients, next) {
                clients[slot].pointer_enter(id, next, surface_x, surface_y);
            }
        }
        return; // the enter carries the position; a motion after it says nothing new
    }
    if next == surface::NONE {
        return;
    }
    if let Some((slot, id)) = locate_pointer(clients, next) {
        clients[slot].pointer_motion(id, surface_x, surface_y);
    }
}

/// A button changed. `buttons` is the driver's bit order: left, right, middle.
pub fn button(clients: &mut [Client; MAX_CLIENTS], buttons: u8) {
    let was = unsafe { BUTTONS };
    unsafe { BUTTONS = buttons };
    let changed = was ^ buttons;
    if changed == 0 {
        return;
    }
    let focus = unsafe { POINTER_FOCUS };
    if focus == surface::NONE {
        return;
    }
    let Some((slot, id)) = locate_pointer(clients, focus) else {
        return;
    };
    for (bit, code) in [
        (0u8, crate::protocol::BTN_LEFT),
        (1, crate::protocol::BTN_RIGHT),
        (2, crate::protocol::BTN_MIDDLE),
    ] {
        let mask = 1u8 << bit;
        if changed & mask != 0 {
            clients[slot].pointer_button(id, code, buttons & mask != 0);
        }
    }
}

/// The wheel turned, by `detents` clicks, positive towards the user.
///
/// It goes to the surface the pointer is over and not to the focused one: a
/// wheel is part of the pointer, and scrolling the window you are pointing at
/// is what every other system does. A client that has not taken a pointer
/// hears nothing, which is the ordinary case for one that only draws.
pub fn axis(clients: &mut [Client; MAX_CLIENTS], detents: i32) {
    if detents == 0 {
        return;
    }
    let focus = unsafe { POINTER_FOCUS };
    if focus == surface::NONE {
        return;
    }
    let Some((slot, id)) = locate_pointer(clients, focus) else {
        return;
    };
    clients[slot].pointer_axis(id, crate::protocol::AXIS_VERTICAL_SCROLL, detents);
}

/// The surface the pointer is over.
pub fn pointer_focus() -> usize {
    unsafe { POINTER_FOCUS }
}

fn locate_pointer(clients: &[Client; MAX_CLIENTS], surface_idx: usize) -> Option<(usize, u32)> {
    let s = surface::get(surface_idx)?;
    let slot = s.client;
    if slot >= MAX_CLIENTS || !clients[slot].used {
        return None;
    }
    let id = clients[slot].pointer_id()?;
    Some((slot, id))
}

/// The client holding a surface, and the keyboard object it asked for.
///
/// `None` when the client never bound a seat or never took a keyboard, which
/// is the ordinary case for a client that only draws — `weston-simple-shm` is
/// one — and not a failure.
fn locate(clients: &[Client; MAX_CLIENTS], surface_idx: usize) -> Option<(usize, u32)> {
    let s = surface::get(surface_idx)?;
    let slot = s.client;
    if slot >= MAX_CLIENTS || !clients[slot].used {
        return None;
    }
    let id = clients[slot].keyboard_id()?;
    Some((slot, id))
}
