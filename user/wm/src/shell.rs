//! The shell: how a surface becomes a window.
//!
//! A `wl_surface` on its own is a rectangle of pixels with no opinion about
//! where it goes or what it is. `xdg_shell` is what supplies the opinion —
//! that this one is a toplevel, that it should be this big, that the client
//! agreed to that size before anything was shown.
//!
//! The agreement is the part worth naming. The compositor sends a `configure`
//! carrying a serial and shows nothing until the same serial comes back in an
//! `ack_configure`. Without it a client would learn its size by having its
//! first frame clipped, and every compositor and client would disagree about
//! whose fault that was.

use crate::protocol as proto;
use crate::surface::{self, Role};

/// A toplevel this compositor has made and is waiting on.
#[derive(Clone, Copy)]
pub struct Toplevel {
    pub surface: usize,
    /// The size the compositor asked for, which is what the client will draw
    /// unless it insists otherwise.
    pub width: u32,
    pub height: u32,
}

/// Give a surface the toplevel role and decide how big it should be.
///
/// The size is a suggestion in the protocol's own terms: zero would mean "you
/// choose", and this compositor does have an opinion, so it says one.
pub fn make_toplevel(surface_idx: usize) -> Option<Toplevel> {
    if !surface::set_role(surface_idx, Role::Toplevel) {
        return None;
    }
    let (w, h) = crate::suggested_size();
    Some(Toplevel { surface: surface_idx, width: w as u32, height: h as u32 })
}

/// The serial for the configure a surface is about to be sent.
pub fn begin_configure(surface_idx: usize) -> u32 {
    let serial = proto::next_serial();
    surface::set_configure(surface_idx, serial);
    serial
}
