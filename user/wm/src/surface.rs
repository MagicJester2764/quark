//! A surface: what a client wants on the screen, and what is on it.
//!
//! The two are deliberately separate. `attach`, `damage` and `frame` change
//! nothing a viewer could see — they accumulate into *pending* state, and
//! `commit` copies all of it to *current* in one go. That copy is the whole of
//! Wayland's atomicity: a half-drawn frame is not merely unlikely here, it is
//! unrepresentable, because there is no moment at which half of a commit has
//! been applied.
//!
//! A surface with no role is not a window and cannot be shown. The role comes
//! from the shell — `xdg_surface.get_toplevel` — and arrives after the surface
//! does, which is why a surface exists in this table before it has a place on
//! the screen.

use crate::shm;

pub const MAX_SURFACES: usize = 16;
/// A title, capped at what a title bar can show.
pub const MAX_TITLE: usize = 32;
/// Frame callbacks one surface may have outstanding.
///
/// A well-behaved client asks for one and waits for it; the cap is what stops
/// a badly-behaved one from making this table grow on its say-so.
const MAX_FRAME: usize = 8;
pub const NONE: usize = usize::MAX;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Not yet a window. A surface may be given a role once, and never a
    /// second — the two roles have different rules and a surface obeying both
    /// obeys neither.
    None,
    Toplevel,
}

/// What a client has asked for, or what it has got. Both halves of a surface
/// are the same shape on purpose: committing is `current = pending`, and a
/// field that existed in one and not the other would be a field that commit
/// forgot.
#[derive(Clone, Copy)]
pub struct State {
    /// The buffer to show, or `NONE` for "nothing" — which is a real request:
    /// attaching a null buffer unmaps a surface.
    pub buffer: usize,
    /// The client said something about the buffer, even if it said "none".
    /// Without this, "attach nothing" and "do not mention the buffer" would be
    /// the same message, and they mean opposite things.
    pub attached: bool,
    /// Some pixels changed. A commit with no damage and no attach is still a
    /// commit — it applies whatever else was pending — but it need not repaint.
    pub damaged: bool,
}

const NO_STATE: State = State { buffer: NONE, attached: false, damaged: false };

#[derive(Clone, Copy)]
pub struct Surface {
    pub used: bool,
    /// The connection this belongs to, so a client's surfaces go when it does.
    pub client: usize,
    /// The task, which is what a window is owned by.
    pub owner: usize,
    pub role: Role,
    /// The compositor's window, once there is something to show.
    pub window: usize,

    pub pending: State,
    pub current: State,

    /// Callbacks the client is waiting on, by object id.
    frame: [u32; MAX_FRAME],
    nframe: usize,

    /// The `xdg_surface` wrapping this, and its toplevel. Kept so that
    /// destroying the surface can tell the client its objects are gone.
    pub xdg_surface: u32,
    pub toplevel: u32,
    /// The configure the client must acknowledge. Zero once acknowledged.
    pub awaiting: u32,
    /// It has acknowledged at least one. Nothing is shown before this: a
    /// client that has not agreed to a size has not agreed to be a window.
    pub configured: bool,

    /// The toplevel's title.
    ///
    /// Kept here rather than pushed straight to a window, because
    /// `set_title` arrives before the first commit — there is no window to put
    /// it on yet, and a title dropped for being early is a title bar that
    /// stays blank for the life of the program.
    title: [u8; MAX_TITLE],
    title_len: usize,
}

pub const NO_SURFACE: Surface = Surface {
    used: false,
    client: 0,
    owner: 0,
    role: Role::None,
    window: NONE,
    pending: NO_STATE,
    current: NO_STATE,
    frame: [0; MAX_FRAME],
    nframe: 0,
    xdg_surface: 0,
    toplevel: 0,
    awaiting: 0,
    configured: false,
    title: [0; MAX_TITLE],
    title_len: 0,
};

static mut SURFACES: [Surface; MAX_SURFACES] = [NO_SURFACE; MAX_SURFACES];

pub fn get(idx: usize) -> Option<Surface> {
    unsafe { SURFACES.get(idx).copied().filter(|s| s.used) }
}

pub fn create(client: usize, owner: usize) -> Option<usize> {
    let idx = unsafe { SURFACES.iter().position(|s| !s.used) }?;
    unsafe {
        SURFACES[idx] = Surface { used: true, client, owner, ..NO_SURFACE };
    }
    Some(idx)
}

/// Give a surface a role. False if it already has one — which is a protocol
/// error rather than a mistake to paper over.
pub fn set_role(idx: usize, role: Role) -> bool {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return false;
        };
        if s.role != Role::None {
            return false;
        }
        s.role = role;
        true
    }
}

pub fn attach(idx: usize, buffer: usize) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.pending.buffer = buffer;
            s.pending.attached = true;
        }
    }
}

pub fn damage(idx: usize) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.pending.damaged = true;
        }
    }
}

/// Remember a callback to fire when this surface's next commit reaches the
/// screen. Returns false if the client has too many outstanding.
pub fn want_frame(idx: usize, callback: u32) -> bool {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return false;
        };
        if s.nframe >= MAX_FRAME {
            return false;
        }
        s.frame[s.nframe] = callback;
        s.nframe += 1;
        true
    }
}

/// Name a surface. Applied at once if it is already showing, and remembered
/// for the window it does not have yet if it is not.
pub fn set_title(idx: usize, title: &[u8]) {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return;
        };
        s.title_len = title.len().min(MAX_TITLE);
        s.title[..s.title_len].copy_from_slice(&title[..s.title_len]);
        let (window, len) = (s.window, s.title_len);
        let title = s.title;
        if window != NONE {
            crate::set_window_title(window, &title[..len]);
        }
    }
}

pub fn set_configure(idx: usize, serial: u32) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.awaiting = serial;
        }
    }
}

/// The client acknowledged a configure. Only the one outstanding counts: a
/// serial the compositor never sent is a client answering a question nobody
/// asked.
pub fn ack(idx: usize, serial: u32) -> bool {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return false;
        };
        if s.awaiting == 0 || s.awaiting != serial {
            return false;
        }
        s.awaiting = 0;
        s.configured = true;
        true
    }
}

/// What a commit leaves for the client to be told about.
pub struct Applied {
    /// The buffer the compositor has finished reading, if the commit replaced
    /// one. Until this is released the client must not draw into it.
    pub release: usize,
    /// Callbacks that came due, in the order they were asked for.
    pub frames: [u32; MAX_FRAME],
    pub nframes: usize,
    /// The screen needs repainting where this surface is.
    pub repaint: bool,
}

/// Apply everything pending, all at once.
///
/// The copy below is the atomicity. Everything else here is bookkeeping about
/// what to tell the client afterwards.
pub fn commit(idx: usize) -> Option<Applied> {
    let mut out = Applied { release: NONE, frames: [0; MAX_FRAME], nframes: 0, repaint: false };
    // Focus is announced after this block rather than inside it. A surface
    // learns its window here, and until it has, `by_window` cannot find it —
    // so announcing from `adopt_window`, which is where focus actually moves,
    // told the seat about a window no surface claimed yet and no client was
    // ever sent an enter.
    let mut adopted = false;
    unsafe {
        let s = SURFACES.get_mut(idx).filter(|s| s.used)?;

        // Nothing is shown before the client has agreed to a size. A commit
        // that arrives first is not an error — it is how a client asks to be
        // configured — it simply does not reach the screen.
        let showable = s.role != Role::None && s.configured;

        let previous = s.current.buffer;
        if s.pending.attached {
            s.current.buffer = s.pending.buffer;
        }
        s.current.damaged = s.pending.damaged;
        s.current.attached = s.pending.attached;
        let new_buffer = s.current.buffer;
        s.pending = NO_STATE;

        // The old buffer goes back the moment the new one takes its place. A
        // client cannot reuse memory the compositor is reading, and this is
        // the only thing that tells it the reading has stopped.
        if previous != NONE && previous != new_buffer {
            out.release = previous;
        }

        out.frames = s.frame;
        out.nframes = s.nframe;
        s.nframe = 0;

        if !showable {
            return Some(out);
        }

        let window = s.window;
        let owner = s.owner;
        out.repaint = true;

        match (new_buffer, window) {
            (NONE, w) if w != NONE => {
                // A null buffer unmaps the surface: the client is saying there
                // is nothing to show, not that it has gone.
                s.window = NONE;
                crate::destroy_window(w);
                out.repaint = false; // destroy_window repaints the whole screen
                crate::composite();
            }
            (NONE, _) => out.repaint = false,
            (b, w) => {
                let Some((px, bw, bh, stride)) = shm::pixels(b) else {
                    return Some(out);
                };
                shm::set_in_use(b, true);
                if w == NONE {
                    match crate::adopt_window(owner, px as usize, bw, bh, stride) {
                        Some(new) => {
                            s.window = new;
                            crate::set_window_title(new, &s.title[..s.title_len]);
                            out.repaint = false; // a new window repaints everything
                            adopted = true;
                        }
                        None => return Some(out),
                    }
                } else {
                    crate::set_window_buffer(w, px as usize, bw, bh, stride);
                }
            }
        }
        if previous != NONE && previous != new_buffer {
            shm::set_in_use(previous, false);
        }
    }
    if adopted {
        crate::announce_focus();
    }
    Some(out)
}

/// The surface showing in a window, if any.
///
/// Routing knows a window — that is what the compositor's focus is — and the
/// protocol needs a surface and the client that owns it. This is the hop
/// between the two, and it is a search because a window does not point back.
pub fn by_window(window: usize) -> Option<usize> {
    if window == NONE {
        return None;
    }
    (0..MAX_SURFACES).find(|&i| {
        let s = unsafe { SURFACES[i] };
        s.used && s.window == window
    })
}

/// The window this surface is showing in, if it is showing.
pub fn window_of(idx: usize) -> Option<usize> {
    get(idx).map(|s| s.window).filter(|&w| w != NONE)
}

pub fn destroy(idx: usize) {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return;
        };
        let window = s.window;
        *s = NO_SURFACE;
        // Before the window goes: `destroy_window` moves focus, and the seat
        // would otherwise be asked to announce a leave for a surface that no
        // longer exists.
        crate::seat::surface_gone(idx);
        if window != NONE {
            crate::destroy_window(window);
            crate::composite();
        }
    }
}

/// Everything a client had. A client may disconnect without destroying
/// anything, and a crash is a disconnection too.
pub fn forget_client(client: usize) {
    for i in 0..MAX_SURFACES {
        let go = unsafe { SURFACES[i].used && SURFACES[i].client == client };
        if go {
            destroy(i);
        }
    }
}
