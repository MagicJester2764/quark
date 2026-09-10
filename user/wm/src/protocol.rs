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

// wl_shm requests and events.
pub const SHM_CREATE_POOL: u16 = 0;
pub const SHM_FORMAT: u16 = 0;

// wl_shm_pool requests.
pub const SHM_POOL_CREATE_BUFFER: u16 = 0;
pub const SHM_POOL_DESTROY: u16 = 1;
pub const SHM_POOL_RESIZE: u16 = 2;

// wl_buffer requests and events.
pub const BUFFER_DESTROY: u16 = 0;
pub const BUFFER_RELEASE: u16 = 0;

/// `wl_display.error` codes. `INVALID_METHOD` is what a request the compositor
/// cannot honour earns: telling the client which object and why is the
/// difference between a bug it can find and a connection that simply stopped.
pub const ERR_INVALID_OBJECT: u32 = 0;
pub const ERR_INVALID_METHOD: u32 = 1;
pub const ERR_NO_MEMORY: u32 = 2;

/// `wl_shm.error`: the client asked for a format nothing here can composite.
pub const SHM_ERR_INVALID_FORMAT: u32 = 0;
pub const SHM_ERR_INVALID_STRIDE: u32 = 1;
pub const SHM_ERR_INVALID_FD: u32 = 2;

// wl_compositor requests.
pub const COMPOSITOR_CREATE_SURFACE: u16 = 0;
pub const COMPOSITOR_CREATE_REGION: u16 = 1;

// wl_surface requests. Everything between `attach` and `commit` accumulates:
// none of them changes what is on screen, which is what makes `commit` atomic.
pub const SURFACE_DESTROY: u16 = 0;
pub const SURFACE_ATTACH: u16 = 1;
pub const SURFACE_DAMAGE: u16 = 2;
pub const SURFACE_FRAME: u16 = 3;
pub const SURFACE_SET_OPAQUE_REGION: u16 = 4;
pub const SURFACE_SET_INPUT_REGION: u16 = 5;
pub const SURFACE_COMMIT: u16 = 6;
pub const SURFACE_SET_BUFFER_TRANSFORM: u16 = 7;
pub const SURFACE_SET_BUFFER_SCALE: u16 = 8;
pub const SURFACE_DAMAGE_BUFFER: u16 = 9;
pub const SURFACE_OFFSET: u16 = 10;
// wl_surface events.
pub const SURFACE_ENTER: u16 = 0;

// wl_region requests. A region is an optimisation hint about which pixels are
// opaque or want input; this compositor composites and routes the same either
// way, so the object exists and its requests do nothing.
pub const REGION_DESTROY: u16 = 0;

// wl_output events. Advertised at version 2, which is the version that has
// `done` — a client binding an output waits for it before believing anything.
pub const OUTPUT_GEOMETRY: u16 = 0;
pub const OUTPUT_MODE: u16 = 1;
pub const OUTPUT_DONE: u16 = 2;
pub const OUTPUT_SCALE: u16 = 3;
/// `wl_output.mode` flags: this is the current mode, and the only one.
pub const OUTPUT_MODE_CURRENT: u32 = 1;
pub const OUTPUT_MODE_PREFERRED: u32 = 2;

// xdg_wm_base requests and events.
pub const WM_BASE_DESTROY: u16 = 0;
pub const WM_BASE_CREATE_POSITIONER: u16 = 1;
pub const WM_BASE_GET_XDG_SURFACE: u16 = 2;
pub const WM_BASE_PONG: u16 = 3;

// xdg_surface requests and events.
pub const XDG_SURFACE_DESTROY: u16 = 0;
pub const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
pub const XDG_SURFACE_GET_POPUP: u16 = 2;
pub const XDG_SURFACE_SET_GEOMETRY: u16 = 3;
pub const XDG_SURFACE_ACK_CONFIGURE: u16 = 4;
pub const XDG_SURFACE_CONFIGURE: u16 = 0;

// xdg_toplevel requests and events.
pub const TOPLEVEL_DESTROY: u16 = 0;
pub const TOPLEVEL_SET_TITLE: u16 = 2;
pub const TOPLEVEL_CONFIGURE: u16 = 0;
pub const TOPLEVEL_CLOSE: u16 = 1;

/// `xdg_wm_base.error`: a surface was given a second role, or shown before it
/// acknowledged the configure that told it how big to be.
pub const XDG_ERR_ROLE: u32 = 0;
pub const XDG_ERR_UNCONFIGURED_BUFFER: u32 = 3;

/// The object id of `wl_display`, which exists before anything is asked for.
pub const DISPLAY_ID: u32 = 1;

/// The next serial.
///
/// A serial is how a client says *which* event it is answering. The compositor
/// sends a configure carrying one and will not show the surface until the same
/// number comes back, which is what makes "the client agreed to this size"
/// something that can be checked rather than assumed.
pub fn next_serial() -> u32 {
    static mut SERIAL: u32 = 0;
    unsafe {
        SERIAL = SERIAL.wrapping_add(1);
        SERIAL
    }
}
