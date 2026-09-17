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
/// What one client may have: its share of them, so that a client making
/// surfaces in a loop leaves the others theirs.
pub const MAX_PER_CLIENT: usize = MAX_SURFACES / crate::client::MAX_CLIENTS;
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
    /// The newest configure sent and not yet acknowledged. Zero once every
    /// one has been.
    pub awaiting: u32,
    /// The oldest one it may still acknowledge.
    ///
    /// A resize sends a configure per tick, and a client answers them in its
    /// own time — so by the time one ack arrives there may be three newer
    /// serials outstanding. The protocol lets a client acknowledge any
    /// configure it has been sent and not already answered, and acknowledging
    /// one supersedes every older one; a compositor that insisted on the
    /// newest would kill a client for being a frame behind. This one did.
    pub oldest: u32,
    /// It has acknowledged at least one. Nothing is shown before this: a
    /// client that has not agreed to a size has not agreed to be a window.
    pub configured: bool,
    /// The client has been told this surface is on the output.
    ///
    /// Kept so the pair stays balanced: an enter for a surface already on the
    /// output, or a leave for one that was never on it, is a client whose idea
    /// of where its windows are drifts away from the compositor's.
    pub entered: bool,

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
    oldest: 0,
    entered: false,
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
    let mine = unsafe { SURFACES.iter().filter(|s| s.used && s.client == client).count() };
    if mine >= MAX_PER_CLIENT {
        return None;
    }
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

/// Record the shell objects wrapping a surface.
///
/// The compositor needs them to say anything to a client it was not asked a
/// question by: a configure it decides to send — because a window is being
/// resized, or maximised — goes to these two ids and to nothing else. Kept
/// here rather than looked up in the client's object table because the table
/// is indexed by id, and this is the other direction.
pub fn set_xdg_surface(idx: usize, id: u32) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.xdg_surface = id;
        }
    }
}

pub fn set_toplevel(idx: usize, id: u32) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.toplevel = id;
        }
    }
}

/// A surface of this client whose visibility disagrees with what the client
/// has been told, and which way it disagrees.
///
/// One at a time, because saying so is a send and a send can fail; the caller
/// loops until there is nothing left to say.
pub fn output_change(client: usize) -> Option<(usize, bool)> {
    unsafe {
        for (i, s) in SURFACES.iter().enumerate() {
            if !s.used || s.client != client {
                continue;
            }
            let visible = s.window != NONE;
            if visible != s.entered {
                return Some((i, visible));
            }
        }
    }
    None
}

pub fn set_entered(idx: usize, yes: bool) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            s.entered = yes;
        }
    }
}

pub fn set_configure(idx: usize, serial: u32) {
    unsafe {
        if let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) {
            if s.awaiting == 0 {
                s.oldest = serial;
            }
            s.awaiting = serial;
        }
    }
}

/// The client acknowledged a configure.
///
/// Any serial from the oldest unanswered one up to the newest sent, because
/// that is the set the client has been given and not yet answered. Answering
/// one supersedes the older ones, so the window moves up rather than closing:
/// a serial from before that is one already answered, and a serial after it is
/// one nobody sent — both are a client answering a question that was not
/// asked, which is what the error says.
pub fn ack(idx: usize, serial: u32) -> bool {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return false;
        };
        if s.awaiting == 0 || serial < s.oldest || serial > s.awaiting {
            return false;
        }
        if serial == s.awaiting {
            s.awaiting = 0;
            s.oldest = 0;
        } else {
            s.oldest = serial + 1;
        }
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
            (b, w) => match shm::pixels(b) {
                None => {
                    // The client destroyed the buffer between attaching it and
                    // committing it. There is nothing to show: the surface
                    // keeps what it was showing, and nothing is released —
                    // telling the client the old buffer was free while a
                    // window still points at its pixels is how a compositor
                    // ends up compositing memory that has been unmapped.
                    s.current.buffer = previous;
                    out.release = NONE;
                    out.repaint = false;
                    return Some(out);
                }
                Some((px, bw, bh, stride)) => {
                    if w == NONE {
                        match crate::adopt_window(owner, px as usize, bw, bh, stride) {
                            Some(new) => {
                                s.window = new;
                                crate::set_window_title(new, &s.title[..s.title_len]);
                                out.repaint = false; // a new window repaints everything
                                adopted = true;
                            }
                            // No window to be had. The buffer is still what
                            // the surface shows, so it is still held.
                            None => out.repaint = false,
                        }
                    } else {
                        crate::set_window_buffer(w, px as usize, bw, bh, stride);
                    }
                }
            },
        }
        // What the surface shows changes at most once a commit, so the counts
        // move exactly once — the new buffer held before the old is let go,
        // so that a pool both are carved from is never briefly unused.
        if new_buffer != previous {
            if new_buffer != NONE {
                shm::set_in_use(new_buffer, true);
            }
            if previous != NONE {
                shm::set_in_use(previous, false);
            }
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

/// The role object has gone, but the surface has not: it stops being a window
/// and is a surface with no role again.
///
/// The slot stays taken, because the client's `wl_surface` still names it —
/// and a slot freed while an object names it is a slot the next client's
/// surface takes, with the first client still able to name it.
pub fn clear_role(idx: usize) {
    let (window, showing) = unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return;
        };
        let was = (s.window, s.current.buffer);
        s.role = Role::None;
        s.configured = false;
        s.awaiting = 0;
        s.oldest = 0;
        s.window = NONE;
        // `entered` is deliberately left alone: the surface has stopped being
        // shown but the client has not been told yet, and that disagreement is
        // exactly what makes the leave go out.
        s.current = NO_STATE;
        s.pending = NO_STATE;
        s.nframe = 0;
        s.xdg_surface = 0;
        s.toplevel = 0;
        was
    };
    if showing != NONE {
        shm::set_in_use(showing, false);
    }
    crate::seat::surface_gone(idx);
    if window != NONE {
        crate::destroy_window(window);
        crate::composite();
    }
}

pub fn destroy(idx: usize) {
    unsafe {
        let Some(s) = SURFACES.get_mut(idx).filter(|s| s.used) else {
            return;
        };
        let window = s.window;
        let showing = s.current.buffer;
        *s = NO_SURFACE;
        // It is not showing anything any more, which is what lets a buffer
        // the client destroyed while it was on screen finally go.
        if showing != NONE {
            shm::set_in_use(showing, false);
        }
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
