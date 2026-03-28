use crate::sys::map_quark_error;
use crate::{io, process, sys};

pub const STDIN_BUF_SIZE: usize = crate::sys::io::DEFAULT_BUF_SIZE;

pub struct Stdin {}

impl Stdin {
    pub const fn new() -> Self {
        Self {}
    }
}

pub struct Stdout {}

impl Stdout {
    pub const fn new() -> Self {
        Self {}
    }
}

pub struct Stderr {}

impl Stderr {
    pub const fn new() -> Self {
        Self {}
    }
}

impl io::Read for Stdin {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        quark_rt::rt::fd_read(quark_rt::rt::FD_STDIN, buf).map_err(map_quark_error)
    }
}

impl io::Write for Stdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        quark_rt::rt::fd_write(quark_rt::rt::FD_STDOUT, buf).map_err(map_quark_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Write for Stderr {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        quark_rt::rt::fd_write(quark_rt::rt::FD_STDERR, buf).map_err(map_quark_error)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub fn panic_output() -> Option<impl io::Write> {
    Some(Stderr::new())
}

pub fn is_ebadf(_err: &io::Error) -> bool {
    true
}
