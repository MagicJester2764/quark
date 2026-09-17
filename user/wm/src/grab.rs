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

use crate::protocol as proto;

/// What the pointer is doing.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    None,
    Move,
    Resize,
}

/// A window no smaller than this. A window of nothing has no title bar left to
/// take hold of, and a client asked for a zero-sized buffer would be right to
/// complain.
const MIN_W: usize = 64;
const MIN_H: usize = 48;

static mut KIND: Kind = Kind::None;
static mut WINDOW: usize = 0;
/// Where inside the window the pointer took hold.
///
/// Kept so that the window travels *with* the pointer. Without it the corner
/// of the window jumps to wherever the press was, which is the difference
/// between dragging something and throwing it.
static mut OFF_X: usize = 0;
static mut OFF_Y: usize = 0;

/// Which edges a resize is holding, and what the window was when it started.
///
/// Measured from the start rather than accumulated per motion: a size built up
/// from deltas drifts away from the pointer as soon as one of them is clamped
/// away at the minimum, and the window then stops following the hand.
static mut EDGES: u32 = 0;
static mut START_W: usize = 0;
static mut START_H: usize = 0;
static mut START_PX: usize = 0;
static mut START_PY: usize = 0;
/// The tick a configure last went out on. A mouse reports a hundred times a
/// second and a client redraws at the screen's rate; asking it for a new size
/// per packet is asking for work nobody will see.
static mut LAST_ASK: u64 = u64::MAX;

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

/// Take hold of an edge or a corner.
///
/// `edges` is the protocol's pair of bits, so a corner arrives as one value
/// and needs no special case here.
pub fn start_resize(window: usize, edges: u32, x: usize, y: usize) {
    let Some((w, h)) = crate::window_size(window) else {
        return;
    };
    if edges == 0 {
        return;
    }
    unsafe {
        KIND = Kind::Resize;
        WINDOW = window;
        EDGES = edges;
        START_W = w;
        START_H = h;
        START_PX = x;
        START_PY = y;
        LAST_ASK = u64::MAX;
    }
}

/// Which edges a resize of this window is holding, if one is.
///
/// Asked when the client's new buffer arrives: a window being resized by its
/// left or top edge has to move as it changes size, or the edge the pointer is
/// *not* holding walks across the screen.
pub fn resize_edges(idx: usize) -> Option<u32> {
    unsafe {
        if KIND == Kind::Resize && WINDOW == idx { Some(EDGES) } else { None }
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
            Kind::Resize => {
                let now = quark_rt::syscall::sys_ticks();
                if now == LAST_ASK {
                    return true; // one ask per tick; the rest is the same tick
                }
                LAST_ASK = now;
                let (w, h) = wanted(x, y);
                crate::ask_resize(WINDOW, w, h, true);
                true
            }
        }
    }
}

/// The size the pointer is asking for, clamped.
///
/// The compositor does not resize the window itself. It asks, and the client's
/// next commit is what changes anything — which is why a client that ignores
/// the configure simply keeps the size it had.
fn wanted(x: usize, y: usize) -> (usize, usize) {
    unsafe {
        let dx = x as i64 - START_PX as i64;
        let dy = y as i64 - START_PY as i64;
        let mut w = START_W as i64;
        let mut h = START_H as i64;
        if EDGES & proto::EDGE_RIGHT != 0 {
            w += dx;
        }
        if EDGES & proto::EDGE_LEFT != 0 {
            w -= dx;
        }
        if EDGES & proto::EDGE_BOTTOM != 0 {
            h += dy;
        }
        if EDGES & proto::EDGE_TOP != 0 {
            h -= dy;
        }
        let (sw, sh) = crate::screen_size();
        (
            (w.max(MIN_W as i64) as usize).min(sw),
            (h.max(MIN_H as i64) as usize).min(sh),
        )
    }
}

/// The button came up, or something else ended it.
pub fn release() {
    unsafe {
        if KIND == Kind::Resize {
            // One more configure, without `resizing` in it: the gesture is
            // over, and a client that slowed down for it can stop.
            if let Some((w, h)) = crate::window_size(WINDOW) {
                crate::ask_resize(WINDOW, w, h, false);
            }
        }
        KIND = Kind::None;
    }
}

/// A window went away. A grab that was about it ends with it.
pub fn window_gone(idx: usize) {
    unsafe {
        if KIND != Kind::None && WINDOW == idx {
            KIND = Kind::None;
        }
    }
}
