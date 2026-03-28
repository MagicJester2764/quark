use crate::ffi::{OsStr, OsString};
use crate::io;

pub struct Env(!);

impl Iterator for Env {
    type Item = (OsString, OsString);
    fn next(&mut self) -> Option<(OsString, OsString)> {
        self.0
    }
}

pub fn env() -> Env {
    panic!("not supported on this platform")
}

pub fn getenv(_key: &OsStr) -> Option<OsString> {
    None
}

pub unsafe fn setenv(_key: &OsStr, _val: &OsStr) -> io::Result<()> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

pub unsafe fn unsetenv(_key: &OsStr) -> io::Result<()> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}
