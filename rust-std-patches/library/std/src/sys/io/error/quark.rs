use crate::io;

pub const INVALID_ARGUMENT: i32 = quark_rt::rt::E_INVALID;
pub const NOT_FOUND: i32 = quark_rt::rt::E_NOT_FOUND;
pub const PERMISSION_DENIED: i32 = quark_rt::rt::E_PERMISSION;
pub const ALREADY_EXISTS: i32 = quark_rt::rt::E_EXISTS;
pub const WOULD_BLOCK: i32 = quark_rt::rt::E_WOULD_BLOCK;
pub const UNSUPPORTED: i32 = quark_rt::rt::E_UNSUPPORTED;

pub fn decode_error_kind(errno: i32) -> io::ErrorKind {
    match errno {
        quark_rt::rt::E_NOT_FOUND => io::ErrorKind::NotFound,
        quark_rt::rt::E_PERMISSION => io::ErrorKind::PermissionDenied,
        quark_rt::rt::E_INVALID => io::ErrorKind::InvalidInput,
        quark_rt::rt::E_EXISTS => io::ErrorKind::AlreadyExists,
        quark_rt::rt::E_IO => io::ErrorKind::Other,
        quark_rt::rt::E_NOT_DIR => io::ErrorKind::NotADirectory,
        quark_rt::rt::E_IS_DIR => io::ErrorKind::IsADirectory,
        quark_rt::rt::E_WOULD_BLOCK => io::ErrorKind::WouldBlock,
        quark_rt::rt::E_UNSUPPORTED => io::ErrorKind::Unsupported,
        _ => io::ErrorKind::Other,
    }
}
