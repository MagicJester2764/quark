//! Wayland, as this system speaks it.
//!
//! The client half is upstream libwayland and needs nothing from here. What
//! lives here is the part both halves have to agree about — the wire format —
//! written once rather than once per program, the same way the VFS protocol is.

pub mod wire;
