use crate::ffi::OsString;
use crate::fmt;
use crate::vec;

pub struct Args {
    iter: vec::IntoIter<OsString>,
}

pub fn args() -> Args {
    let count = quark_rt::rt::argc();
    let mut vec = Vec::with_capacity(count);
    for i in 0..count {
        if let Some(bytes) = quark_rt::rt::argv(i) {
            vec.push(OsString::from(core::str::from_utf8(bytes).unwrap_or("")));
        }
    }
    Args { iter: vec.into_iter() }
}

impl fmt::Debug for Args {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.iter.as_slice().fmt(f)
    }
}

impl Iterator for Args {
    type Item = OsString;
    fn next(&mut self) -> Option<OsString> {
        self.iter.next()
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.iter.size_hint()
    }
}

impl ExactSizeIterator for Args {
    fn len(&self) -> usize {
        self.iter.len()
    }
}

impl DoubleEndedIterator for Args {
    fn next_back(&mut self) -> Option<OsString> {
        self.iter.next_back()
    }
}
