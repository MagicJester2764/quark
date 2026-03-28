use crate::io;

pub struct Pipe(!);

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

impl Pipe {
    pub fn read(&self, _buf: &mut [u8]) -> io::Result<usize> {
        self.0
    }

    pub fn read_buf(&self, _buf: crate::io::BorrowedCursor<'_>) -> io::Result<()> {
        self.0
    }

    pub fn read_to_end(&self, _buf: &mut Vec<u8>) -> io::Result<usize> {
        self.0
    }

    pub fn write(&self, _buf: &[u8]) -> io::Result<usize> {
        self.0
    }
}
