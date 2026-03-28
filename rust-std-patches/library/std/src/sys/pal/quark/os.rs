use crate::error::Error as StdError;
use crate::ffi::{OsStr, OsString};
use crate::path::{self, PathBuf};
use crate::{fmt, io};

pub fn errno() -> i32 {
    0
}

pub fn error_string(errno: i32) -> String {
    match errno {
        0 => "success".into(),
        1 => "not found".into(),
        2 => "permission denied".into(),
        3 => "invalid argument".into(),
        4 => "already exists".into(),
        5 => "I/O error".into(),
        6 => "not a directory".into(),
        7 => "is a directory".into(),
        8 => "would block".into(),
        9 => "unsupported".into(),
        _ => format!("unknown error {errno}"),
    }
}

pub fn getcwd() -> io::Result<PathBuf> {
    Ok(PathBuf::from("/"))
}

pub fn chdir(_p: &path::Path) -> io::Result<()> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

pub struct SplitPaths<'a>(!, core::marker::PhantomData<&'a ()>);

pub fn split_paths(_unparsed: &OsStr) -> SplitPaths<'_> {
    panic!("unsupported")
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        self.0
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(_paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    Err(JoinPathsError)
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "not supported on this platform".fmt(f)
    }
}

impl StdError for JoinPathsError {
    fn description(&self) -> &str {
        "not supported on this platform"
    }
}

pub fn current_exe() -> io::Result<PathBuf> {
    Err(io::Error::UNSUPPORTED_PLATFORM)
}

pub fn home_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/"))
}

pub fn temp_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

pub fn getpid() -> u32 {
    quark_rt::syscall::sys_getpid() as u32
}
