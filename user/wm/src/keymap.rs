//! The keyboard layout, said once and shared.
//!
//! A `NO_KEYMAP` seat leaves every client to guess what the key codes mean, and
//! the guesses do not agree — a toolkit assumes US QWERTY, a terminal assumes
//! whatever the last compositor told it. `XKB_V1` is the format every client
//! already parses, and saying it once is the difference between a keyboard and
//! a stream of numbers.
//!
//! The text is generated rather than written, because hand-written XKB is how
//! you get a keymap that xkbcommon rejects at run time, a long way from here:
//!
//! ```text
//! printf 'xkb_keymap {
//!     xkb_keycodes  { include "evdev" };
//!     xkb_types     { include "complete" };
//!     xkb_compat    { include "basic+iso9995" };
//!     xkb_symbols   { include "pc+us+inet(evdev)" };
//! };\n' > us.in
//! xkbcomp -xkb -o user/wm/src/us.xkb us.in
//! ```
//!
//! `xkb_geometry` is left out: it describes the physical shape of the keys for
//! drawing a picture of a keyboard, and no client here draws one.
//!
//! The keycodes line up because they are the same numbers all the way down.
//! A PS/2 set-1 scancode is the Linux evdev keycode for everything in the
//! non-extended set, and an XKB keycode is that plus eight — which is why this
//! keymap resolves evdev 30 to `AC01`, the key with `a` on it.

use quark_rt::syscall;

/// Where the text is written while the region is being filled. Mapped, copied
/// into, and unmapped again — the descriptor is what is kept.
const SCRATCH: usize = 0x89_0000_0000;

static TEXT: &[u8] = include_bytes!("us.xkb");

/// The descriptor naming the filled region, or `usize::MAX` before the first
/// client asks for a keyboard.
static mut FD: usize = usize::MAX;

/// How many bytes a client should map, which includes the terminating NUL.
///
/// xkbcommon reads the mapping as a C string, so the NUL is part of the data
/// and not an implementation detail of how it got there.
pub fn size() -> u32 {
    TEXT.len() as u32 + 1
}

/// Build the region now, so that failing to costs a line on a console rather
/// than a client's first roundtrip. Returns whether there is a keymap to send.
pub fn prepare() -> bool {
    unsafe {
        if FD == usize::MAX {
            FD = match build() {
                Some(fd) => fd,
                None => return false,
            };
        }
        true
    }
}

/// A descriptor for the keymap, made on first use and duplicated after.
///
/// Duplicated rather than shared, because a client closes the descriptor it is
/// given — that is what the protocol tells it to do — and the next client still
/// needs one.
pub fn descriptor() -> Option<usize> {
    unsafe {
        if FD == usize::MAX {
            FD = build()?;
        }
        syscall::sys_fd_dup_self(FD, 3).ok()
    }
}

/// Whether there is a keymap to describe, which decides what format a client
/// is told. Saying `XKB_V1` and then sending nothing is worse than admitting
/// there is no keymap: the client parses the empty mapping and gets an error
/// where it could have had a default.
pub fn ready() -> bool {
    unsafe { FD != usize::MAX }
}

fn build() -> Option<usize> {
    let bytes = TEXT.len() + 1;
    let pages = bytes.div_ceil(4096);
    let fd = syscall::sys_memfd_create(pages).ok()?;
    if syscall::sys_mmap_fd(fd, SCRATCH).is_err() {
        let _ = syscall::sys_fd_close(fd);
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(TEXT.as_ptr(), SCRATCH as *mut u8, TEXT.len());
        core::ptr::write_volatile((SCRATCH + TEXT.len()) as *mut u8, 0);
    }
    // Unmapped here: the compositor never reads it back, and leaving forty
    // kilobytes mapped for the life of the session to no purpose is the kind
    // of thing that is only ever noticed much later.
    let _ = syscall::sys_munmap(SCRATCH, pages);
    Some(fd)
}
