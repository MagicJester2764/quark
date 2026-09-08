use core::fmt;
use crate::syscall;

const BUF_SIZE: usize = 256;

struct BufWriter {
    buf: [u8; BUF_SIZE],
    pos: usize,
}

impl BufWriter {
    const fn new() -> Self {
        BufWriter { buf: [0; BUF_SIZE], pos: 0 }
    }

    fn flush(&mut self, fd: usize) {
        if self.pos == 0 {
            return;
        }
        let data = &self.buf[..self.pos];
        let ret = syscall::sys_fd_write(fd, data);
        if ret == u64::MAX {
            syscall::sys_write(data);
        }
        self.pos = 0;
    }
}

impl fmt::Write for BufWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &b in s.as_bytes() {
            if self.pos >= BUF_SIZE {
                // Buffer full — shouldn't happen for typical prints,
                // but avoid overflow silently
                break;
            }
            self.buf[self.pos] = b;
            self.pos += 1;
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    let mut w = BufWriter::new();
    let _ = w.write_fmt(args);
    w.flush(1);
}

pub fn _eprint(args: fmt::Arguments) {
    use fmt::Write;
    let mut w = BufWriter::new();
    let _ = w.write_fmt(args);
    w.flush(2);
}

/// Read a line from stdin (fd 0) into `buf`. Returns the number of bytes read.
/// Blocks until a line is available. Returns 0 if stdin is not connected.
pub fn read_line(buf: &mut [u8]) -> usize {
    read_line_result(buf).unwrap_or(0)
}

/// Read a line, distinguishing "nothing was typed" from "there is nowhere to
/// read from".
///
/// [`read_line`] answers 0 to both, which is fine for a program that will ask
/// again and wrong for one that will ask again *immediately*: a shell with no
/// descriptor on stdin spins printing prompts at a pipe nobody is reading.
/// `Err` means the descriptor is not connected, which is not a pause — it is
/// the end.
pub fn read_line_result(buf: &mut [u8]) -> Result<usize, ()> {
    let ret = syscall::sys_fd_read(0, buf);
    if ret == u64::MAX { Err(()) } else { Ok(ret as usize) }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {
        $crate::stdio::_print(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($fmt:expr $(, $($arg:tt)*)?) => {
        $crate::stdio::_print(format_args!(concat!($fmt, "\n") $(, $($arg)*)?))
    };
}

#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::stdio::_eprint(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! eprintln {
    () => { $crate::eprint!("\n") };
    ($fmt:expr $(, $($arg:tt)*)?) => {
        $crate::stdio::_eprint(format_args!(concat!($fmt, "\n") $(, $($arg)*)?))
    };
}
