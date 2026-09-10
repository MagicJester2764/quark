//! The interfaces this compositor implements.
//!
//! Written out rather than generated. The MVP needs a dozen of them, and a
//! generator is a bigger thing than the list it would produce; it becomes worth
//! building at the point where this list changes more often than the code that
//! reads it, and not before.
//!
//! Versions are what the compositor *advertises*. A client binds no higher than
//! the minimum of what it supports and what it is offered, so starting low is
//! safe and raising one is a decision to be made when a client asks.

pub struct Interface {
    pub name: &'static [u8],
    pub version: u32,
}

pub const COMPOSITOR: Interface = Interface { name: b"wl_compositor", version: 1 };
pub const SHM: Interface = Interface { name: b"wl_shm", version: 1 };
pub const OUTPUT: Interface = Interface { name: b"wl_output", version: 2 };
pub const XDG_WM_BASE: Interface = Interface { name: b"xdg_wm_base", version: 1 };

/// What the registry advertises, and the order it advertises them in. The
/// index is the `name` a client binds by.
pub const GLOBALS: [&Interface; 4] = [&COMPOSITOR, &SHM, &OUTPUT, &XDG_WM_BASE];

// wl_display requests.
pub const DISPLAY_SYNC: u16 = 0;
pub const DISPLAY_GET_REGISTRY: u16 = 1;
// wl_display events.
pub const DISPLAY_ERROR: u16 = 0;
pub const DISPLAY_DELETE_ID: u16 = 1;

// wl_registry requests and events.
pub const REGISTRY_BIND: u16 = 0;
pub const REGISTRY_GLOBAL: u16 = 0;

// wl_callback events.
pub const CALLBACK_DONE: u16 = 0;

/// The object id of `wl_display`, which exists before anything is asked for.
pub const DISPLAY_ID: u32 = 1;
