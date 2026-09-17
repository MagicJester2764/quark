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
/// Version 5: `wl_seat.name`, `wl_keyboard.repeat_info`, and `wl_pointer`'s
/// `frame` with the axis detail that goes with it. A version is advertised
/// only when every event of it is sent, so this moved from 4 to 5 in the same
/// change that started sending them — a client binding 5 is right to expect a
/// `frame` after every pointer event group, and gets one.
///
/// Version 6 is `wl_seat.release`'s tightened rules and version 8 is
/// `axis_value120`, which replaces `axis_discrete` for high-resolution
/// wheels. A PS/2 wheel has no fractions to report, so there is nothing 8
/// would let this compositor say that 5 does not.
pub const SEAT: Interface = Interface { name: b"wl_seat", version: 5 };
/// Server-side decorations, which is the only answer this compositor has: it
/// draws a title bar whether or not anybody asks. The value of saying so is
/// that a toolkit stops drawing its own on top of it.
pub const DECORATION: Interface =
    Interface { name: b"zxdg_decoration_manager_v1", version: 1 };
/// The clipboard. Version 1 on purpose: that is exactly the selection, and
/// versions 2 and 3 are drag and drop, which needs pointer grabs and a drag
/// icon surface and is a larger thing than copying text.
pub const DATA_DEVICE_MANAGER: Interface =
    Interface { name: b"wl_data_device_manager", version: 1 };

/// What the registry advertises, and the order it advertises them in. The
/// index is the `name` a client binds by.
pub const GLOBALS: [&Interface; 7] = [
    &COMPOSITOR,
    &SHM,
    &OUTPUT,
    &XDG_WM_BASE,
    &SEAT,
    &DECORATION,
    &DATA_DEVICE_MANAGER,
];

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

/// `wl_surface.error`: a scale or a transform that is not one.
pub const SURFACE_ERR_INVALID_SCALE: u32 = 0;
pub const SURFACE_ERR_INVALID_TRANSFORM: u32 = 1;

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

// wl_seat requests and events.
pub const SEAT_GET_POINTER: u16 = 0;
pub const SEAT_GET_KEYBOARD: u16 = 1;
pub const SEAT_GET_TOUCH: u16 = 2;
pub const SEAT_RELEASE: u16 = 3;
pub const SEAT_CAPABILITIES: u16 = 0;
pub const SEAT_NAME: u16 = 1;
/// `wl_seat.capability` bits. A client reads these to decide what to ask for,
/// so advertising one means answering the request that follows.
pub const SEAT_CAP_POINTER: u32 = 1;
pub const SEAT_CAP_KEYBOARD: u32 = 2;

// wl_pointer requests and events.
pub const POINTER_SET_CURSOR: u16 = 0;
pub const POINTER_RELEASE: u16 = 1;
pub const POINTER_ENTER: u16 = 0;
pub const POINTER_LEAVE: u16 = 1;
pub const POINTER_MOTION: u16 = 2;
pub const POINTER_BUTTON: u16 = 3;
pub const POINTER_AXIS: u16 = 4;
/// The end of a group. Everything since the last one is one logical event —
/// an enter and the motion that came with it, a wheel click and the axis
/// detail describing it — and a client applies the group at once.
pub const POINTER_FRAME: u16 = 5;
pub const POINTER_AXIS_SOURCE: u16 = 6;
pub const POINTER_AXIS_STOP: u16 = 7;
pub const POINTER_AXIS_DISCRETE: u16 = 8;
/// The version that added the four events above. An object below it hears none
/// of them, which is why every send checks: a `frame` to a client that bound 4
/// is an opcode its libwayland has no listener slot for.
pub const POINTER_FRAME_SINCE: u32 = 5;

/// `wl_pointer.axis`. Horizontal exists for completeness; a PS/2 wheel has one
/// axis and a tilt this driver does not report.
pub const AXIS_VERTICAL_SCROLL: u32 = 0;
pub const AXIS_HORIZONTAL_SCROLL: u32 = 1;
/// `wl_pointer.axis_source`. A wheel is the discrete one: it moves in clicks,
/// which is what `axis_discrete` counts, and unlike a touchpad it has no end
/// to a gesture — which is why `axis_stop` is defined here and never sent.
pub const AXIS_SOURCE_WHEEL: u32 = 0;
pub const AXIS_SOURCE_FINGER: u32 = 1;
pub const AXIS_SOURCE_CONTINUOUS: u32 = 2;
/// What one click of the wheel is worth in surface coordinates. Ten, which is
/// what Weston sends, so a client tuned against a Linux compositor scrolls by
/// the same amount here.
pub const AXIS_STEP: i32 = 10;
/// `wl_pointer.button_state`.
pub const BUTTON_RELEASED: u32 = 0;
pub const BUTTON_PRESSED: u32 = 1;
/// Linux evdev button codes, which is what `wl_pointer.button` carries — the
/// same numbering as the key codes, from the same header.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

// wl_keyboard requests and events.
pub const KEYBOARD_RELEASE: u16 = 0;
pub const KEYBOARD_KEYMAP: u16 = 0;
pub const KEYBOARD_ENTER: u16 = 1;
pub const KEYBOARD_LEAVE: u16 = 2;
pub const KEYBOARD_KEY: u16 = 3;
pub const KEYBOARD_MODIFIERS: u16 = 4;
pub const KEYBOARD_REPEAT_INFO: u16 = 5;
/// `wl_keyboard.keymap_format`. `NO_KEYMAP` means the client uses a layout of
/// its own, which is the honest thing to say until the compositor has one.
pub const KEYMAP_FORMAT_NO_KEYMAP: u32 = 0;
pub const KEYMAP_FORMAT_XKB_V1: u32 = 1;
/// `wl_keyboard.key_state`.
pub const KEY_RELEASED: u32 = 0;
pub const KEY_PRESSED: u32 = 1;

// zxdg_decoration_manager_v1 requests.
pub const DECORATION_DESTROY: u16 = 0;
pub const DECORATION_GET_TOPLEVEL: u16 = 1;

// zxdg_toplevel_decoration_v1 requests and events.
pub const TOPLEVEL_DECORATION_DESTROY: u16 = 0;
pub const TOPLEVEL_DECORATION_SET_MODE: u16 = 1;
pub const TOPLEVEL_DECORATION_UNSET_MODE: u16 = 2;
pub const TOPLEVEL_DECORATION_CONFIGURE: u16 = 0;
/// `zxdg_toplevel_decoration_v1.mode`. A client may ask for either and is told
/// which it gets; this compositor always answers `SERVER_SIDE`, because the
/// frame is drawn before the client's pixels are and there is no way for it to
/// not be drawn.
pub const DECORATION_MODE_CLIENT_SIDE: u32 = 1;
pub const DECORATION_MODE_SERVER_SIDE: u32 = 2;

// wl_data_device_manager requests.
pub const DDM_CREATE_DATA_SOURCE: u16 = 0;
pub const DDM_GET_DATA_DEVICE: u16 = 1;

// wl_data_source requests and events.
pub const DATA_SOURCE_OFFER: u16 = 0;
pub const DATA_SOURCE_DESTROY: u16 = 1;
pub const DATA_SOURCE_TARGET: u16 = 0;
pub const DATA_SOURCE_SEND: u16 = 1;
pub const DATA_SOURCE_CANCELLED: u16 = 2;

// wl_data_device requests and events.
pub const DATA_DEVICE_START_DRAG: u16 = 0;
pub const DATA_DEVICE_SET_SELECTION: u16 = 1;
pub const DATA_DEVICE_RELEASE: u16 = 2;
pub const DATA_DEVICE_DATA_OFFER: u16 = 0;
pub const DATA_DEVICE_SELECTION: u16 = 5;

// wl_data_offer requests and events.
pub const DATA_OFFER_ACCEPT: u16 = 0;
pub const DATA_OFFER_RECEIVE: u16 = 1;
pub const DATA_OFFER_DESTROY: u16 = 2;
pub const DATA_OFFER_OFFER: u16 = 0;

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
