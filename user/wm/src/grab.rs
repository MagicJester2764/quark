//! What the pointer is doing between a press on the compositor's own furniture
//! and the release that ends it.
//!
//! A compositor's pointer has two jobs, and they are not the same job. Most of
//! the time it belongs to a client: whatever is under it hears where it is and
//! what it did. But a press on a title bar is a press on the compositor's own
//! chrome, and from there until the button comes up the pointer belongs to the
//! compositor — no client hears a motion, because the movement is not about
//! them. That is the whole of a grab: which mode the pointer is in, which
//! window it is about, and where inside that window it took hold.
//!
//! A grab ends three ways: the button comes up, the window goes away, or the
//! client that owned the window disconnects. The last two are the same thing
//! from here — a grab on a window that no longer exists would move a window
//! slot somebody else has since been given.

/// What the pointer is doing.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    None,
    Move,
}

static mut KIND: Kind = Kind::None;
static mut WINDOW: usize = 0;
/// Where inside the window the pointer took hold.
///
/// Kept so that the window travels *with* the pointer. Without it the corner
/// of the window jumps to wherever the press was, which is the difference
/// between dragging something and throwing it.
static mut OFF_X: usize = 0;
static mut OFF_Y: usize = 0;

/// Is the pointer the compositor's right now?
pub fn active() -> bool {
    unsafe { KIND != Kind::None }
}

/// Take hold of a window at a screen position.
pub fn start_move(window: usize, x: usize, y: usize) {
    let Some(win) = crate::window_origin(window) else {
        return;
    };
    unsafe {
        KIND = Kind::Move;
        WINDOW = window;
        OFF_X = x.saturating_sub(win.0);
        OFF_Y = y.saturating_sub(win.1);
    }
}

/// The pointer moved. Returns whether the grab consumed the movement.
pub fn motion(x: usize, y: usize) -> bool {
    unsafe {
        match KIND {
            Kind::None => false,
            Kind::Move => {
                crate::move_window(WINDOW, x.saturating_sub(OFF_X), y.saturating_sub(OFF_Y));
                true
            }
        }
    }
}

/// The button came up, or something else ended it.
pub fn release() {
    unsafe { KIND = Kind::None };
}

/// A window went away. A grab that was about it ends with it.
pub fn window_gone(idx: usize) {
    unsafe {
        if KIND != Kind::None && WINDOW == idx {
            KIND = Kind::None;
        }
    }
}
