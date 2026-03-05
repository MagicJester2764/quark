use core::fmt;
use crate::syscall;

struct Stdout;

impl fmt::Write for Stdout {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let buf = s.as_bytes();
        let ret = syscall::sys_fd_write(1, buf);
        if ret == u64::MAX {
            // fd 1 not connected — kernel console handles fallback
            syscall::sys_write(buf);
        }
        Ok(())
    }
}

struct Stderr;

impl fmt::Write for Stderr {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let buf = s.as_bytes();
        let ret = syscall::sys_fd_write(2, buf);
        if ret == u64::MAX {
            syscall::sys_write(buf);
        }
        Ok(())
    }
}

pub fn _print(args: fmt::Arguments) {
    use fmt::Write;
    Stdout.write_fmt(args).unwrap();
}

pub fn _eprint(args: fmt::Arguments) {
    use fmt::Write;
    Stderr.write_fmt(args).unwrap();
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
